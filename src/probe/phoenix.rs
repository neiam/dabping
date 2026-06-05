//! Phoenix-socket probe: RTT = a `heartbeat` on the reserved `phoenix`
//! topic, which every Phoenix socket answers without joining a channel
//! (LiveView's CSRF check happens at join, not connect). One WebSocket
//! connection per round, N heartbeats over it — so ping 1 includes
//! TCP+TLS+upgrade and the rest measure pure channel round-trips; the
//! smoke shows both.

use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::{RoundResult, pace, resolve};
use crate::config::PhoenixConfig;

pub struct PhoenixProbe {
    path: String,
    tls: bool,
    cookie: Option<String>,
    timeout: Duration,
    interval: Duration,
}

impl PhoenixProbe {
    pub fn new(cfg: &PhoenixConfig) -> Result<PhoenixProbe> {
        if !cfg.path.starts_with('/') {
            bail!("phoenix probe path must start with '/' (got {:?})", cfg.path);
        }
        Ok(PhoenixProbe {
            path: cfg.path.clone(),
            tls: cfg.tls,
            cookie: cfg.cookie.clone(),
            timeout: cfg.timeout(),
            interval: cfg.interval(),
        })
    }

    fn url(&self, host: &str, port_override: Option<u16>) -> String {
        let scheme = if self.tls { "wss" } else { "ws" };
        let port = match port_override {
            Some(p) => format!(":{p}"),
            None => String::new(),
        };
        let sep = if self.path.contains('?') { '&' } else { '?' };
        format!("{scheme}://{host}{port}{}{sep}vsn=2.0.0", self.path)
    }

    pub async fn round(
        &self,
        target_path: &str,
        host: &str,
        port_override: Option<u16>,
        pings: u32,
    ) -> Result<RoundResult> {
        let addr = resolve(host).await?;
        let started = SystemTime::now();
        let empty = |rtts: Vec<Duration>| RoundResult {
            target: target_path.to_string(),
            host: host.to_string(),
            addr,
            started,
            sent: pings,
            rtts,
        };

        let mut request = self
            .url(host, port_override)
            .into_client_request()
            .context("invalid phoenix socket url")?;
        if let Some(cookie) = &self.cookie {
            request.headers_mut().insert(
                "Cookie",
                cookie.parse().context("phoenix probe cookie is not a valid header value")?,
            );
        }

        // an unreachable/refusing/auth-gated socket is a measurement
        // (total loss), not a probe error
        let mut ws = match tokio::time::timeout(
            self.timeout,
            tokio_tungstenite::connect_async(request),
        )
        .await
        {
            Ok(Ok((ws, _resp))) => ws,
            Ok(Err(e)) => {
                tracing::debug!(path = %target_path, %host, error = %e, "phoenix connect failed");
                return Ok(empty(Vec::new()));
            }
            Err(_) => return Ok(empty(Vec::new())),
        };

        let mut rtts = Vec::with_capacity(pings as usize);
        'round: for seq in 0..pings {
            let r = (seq + 1).to_string();
            // v2 wire format: [join_ref, ref, topic, event, payload]
            let beat = serde_json::json!([(), r, "phoenix", "heartbeat", {}]).to_string();
            let t0 = Instant::now();
            if ws.send(Message::Text(beat.into())).await.is_err() {
                break 'round; // connection died: remaining pings are lost
            }
            let deadline = t0 + self.timeout;
            loop {
                let now = Instant::now();
                if now >= deadline {
                    break; // this heartbeat is a loss
                }
                match tokio::time::timeout(deadline - now, ws.next()).await {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        if is_reply(&text, &r) {
                            rtts.push(t0.elapsed());
                            break;
                        } // else: other traffic on the socket — keep waiting
                    }
                    Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => break 'round,
                    Ok(Some(Ok(_))) => continue, // ping/pong/binary
                    Err(_) => break,             // timeout
                }
            }
            pace(t0, self.interval, seq + 1 == pings).await;
        }
        let _ = ws.close(None).await;

        Ok(empty(rtts))
    }
}

/// True if `text` is the phx_reply for our heartbeat ref.
fn is_reply(text: &str, want_ref: &str) -> bool {
    let Ok(serde_json::Value::Array(m)) = serde_json::from_str(text) else {
        return false;
    };
    m.get(1).and_then(|v| v.as_str()) == Some(want_ref)
        && m.get(3).and_then(|v| v.as_str()) == Some("phx_reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};

    fn cfg(path: &str, tls: bool) -> PhoenixConfig {
        PhoenixConfig {
            path: path.into(),
            tls,
            cookie: None,
            timeout_ms: 1000,
            interval_ms: 10,
        }
    }

    #[test]
    fn validates_and_builds_urls() {
        assert!(PhoenixProbe::new(&cfg("nope", true)).is_err());
        let p = PhoenixProbe::new(&cfg("/live/websocket", true)).unwrap();
        assert_eq!(p.url("x.org", None), "wss://x.org/live/websocket?vsn=2.0.0");
        assert_eq!(p.url("x.org", Some(4000)), "wss://x.org:4000/live/websocket?vsn=2.0.0");
        let p = PhoenixProbe::new(&cfg("/socket/websocket?token=t", false)).unwrap();
        assert_eq!(p.url("x", None), "ws://x/socket/websocket?token=t&vsn=2.0.0");
    }

    #[test]
    fn matches_replies() {
        assert!(is_reply(r#"[null,"3","phoenix","phx_reply",{"status":"ok"}]"#, "3"));
        assert!(!is_reply(r#"[null,"4","phoenix","phx_reply",{}]"#, "3"));
        assert!(!is_reply(r#"[null,"3","room:1","new_msg",{}]"#, "3"));
        assert!(!is_reply("not json", "3"));
    }

    /// Mock Phoenix endpoint: answers heartbeats, ignores everything else.
    async fn mock_phoenix(port_holder: &mut u16) {
        async fn sock(mut ws: WebSocket) {
            while let Some(Ok(m)) = ws.recv().await {
                if let AxMsg::Text(t) = m {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    if v[3] == "heartbeat" {
                        let reply = serde_json::json!([(), v[1], "phoenix", "phx_reply", {"status": "ok", "response": {}}]);
                        let _ = ws.send(AxMsg::Text(reply.to_string().into())).await;
                    }
                }
            }
        }
        let app = axum::Router::new().route(
            "/live/websocket",
            axum::routing::get(|u: WebSocketUpgrade| async move { u.on_upgrade(sock) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        *port_holder = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    }

    #[tokio::test]
    async fn round_against_mock_phoenix() {
        let mut port = 0;
        mock_phoenix(&mut port).await;
        let probe = PhoenixProbe::new(&cfg("/live/websocket", false)).unwrap();
        let r = probe.round("t", "127.0.0.1", Some(port), 5).await.unwrap();
        assert_eq!(r.sent, 5);
        assert_eq!(r.received(), 5);
        assert_eq!(r.loss_pct(), 0.0);
        assert!(r.median().unwrap() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn refused_connection_is_total_loss() {
        // grab a port and close it again
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let probe = PhoenixProbe::new(&cfg("/live/websocket", false)).unwrap();
        let r = probe.round("t", "127.0.0.1", Some(port), 3).await.unwrap();
        assert_eq!(r.received(), 0);
        assert_eq!(r.loss_pct(), 100.0);
    }
}
