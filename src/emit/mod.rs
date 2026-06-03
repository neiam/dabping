pub mod graphite;
pub mod influx;
pub mod prometheus;

use std::time::Duration;

use crate::probe::RoundResult;

/// Sink for completed rounds. Sync for now; revisit (async, batching)
/// when the TSDB emitters land in milestone 7.
pub trait Emitter: Send + Sync {
    fn emit(&self, round: &RoundResult);
}

pub struct LogEmitter;

impl Emitter for LogEmitter {
    fn emit(&self, r: &RoundResult) {
        tracing::info!(
            path = %r.target,
            host = %r.host,
            addr = %r.addr,
            sent = r.sent,
            recv = r.received(),
            loss = format_args!("{:.1}%", r.loss_pct()),
            min = %fmt_opt(r.min()),
            med = %fmt_opt(r.median()),
            avg = %fmt_opt(r.avg()),
            max = %fmt_opt(r.max()),
            "round"
        );
    }
}

pub fn fmt_opt(d: Option<Duration>) -> String {
    match d {
        Some(d) => fmt_rtt(d),
        None => "-".into(),
    }
}

pub fn fmt_rtt(d: Duration) -> String {
    format!("{:.2}ms", d.as_secs_f64() * 1000.0)
}
