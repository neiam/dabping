//! Alert engine: pattern matching over recent rounds, edge-triggered
//! notifications with optional repeat, dispatched to log/exec/webhook/email.

pub mod notify;
pub mod pattern;

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{AlertConfig, AlertKind, Config, SmtpConfig};
use crate::emit::Emitter;
use crate::probe::RoundResult;
use crate::util::parse_range;
use notify::NotifySpec;
use pattern::Pattern;

/// One parsed alert definition.
struct Alert {
    kind: AlertKind,
    pattern: Pattern,
    comment: String,
    to: Vec<NotifySpec>,
    repeat: Option<Duration>,
    notify_clear: bool,
}

impl Alert {
    fn parse(name: &str, cfg: &AlertConfig) -> Result<Alert> {
        let pattern = Pattern::parse(&cfg.pattern).with_context(|| format!("alert {name:?}"))?;
        if cfg.to.is_empty() {
            bail!("alert {name:?}: `to` must list at least one notification target");
        }
        let to = cfg
            .to
            .iter()
            .map(|s| NotifySpec::parse(s))
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("alert {name:?}"))?;
        let repeat = cfg
            .repeat_every
            .as_deref()
            .map(|s| parse_range(s).map(Duration::from_secs))
            .transpose()
            .with_context(|| format!("alert {name:?}: repeat_every"))?;
        Ok(Alert {
            kind: cfg.kind,
            pattern,
            comment: cfg.comment.clone(),
            to,
            repeat,
            notify_clear: cfg.notify_clear,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AlertEvent {
    Raised,
    Repeat,
    Cleared,
}

impl std::fmt::Display for AlertEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AlertEvent::Raised => "raised",
            AlertEvent::Repeat => "still-raised",
            AlertEvent::Cleared => "cleared",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub alert: String,
    pub target: String,
    pub host: String,
    pub state: AlertEvent,
    pub comment: String,
    /// Recent sample values, oldest→newest, for context.
    pub detail: String,
}

#[derive(Clone, Copy)]
struct Sample {
    loss: f64,
    median_ms: Option<f64>,
}

#[derive(Default)]
struct AlertState {
    raised: bool,
    last_notify: Option<Instant>,
}

pub struct Alerter {
    alerts: HashMap<String, Alert>,
    /// target path → alert names that watch it
    watches: HashMap<String, Vec<String>>,
    history_depth: usize,
    smtp: Option<SmtpConfig>,
    history: Mutex<HashMap<String, VecDeque<Sample>>>,
    state: Mutex<HashMap<(String, String), AlertState>>,
}

impl Alerter {
    /// Returns None when no target references an alert.
    pub fn new(cfg: &Config) -> Result<Option<Alerter>> {
        let mut alerts = HashMap::new();
        for (name, ac) in &cfg.alerts {
            alerts.insert(name.clone(), Alert::parse(name, ac)?);
        }
        let mut watches: HashMap<String, Vec<String>> = HashMap::new();
        for t in cfg.flatten_targets()? {
            for name in &t.alerts {
                if !alerts.contains_key(name) {
                    bail!("target {}: unknown alert {name:?}", t.path);
                }
                watches.entry(t.path.clone()).or_default().push(name.clone());
            }
        }
        if watches.is_empty() {
            return Ok(None);
        }
        if alerts
            .values()
            .any(|a| a.to.iter().any(|s| matches!(s, NotifySpec::Email(_))))
            && cfg.smtp.is_none()
        {
            bail!("an alert uses email: but there is no [smtp] section");
        }
        let history_depth = alerts.values().map(|a| a.pattern.depth()).max().unwrap_or(1).max(8);
        Ok(Some(Alerter {
            alerts,
            watches,
            history_depth,
            smtp: cfg.smtp.clone(),
            history: Mutex::new(HashMap::new()),
            state: Mutex::new(HashMap::new()),
        }))
    }

    /// Pure-ish core: record the round, evaluate the target's alerts, return
    /// the notifications to send. Split from emit() for testability.
    fn process_round(&self, r: &RoundResult) -> Vec<Notification> {
        let Some(alert_names) = self.watches.get(&r.target) else {
            return Vec::new();
        };

        let samples: Vec<Sample> = {
            let mut hist = self.history.lock().expect("history lock poisoned");
            let h = hist.entry(r.target.clone()).or_default();
            h.push_back(Sample {
                loss: r.loss_pct(),
                median_ms: r.median().map(|d| d.as_secs_f64() * 1000.0),
            });
            while h.len() > self.history_depth {
                h.pop_front();
            }
            h.iter().copied().collect()
        };

        let mut out = Vec::new();
        let mut state = self.state.lock().expect("state lock poisoned");
        for name in alert_names {
            let alert = &self.alerts[name];
            let series: Vec<Option<f64>> = samples
                .iter()
                .map(|s| match alert.kind {
                    AlertKind::Loss => Some(s.loss),
                    AlertKind::Rtt => s.median_ms,
                })
                .collect();
            let matched = alert.pattern.matches(&series);

            let st = state.entry((name.clone(), r.target.clone())).or_default();
            let event = match (st.raised, matched) {
                (false, true) => Some(AlertEvent::Raised),
                (true, true) => match alert.repeat {
                    Some(every)
                        if st.last_notify.is_none_or(|t| t.elapsed() >= every) =>
                    {
                        Some(AlertEvent::Repeat)
                    }
                    _ => None,
                },
                (true, false) if alert.notify_clear => Some(AlertEvent::Cleared),
                (true, false) => None,
                (false, false) => None,
            };
            st.raised = matched;
            if let Some(state_ev) = event {
                st.last_notify = Some(Instant::now());
                out.push(Notification {
                    alert: name.clone(),
                    target: r.target.clone(),
                    host: r.host.clone(),
                    state: state_ev,
                    comment: alert.comment.clone(),
                    detail: detail(alert.kind, &samples),
                });
            }
        }
        out
    }
}

fn detail(kind: AlertKind, samples: &[Sample]) -> String {
    let vals: Vec<String> = samples
        .iter()
        .map(|s| match kind {
            AlertKind::Loss => format!("{:.0}", s.loss),
            AlertKind::Rtt => s.median_ms.map_or("U".into(), |m| format!("{m:.1}")),
        })
        .collect();
    let label = match kind {
        AlertKind::Loss => "loss%",
        AlertKind::Rtt => "median ms",
    };
    format!("{label} (oldest→newest): {}", vals.join(" "))
}

impl Emitter for Alerter {
    fn emit(&self, r: &RoundResult) {
        for n in self.process_round(r) {
            let specs = self.alerts[&n.alert].to.clone();
            let smtp = self.smtp.clone();
            tokio::spawn(async move {
                for spec in &specs {
                    if let Err(e) = notify::dispatch(&n, spec, &smtp).await {
                        tracing::error!(alert = %n.alert, target = %n.target, error = %e, "notification failed");
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration as StdDuration, SystemTime};

    fn test_alerter(pattern: &str, kind: &str, extra: &str) -> Alerter {
        let cfg: Config = toml::from_str(&format!(
            r#"
            [alerts.a1]
            type = "{kind}"
            pattern = "{pattern}"
            comment = "test alert"
            to = ["log:"]
            {extra}

            [targets.t]
              [targets.t.x]
              host = "192.0.2.1"
              alerts = ["a1"]
            "#
        ))
        .unwrap();
        // mimic Config::load's implicit probe default
        let mut cfg = cfg;
        cfg.probes.insert("icmp".into(), crate::config::ProbeConfig::Icmp(Default::default()));
        Alerter::new(&cfg).unwrap().expect("alerter should exist")
    }

    fn round(loss_of_4: u32) -> RoundResult {
        let rtts = (0..(4 - loss_of_4))
            .map(|_| StdDuration::from_millis(10))
            .collect();
        RoundResult {
            target: "t/x".into(),
            host: "192.0.2.1".into(),
            addr: "192.0.2.1".parse().unwrap(),
            started: SystemTime::UNIX_EPOCH,
            sent: 4,
            rtts,
        }
    }

    #[test]
    fn edge_trigger_raise_once_then_clear() {
        let a = test_alerter(">10%,>10%", "loss", "");
        assert!(a.process_round(&round(0)).is_empty()); // healthy
        assert!(a.process_round(&round(2)).is_empty()); // one bad round: no match yet
        let n = a.process_round(&round(2)); // two consecutive: raise
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].state, AlertEvent::Raised);
        assert!(a.process_round(&round(2)).is_empty()); // still bad: edge-triggered, silent
        let n = a.process_round(&round(0)); // recovered
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].state, AlertEvent::Cleared);
        assert!(a.process_round(&round(0)).is_empty());
    }

    #[test]
    fn no_clear_when_disabled() {
        let a = test_alerter(">10%", "loss", "notify_clear = false");
        assert_eq!(a.process_round(&round(2))[0].state, AlertEvent::Raised);
        assert!(a.process_round(&round(0)).is_empty()); // clear suppressed
        // and it can raise again afterwards
        assert_eq!(a.process_round(&round(2))[0].state, AlertEvent::Raised);
    }

    #[test]
    fn repeat_fires_after_interval() {
        let a = test_alerter(">10%", "loss", r#"repeat_every = "1s""#);
        assert_eq!(a.process_round(&round(2))[0].state, AlertEvent::Raised);
        assert!(a.process_round(&round(2)).is_empty()); // too soon
        std::thread::sleep(StdDuration::from_millis(1100));
        assert_eq!(a.process_round(&round(2))[0].state, AlertEvent::Repeat);
    }

    #[test]
    fn rtt_alerts_see_unknown_on_total_loss() {
        let a = test_alerter("==U,==U", "rtt", "");
        assert!(a.process_round(&round(4)).is_empty());
        let n = a.process_round(&round(4));
        assert_eq!(n.len(), 1);
        assert!(n[0].detail.contains("U U"));
    }

    #[test]
    fn rtt_threshold() {
        let a = test_alerter(">5", "rtt", "");
        // 10ms median > 5ms
        assert_eq!(a.process_round(&round(0))[0].state, AlertEvent::Raised);
    }
}
