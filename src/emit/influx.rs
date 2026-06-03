//! InfluxDB line-protocol emitter. Batches lines and POSTs them to the
//! configured write endpoint (v1 `/write?db=` or v2 `/api/v2/write?…`)
//! every `flush_secs` or 1000 lines, whichever comes first.

use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::mpsc;

use super::Emitter;
use crate::config::InfluxConfig;
use crate::probe::RoundResult;

const MAX_BATCH: usize = 1000;

pub struct InfluxEmitter {
    measurement: String,
    tx: mpsc::Sender<String>,
}

impl InfluxEmitter {
    pub fn spawn(cfg: &InfluxConfig) -> InfluxEmitter {
        let (tx, rx) = mpsc::channel(4096);
        tokio::spawn(sender(cfg.clone(), rx));
        InfluxEmitter { measurement: cfg.measurement.clone(), tx }
    }
}

impl Emitter for InfluxEmitter {
    fn emit(&self, r: &RoundResult) {
        if self.tx.try_send(format_line(&self.measurement, r)).is_err() {
            tracing::warn!(target: "dabping::emit", path = %r.target, "influx queue full; dropping round");
        }
    }
}

async fn sender(cfg: InfluxConfig, mut rx: mpsc::Receiver<String>) {
    let client = reqwest::Client::new();
    let mut batch: Vec<String> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.flush_secs.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(l) => {
                    batch.push(l);
                    if batch.len() >= MAX_BATCH {
                        flush(&client, &cfg, &mut batch).await;
                    }
                }
                None => {
                    flush(&client, &cfg, &mut batch).await;
                    return;
                }
            },
            _ = tick.tick() => flush(&client, &cfg, &mut batch).await,
        }
    }
}

async fn flush(client: &reqwest::Client, cfg: &InfluxConfig, batch: &mut Vec<String>) {
    if batch.is_empty() {
        return;
    }
    let body = batch.join("");
    let n = batch.len();
    batch.clear(); // on failure the batch is lost — measurement won't backlog
    let mut req = client.post(&cfg.url).body(body);
    if let Some(token) = &cfg.token {
        req = req.header("Authorization", format!("Token {token}"));
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => tracing::warn!(url = %cfg.url, status = %resp.status(), dropped = n, "influx write rejected"),
        Err(e) => tracing::warn!(url = %cfg.url, error = %e, dropped = n, "influx unreachable"),
    }
}

/// Tag-value escaping per the line protocol: `,`, `=`, ` ` get backslashed.
fn esc_tag(s: &str) -> String {
    s.replace(',', "\\,").replace('=', "\\=").replace(' ', "\\ ")
}

fn format_line(measurement: &str, r: &RoundResult) -> String {
    let ns = r
        .started
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or_default();
    let mut fields = format!("loss={:.3},sent={}i,recv={}i", r.loss_pct(), r.sent, r.received());
    for (name, v) in [("median", r.median()), ("min", r.min()), ("max", r.max())] {
        if let Some(v) = v {
            fields.push_str(&format!(",{name}={:.6}", v.as_secs_f64()));
        }
    }
    format!(
        "{},target={},host={} {} {}\n",
        esc_tag(measurement),
        esc_tag(&r.target),
        esc_tag(&r.host),
        fields,
        ns
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn round() -> RoundResult {
        RoundResult {
            target: "net/cf".into(),
            host: "1.1.1.1".into(),
            addr: "1.1.1.1".parse().unwrap(),
            started: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            sent: 4,
            rtts: vec![Duration::from_millis(10), Duration::from_millis(20)],
        }
    }

    #[test]
    fn formats_line_protocol() {
        let line = format_line("dabping", &round());
        assert_eq!(
            line,
            "dabping,target=net/cf,host=1.1.1.1 loss=50.000,sent=4i,recv=2i,median=0.015000,min=0.010000,max=0.020000 1000000000000\n"
        );
    }

    #[test]
    fn escapes_tag_values() {
        assert_eq!(esc_tag("a b,c=d"), "a\\ b\\,c\\=d");
    }

    #[tokio::test]
    async fn posts_batches_with_token() {
        use std::sync::{Arc, Mutex};
        let got: Arc<Mutex<Vec<(Option<String>, String)>>> = Arc::default();
        let got2 = got.clone();
        let app = axum::Router::new().route(
            "/api/v2/write",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                got2.lock().unwrap().push((auth, body));
                async { axum::http::StatusCode::NO_CONTENT }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/api/v2/write?org=o&bucket=b",
            listener.local_addr().unwrap().port()
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = InfluxConfig {
            url,
            token: Some("t0k3n".into()),
            measurement: "dabping".into(),
            flush_secs: 60, // rely on channel close to flush
        };
        let emitter = InfluxEmitter::spawn(&cfg);
        emitter.emit(&round());
        emitter.emit(&round());
        drop(emitter);
        tokio::time::sleep(Duration::from_millis(200)).await;

        let got = got.lock().unwrap();
        assert_eq!(got.len(), 1, "two lines, one batch");
        assert_eq!(got[0].0.as_deref(), Some("Token t0k3n"));
        assert_eq!(got[0].1.lines().count(), 2);
        assert!(got[0].1.starts_with("dabping,target=net/cf"));
    }
}
