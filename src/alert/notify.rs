//! Notification dispatch: log, exec, webhook, email.

use anyhow::{Context, Result, bail};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::Notification;
use crate::config::SmtpConfig;

#[derive(Debug, Clone, PartialEq)]
pub enum NotifySpec {
    /// `log:` — a WARN line; the do-nothing default and handy in tests.
    Log,
    /// `email:addr@example.com` — needs the [smtp] section.
    Email(String),
    /// `exec:/path/to/script` — argv: alert target host state comment.
    Exec(String),
    /// `webhook:https://…` — POST a JSON body.
    Webhook(String),
}

impl NotifySpec {
    pub fn parse(s: &str) -> Result<NotifySpec> {
        let (kind, rest) = s.split_once(':').unwrap_or((s, ""));
        let rest = rest.trim();
        Ok(match kind {
            "log" => NotifySpec::Log,
            "email" if !rest.is_empty() => NotifySpec::Email(rest.into()),
            "exec" if !rest.is_empty() => NotifySpec::Exec(rest.into()),
            "webhook" if rest.starts_with("http://") || rest.starts_with("https://") => {
                NotifySpec::Webhook(rest.into())
            }
            _ => bail!("bad notification target {s:?} (log: | email:addr | exec:cmd | webhook:url)"),
        })
    }
}

pub async fn dispatch(n: &Notification, spec: &NotifySpec, smtp: &Option<SmtpConfig>) -> Result<()> {
    match spec {
        NotifySpec::Log => {
            tracing::warn!(
                alert = %n.alert,
                target = %n.target,
                state = %n.state,
                detail = %n.detail,
                "ALERT {} {} on {} ({}): {}",
                n.state, n.alert, n.target, n.host, n.comment
            );
            Ok(())
        }
        NotifySpec::Exec(cmd) => {
            let status = tokio::process::Command::new(cmd)
                .args([&n.alert, &n.target, &n.host, &n.state.to_string(), &n.comment])
                .env("DABPING_DETAIL", &n.detail)
                .status()
                .await
                .with_context(|| format!("exec notifier {cmd:?} failed to start"))?;
            if !status.success() {
                bail!("exec notifier {cmd:?} exited with {status}");
            }
            Ok(())
        }
        NotifySpec::Webhook(url) => {
            let resp = reqwest::Client::new()
                .post(url)
                .json(&serde_json::json!({
                    "alert": n.alert,
                    "target": n.target,
                    "host": n.host,
                    "state": n.state.to_string(),
                    "comment": n.comment,
                    "detail": n.detail,
                }))
                .send()
                .await
                .with_context(|| format!("webhook {url} unreachable"))?;
            if !resp.status().is_success() {
                bail!("webhook {url} answered {}", resp.status());
            }
            Ok(())
        }
        NotifySpec::Email(addr) => {
            let smtp = smtp.as_ref().context("email notifier needs an [smtp] section")?;
            let msg = Message::builder()
                .from(smtp.from.parse().context("bad smtp.from address")?)
                .to(addr.parse().with_context(|| format!("bad email address {addr:?}"))?)
                .subject(format!("[dabping] {} {}: {}", n.state, n.alert, n.target))
                .header(ContentType::TEXT_PLAIN)
                .body(format!(
                    "{} {} on {} ({})\n\n{}\n\n{}\n",
                    n.state, n.alert, n.target, n.host, n.comment, n.detail
                ))?;
            let mut builder = match smtp.tls.as_str() {
                "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&smtp.server),
                "starttls" => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.server)?,
                "implicit" => AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.server)?,
                other => bail!("smtp.tls must be none|starttls|implicit (got {other:?})"),
            }
            .port(smtp.port);
            if let (Some(u), Some(p)) = (&smtp.username, &smtp.password) {
                builder = builder.credentials(Credentials::new(u.clone(), p.clone()));
            }
            builder.build().send(msg).await.context("smtp send failed")?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::AlertEvent;

    fn notification() -> Notification {
        Notification {
            alert: "bigloss".into(),
            target: "isp/gw".into(),
            host: "192.0.2.1".into(),
            state: AlertEvent::Raised,
            comment: "lots of loss".into(),
            detail: "loss%: 0 30 60".into(),
        }
    }

    #[test]
    fn parses_specs() {
        assert_eq!(NotifySpec::parse("log:").unwrap(), NotifySpec::Log);
        assert_eq!(NotifySpec::parse("log").unwrap(), NotifySpec::Log);
        assert!(matches!(NotifySpec::parse("email:a@b.c").unwrap(), NotifySpec::Email(_)));
        assert!(matches!(NotifySpec::parse("exec:/bin/true").unwrap(), NotifySpec::Exec(_)));
        assert!(matches!(NotifySpec::parse("webhook:https://x/y").unwrap(), NotifySpec::Webhook(_)));
        assert!(NotifySpec::parse("email:").is_err());
        assert!(NotifySpec::parse("webhook:ftp://x").is_err());
        assert!(NotifySpec::parse("page-me").is_err());
    }

    #[tokio::test]
    async fn exec_notifier_passes_args() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("notify.sh");
        let out = dir.path().join("out.txt");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$1|$2|$4|$DABPING_DETAIL\" > {}\n", out.display())).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        dispatch(&notification(), &NotifySpec::Exec(script.display().to_string()), &None)
            .await
            .unwrap();
        let written = std::fs::read_to_string(&out).unwrap();
        assert_eq!(written.trim(), "bigloss|isp/gw|raised|loss%: 0 30 60");
    }

    #[tokio::test]
    async fn webhook_notifier_posts_json() {
        use std::sync::{Arc, Mutex};
        let got: Arc<Mutex<Option<serde_json::Value>>> = Arc::default();
        let got2 = got.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
                *got2.lock().unwrap() = Some(v);
                async { "ok" }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}/hook", listener.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        dispatch(&notification(), &NotifySpec::Webhook(url), &None).await.unwrap();
        let v = got.lock().unwrap().take().unwrap();
        assert_eq!(v["alert"], "bigloss");
        assert_eq!(v["state"], "raised");
        assert_eq!(v["target"], "isp/gw");
    }

    #[tokio::test]
    async fn email_without_smtp_errors() {
        let err = dispatch(&notification(), &NotifySpec::Email("a@b.c".into()), &None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("[smtp]"));
    }
}
