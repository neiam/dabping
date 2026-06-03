//! Graphite plaintext emitter: `prefix.path.metric value ts\n` over TCP.
//! A background task owns the connection (reconnects lazily); rounds are
//! fanned in over a bounded channel and dropped with a warning if the
//! sink can't keep up — measurement must never block on export.

use std::time::UNIX_EPOCH;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::Emitter;
use crate::config::GraphiteConfig;
use crate::probe::RoundResult;

pub struct GraphiteEmitter {
    prefix: String,
    tx: mpsc::Sender<String>,
}

impl GraphiteEmitter {
    pub fn spawn(cfg: &GraphiteConfig) -> GraphiteEmitter {
        let (tx, rx) = mpsc::channel(4096);
        tokio::spawn(sender(cfg.addr.clone(), rx));
        GraphiteEmitter { prefix: cfg.prefix.clone(), tx }
    }
}

impl Emitter for GraphiteEmitter {
    fn emit(&self, r: &RoundResult) {
        let payload = format_lines(&self.prefix, r);
        if self.tx.try_send(payload).is_err() {
            tracing::warn!(target: "dabping::emit", path = %r.target, "graphite queue full; dropping round");
        }
    }
}

async fn sender(addr: String, mut rx: mpsc::Receiver<String>) {
    let mut stream: Option<TcpStream> = None;
    while let Some(payload) = rx.recv().await {
        // one reconnect attempt per payload; failures drop the payload
        for _ in 0..2 {
            if stream.is_none() {
                match TcpStream::connect(&addr).await {
                    Ok(s) => stream = Some(s),
                    Err(e) => {
                        tracing::warn!(%addr, error = %e, "graphite unreachable; dropping round");
                        break;
                    }
                }
            }
            match stream.as_mut().expect("stream just set").write_all(payload.as_bytes()).await {
                Ok(()) => break,
                Err(e) => {
                    tracing::warn!(%addr, error = %e, "graphite write failed; reconnecting");
                    stream = None;
                }
            }
        }
    }
}

/// `target/path` → `prefix.target.path` with hostile chars flattened.
fn metric_path(prefix: &str, target: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + target.len() + 1);
    out.push_str(prefix);
    for seg in target.split('/') {
        out.push('.');
        out.extend(seg.chars().map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }
        }));
    }
    out
}

/// loss as percent; rtts in seconds (rrd/smokeping convention).
fn format_lines(prefix: &str, r: &RoundResult) -> String {
    let ts = r.started.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let base = metric_path(prefix, &r.target);
    let mut out = format!("{base}.loss {:.3} {ts}\n", r.loss_pct());
    for (name, v) in [("median", r.median()), ("min", r.min()), ("max", r.max())] {
        if let Some(v) = v {
            out.push_str(&format!("{base}.{name} {:.6} {ts}\n", v.as_secs_f64()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use tokio::io::AsyncReadExt;

    fn round() -> RoundResult {
        RoundResult {
            target: "net/cf.x".into(),
            host: "1.1.1.1".into(),
            addr: "1.1.1.1".parse().unwrap(),
            started: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            sent: 4,
            rtts: vec![
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(30),
            ],
        }
    }

    #[test]
    fn formats_and_sanitizes() {
        let text = format_lines("dabping", &round());
        assert!(text.contains("dabping.net.cf_x.loss 25.000 1000\n"));
        assert!(text.contains("dabping.net.cf_x.median 0.020000 1000\n"));
        assert!(text.contains("dabping.net.cf_x.min 0.010000 1000\n"));
        assert!(text.contains("dabping.net.cf_x.max 0.030000 1000\n"));
    }

    #[tokio::test]
    async fn sends_over_tcp() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = String::new();
            sock.read_to_string(&mut buf).await.unwrap();
            buf
        });

        let cfg = GraphiteConfig { addr, prefix: "dabping".into() };
        let emitter = GraphiteEmitter::spawn(&cfg);
        emitter.emit(&round());
        drop(emitter); // closes the channel → sender task finishes → FIN
        let got = server.await.unwrap();
        assert!(got.contains("dabping.net.cf_x.loss 25.000 1000"));
    }
}
