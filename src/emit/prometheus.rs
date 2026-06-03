//! Prometheus exposition: an in-memory registry of each target's latest
//! round plus cumulative counters, rendered as text 0.0.4 at GET /metrics.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use super::Emitter;
use crate::probe::RoundResult;

#[derive(Default)]
pub struct PromState {
    inner: Mutex<BTreeMap<String, Entry>>,
}

struct Entry {
    host: String,
    ts: i64,
    loss_ratio: f64,
    median: Option<f64>,
    min: Option<f64>,
    max: Option<f64>,
    sent_total: u64,
    recv_total: u64,
}

impl PromState {
    pub fn observe(&self, r: &RoundResult) {
        let mut m = self.inner.lock().expect("prom lock poisoned");
        let e = m.entry(r.target.clone()).or_insert_with(|| Entry {
            host: r.host.clone(),
            ts: 0,
            loss_ratio: 0.0,
            median: None,
            min: None,
            max: None,
            sent_total: 0,
            recv_total: 0,
        });
        e.host = r.host.clone();
        e.ts = r
            .started
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();
        e.loss_ratio = r.loss_pct() / 100.0;
        e.median = r.median().map(|d| d.as_secs_f64());
        e.min = r.min().map(|d| d.as_secs_f64());
        e.max = r.max().map(|d| d.as_secs_f64());
        e.sent_total += r.sent as u64;
        e.recv_total += r.received() as u64;
    }

    pub fn render(&self) -> String {
        let m = self.inner.lock().expect("prom lock poisoned");
        let mut out = String::with_capacity(256 + m.len() * 512);
        let gauge = |out: &mut String, name: &str, help: &str| {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
        };
        let counter = |out: &mut String, name: &str, help: &str| {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
        };
        let labels = |t: &str, h: &str| format!("target=\"{}\",host=\"{}\"", esc(t), esc(h));

        gauge(&mut out, "dabping_loss_ratio", "Packet loss of the last round (0-1).");
        for (t, e) in m.iter() {
            out.push_str(&format!("dabping_loss_ratio{{{}}} {}\n", labels(t, &e.host), e.loss_ratio));
        }
        for (name, get, help) in [
            ("dabping_median_seconds", 0, "Median RTT of the last round."),
            ("dabping_min_seconds", 1, "Fastest RTT of the last round."),
            ("dabping_max_seconds", 2, "Slowest RTT of the last round."),
        ] {
            gauge(&mut out, name, help);
            for (t, e) in m.iter() {
                let v = match get {
                    0 => e.median,
                    1 => e.min,
                    _ => e.max,
                };
                if let Some(v) = v {
                    out.push_str(&format!("{name}{{{}}} {v}\n", labels(t, &e.host)));
                }
            }
        }
        counter(&mut out, "dabping_pings_sent_total", "Pings sent.");
        for (t, e) in m.iter() {
            out.push_str(&format!("dabping_pings_sent_total{{{}}} {}\n", labels(t, &e.host), e.sent_total));
        }
        counter(&mut out, "dabping_pings_received_total", "Replies received.");
        for (t, e) in m.iter() {
            out.push_str(&format!("dabping_pings_received_total{{{}}} {}\n", labels(t, &e.host), e.recv_total));
        }
        gauge(&mut out, "dabping_last_round_timestamp_seconds", "When the last round started.");
        for (t, e) in m.iter() {
            out.push_str(&format!(
                "dabping_last_round_timestamp_seconds{{{}}} {}\n",
                labels(t, &e.host),
                e.ts
            ));
        }
        out
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

pub struct PromEmitter(pub Arc<PromState>);

impl Emitter for PromEmitter {
    fn emit(&self, r: &RoundResult) {
        self.0.observe(r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn round(rtts_ms: &[u64], sent: u32) -> RoundResult {
        RoundResult {
            target: "net/cf".into(),
            host: "1.1.1.1".into(),
            addr: "1.1.1.1".parse().unwrap(),
            started: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
            sent,
            rtts: rtts_ms.iter().map(|ms| Duration::from_millis(*ms)).collect(),
        }
    }

    #[test]
    fn renders_gauges_and_accumulates_counters() {
        let st = PromState::default();
        st.observe(&round(&[10, 20, 30, 40], 4));
        st.observe(&round(&[10, 20], 4)); // 50% loss
        let text = st.render();
        assert!(text.contains("dabping_loss_ratio{target=\"net/cf\",host=\"1.1.1.1\"} 0.5"));
        assert!(text.contains("dabping_median_seconds{target=\"net/cf\",host=\"1.1.1.1\"} 0.015"));
        assert!(text.contains("dabping_pings_sent_total{target=\"net/cf\",host=\"1.1.1.1\"} 8"));
        assert!(text.contains("dabping_pings_received_total{target=\"net/cf\",host=\"1.1.1.1\"} 6"));
        assert!(text.contains("# TYPE dabping_loss_ratio gauge"));
    }

    #[test]
    fn total_loss_omits_rtt_gauges() {
        let st = PromState::default();
        st.observe(&round(&[], 4));
        let text = st.render();
        assert!(text.contains("dabping_loss_ratio{target=\"net/cf\",host=\"1.1.1.1\"} 1"));
        assert!(!text.contains("dabping_median_seconds{target"));
    }
}
