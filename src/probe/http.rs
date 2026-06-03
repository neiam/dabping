//! HTTP(S) probe: RTT = full request → body downloaded. SmokePing's
//! EchoPingHttp/Curl equivalent. Any HTTP response counts as a reply —
//! a 500 still proves the path to the server; loss means no response.

use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};
use reqwest::Method;

use super::{RoundResult, pace, resolve};
use crate::config::HttpConfig;

pub struct HttpProbe {
    client: reqwest::Client,
    url: String,
    method: Method,
    interval: std::time::Duration,
}

impl HttpProbe {
    pub fn new(cfg: &HttpConfig) -> Result<HttpProbe> {
        let method = match cfg.method.to_ascii_uppercase().as_str() {
            "GET" => Method::GET,
            "HEAD" => Method::HEAD,
            other => bail!("unsupported http method {other:?} (GET|HEAD)"),
        };
        if !cfg.url.starts_with("http://") && !cfg.url.starts_with("https://") {
            bail!("http probe url must start with http:// or https:// (got {:?})", cfg.url);
        }
        let client = reqwest::Client::builder()
            .timeout(cfg.timeout())
            .user_agent(concat!("dabping/", env!("CARGO_PKG_VERSION")))
            // no connection reuse: every ping pays connect+TLS, so the
            // distribution reflects the full path, not a warm socket
            .pool_max_idle_per_host(0)
            .build()
            .context("cannot build http client")?;
        Ok(HttpProbe { client, url: cfg.url.clone(), method, interval: cfg.interval() })
    }

    pub async fn round(&self, target_path: &str, host: &str, pings: u32) -> Result<RoundResult> {
        let url = self.url.replace("%host%", host);
        // resolution shown in the UI; reqwest resolves again itself
        let addr = resolve(host).await.unwrap_or(std::net::Ipv4Addr::UNSPECIFIED.into());
        let started = SystemTime::now();

        let mut rtts = Vec::with_capacity(pings as usize);
        for i in 0..pings {
            let t0 = Instant::now();
            let done = async {
                let resp = self.client.request(self.method.clone(), &url).send().await?;
                resp.bytes().await?; // time the whole transfer
                Ok::<_, reqwest::Error>(())
            };
            if done.await.is_ok() {
                rtts.push(t0.elapsed());
            }
            pace(t0, self.interval, i + 1 == pings).await;
        }

        Ok(RoundResult {
            target: target_path.to_string(),
            host: host.to_string(),
            addr,
            started,
            sent: pings,
            rtts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HttpConfig;

    fn cfg(url: &str, method: &str) -> HttpConfig {
        HttpConfig {
            url: url.into(),
            method: method.into(),
            timeout_ms: 1000,
            interval_ms: 10,
        }
    }

    #[test]
    fn validates_config() {
        assert!(HttpProbe::new(&cfg("https://%host%/", "get")).is_ok());
        assert!(HttpProbe::new(&cfg("ftp://x/", "get")).is_err());
        assert!(HttpProbe::new(&cfg("https://x/", "POST")).is_err());
    }

    #[tokio::test]
    async fn round_against_local_server() {
        let app = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let probe = HttpProbe::new(&cfg(&format!("http://%host%:{port}/"), "get")).unwrap();
        let r = probe.round("t", "127.0.0.1", 4).await.unwrap();
        assert_eq!(r.received(), 4);
        assert!(r.median().unwrap().as_secs_f64() < 1.0);
    }

    #[tokio::test]
    async fn unreachable_is_loss_not_error() {
        // RFC 5737 TEST-NET, nothing listens; 200ms timeout
        let probe = HttpProbe::new(&HttpConfig {
            url: "http://%host%/".into(),
            method: "get".into(),
            timeout_ms: 200,
            interval_ms: 5,
        })
        .unwrap();
        let r = probe.round("t", "192.0.2.1", 2).await.unwrap();
        assert_eq!(r.received(), 0);
        assert_eq!(r.loss_pct(), 100.0);
    }
}
