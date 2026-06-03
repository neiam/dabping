//! Statuspage-style component status: targets map to named components,
//! severities derive from the same pattern DSL the alerts use, incidents
//! auto-open/resolve and persist as a JSONL event log.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::alert::pattern::Pattern;
use crate::config::Config;
use crate::emit::Emitter;
use crate::probe::RoundResult;
use crate::store::{Cf, Store};

/// Component severity, Statuspage vocabulary. Ordered by badness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Operational,
    Degraded,
    PartialOutage,
    MajorOutage,
}

impl Severity {
    pub fn as_statuspage(self) -> &'static str {
        match self {
            Severity::Operational => "operational",
            Severity::Degraded => "degraded_performance",
            Severity::PartialOutage => "partial_outage",
            Severity::MajorOutage => "major_outage",
        }
    }

    /// Statuspage page-level indicator.
    pub fn indicator(self) -> &'static str {
        match self {
            Severity::Operational => "none",
            Severity::Degraded => "minor",
            Severity::PartialOutage => "major",
            Severity::MajorOutage => "critical",
        }
    }

    pub fn human(self) -> &'static str {
        match self {
            Severity::Operational => "Operational",
            Severity::Degraded => "Degraded Performance",
            Severity::PartialOutage => "Partial Outage",
            Severity::MajorOutage => "Major Outage",
        }
    }

    pub fn banner(self) -> &'static str {
        match self {
            Severity::Operational => "All Systems Operational",
            Severity::Degraded => "Degraded Performance",
            Severity::PartialOutage => "Partial System Outage",
            Severity::MajorOutage => "Major System Outage",
        }
    }

    fn parse(s: &str) -> Result<Severity> {
        Ok(match s {
            "degraded" => Severity::Degraded,
            "partial_outage" => Severity::PartialOutage,
            "major_outage" => Severity::MajorOutage,
            other => bail!("bad severity {other:?} (degraded|partial_outage|major_outage)"),
        })
    }
}

pub struct Component {
    pub key: String,
    pub name: String,
    pub group: String,
    pub targets: Vec<String>,
    degraded_loss: Pattern,
    down_loss: Pattern,
    degraded_rtt: Option<Pattern>,
    incident_at: Severity,
}

const DEFAULT_DEGRADED_LOSS: &str = ">5%,>5%";
const DEFAULT_DOWN_LOSS: &str = "==100%,==100%,==100%";

/// Parse + validate the [status.components] table. Also used by
/// Config::validate so bad component configs fail check-config.
pub fn parse_components(cfg: &Config) -> Result<Vec<Component>> {
    let Some(status) = &cfg.status else { return Ok(Vec::new()) };
    let known: Vec<String> = cfg.flatten_targets()?.into_iter().map(|t| t.path).collect();
    let mut out = Vec::new();
    for (key, c) in &status.components {
        let ctx = |what: &str| format!("status component {key:?}: {what}");
        if c.targets.is_empty() {
            bail!(ctx("needs at least one target"));
        }
        for t in &c.targets {
            if !known.contains(t) {
                bail!(ctx(&format!("unknown target {t:?}")));
            }
        }
        out.push(Component {
            key: key.clone(),
            name: c.name.clone().unwrap_or_else(|| key.clone()),
            group: c.group.clone().unwrap_or_default(),
            targets: c.targets.clone(),
            degraded_loss: Pattern::parse(
                c.degraded_loss.as_deref().unwrap_or(DEFAULT_DEGRADED_LOSS),
            )
            .with_context(|| ctx("degraded_loss"))?,
            down_loss: Pattern::parse(c.down_loss.as_deref().unwrap_or(DEFAULT_DOWN_LOSS))
                .with_context(|| ctx("down_loss"))?,
            degraded_rtt: c
                .degraded_rtt
                .as_deref()
                .map(Pattern::parse)
                .transpose()
                .with_context(|| ctx("degraded_rtt"))?,
            incident_at: c
                .incident_at
                .as_deref()
                .map(Severity::parse)
                .transpose()
                .with_context(|| ctx("incident_at"))?
                .unwrap_or(Severity::PartialOutage),
        });
    }
    Ok(out)
}

// ---- incidents ----

#[derive(Debug, Clone, Serialize)]
pub struct Incident {
    pub id: u64,
    pub component: String,
    pub title: String,
    pub opened: i64,
    pub resolved: Option<i64>,
    pub updates: Vec<IncidentUpdate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IncidentUpdate {
    pub ts: i64,
    pub message: String,
}

/// Append-only event log, one JSON object per line; state is a replay.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "lowercase")]
enum Event {
    Open { id: u64, component: String, title: String, ts: i64 },
    Update { id: u64, ts: i64, message: String },
    Resolve { id: u64, ts: i64 },
}

struct IncidentLog {
    path: PathBuf,
    incidents: Vec<Incident>,
    next_id: u64,
}

impl IncidentLog {
    fn load(path: PathBuf) -> Result<IncidentLog> {
        let mut log = IncidentLog { path, incidents: Vec::new(), next_id: 1 };
        if log.path.exists() {
            let raw = std::fs::read_to_string(&log.path)
                .with_context(|| format!("cannot read {}", log.path.display()))?;
            for (no, line) in raw.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let ev: Event = serde_json::from_str(line)
                    .with_context(|| format!("{}:{}", log.path.display(), no + 1))?;
                log.apply(&ev);
            }
        }
        Ok(log)
    }

    fn apply(&mut self, ev: &Event) {
        match ev {
            Event::Open { id, component, title, ts } => {
                self.incidents.push(Incident {
                    id: *id,
                    component: component.clone(),
                    title: title.clone(),
                    opened: *ts,
                    resolved: None,
                    updates: Vec::new(),
                });
                self.next_id = self.next_id.max(id + 1);
            }
            Event::Update { id, ts, message } => {
                if let Some(i) = self.incidents.iter_mut().find(|i| i.id == *id) {
                    i.updates.push(IncidentUpdate { ts: *ts, message: message.clone() });
                }
            }
            Event::Resolve { id, ts } => {
                if let Some(i) = self.incidents.iter_mut().find(|i| i.id == *id) {
                    i.resolved = Some(*ts);
                }
            }
        }
    }

    fn record(&mut self, ev: Event) -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("cannot open {}", self.path.display()))?;
        writeln!(f, "{}", serde_json::to_string(&ev)?)?;
        self.apply(&ev);
        Ok(())
    }

    fn open_for(&self, component: &str) -> Option<u64> {
        self.incidents
            .iter()
            .rev()
            .find(|i| i.component == component && i.resolved.is_none())
            .map(|i| i.id)
    }
}

// ---- engine ----

#[derive(Clone, Copy)]
struct Sample {
    loss: f64,
    median_ms: Option<f64>,
}

pub struct StatusEngine {
    pub title: String,
    pub history_days: u32,
    admin_token: Option<String>,
    components: Vec<Component>,
    by_target: HashMap<String, Vec<usize>>,
    history_depth: usize,
    history: Mutex<HashMap<String, VecDeque<Sample>>>,
    target_state: Mutex<HashMap<String, Severity>>,
    comp_state: Mutex<Vec<Severity>>,
    incidents: Mutex<IncidentLog>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentSnapshot {
    pub key: String,
    pub name: String,
    pub group: String,
    #[serde(serialize_with = "ser_severity")]
    pub status: Severity,
}

fn ser_severity<S: serde::Serializer>(s: &Severity, ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(s.as_statuspage())
}

impl StatusEngine {
    /// None when no [status] section / no components.
    pub fn new(cfg: &Config) -> Result<Option<StatusEngine>> {
        let components = parse_components(cfg)?;
        if components.is_empty() {
            return Ok(None);
        }
        let status = cfg.status.as_ref().expect("components imply status section");
        let mut by_target: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, c) in components.iter().enumerate() {
            for t in &c.targets {
                by_target.entry(t.clone()).or_default().push(i);
            }
        }
        let history_depth = components
            .iter()
            .flat_map(|c| {
                [Some(&c.degraded_loss), Some(&c.down_loss), c.degraded_rtt.as_ref()]
            })
            .flatten()
            .map(Pattern::depth)
            .max()
            .unwrap_or(1)
            .max(8);
        let log_path = cfg.database.dir.join("incidents.jsonl");
        Ok(Some(StatusEngine {
            title: status.title.clone().unwrap_or_else(|| "dabping status".into()),
            history_days: status.history_days,
            admin_token: status.admin_token.clone(),
            comp_state: Mutex::new(vec![Severity::Operational; components.len()]),
            components,
            by_target,
            history_depth,
            history: Mutex::new(HashMap::new()),
            target_state: Mutex::new(HashMap::new()),
            incidents: Mutex::new(IncidentLog::load(log_path)?),
        }))
    }

    pub fn process_round(&self, r: &RoundResult) {
        let Some(comp_idxs) = self.by_target.get(&r.target) else { return };
        let ts = r
            .started
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();

        // update this target's ring and state
        let (losses, rtts): (Vec<Option<f64>>, Vec<Option<f64>>) = {
            let mut hist = self.history.lock().expect("history lock poisoned");
            let h = hist.entry(r.target.clone()).or_default();
            h.push_back(Sample {
                loss: r.loss_pct(),
                median_ms: r.median().map(|d| d.as_secs_f64() * 1000.0),
            });
            while h.len() > self.history_depth {
                h.pop_front();
            }
            (
                h.iter().map(|s| Some(s.loss)).collect(),
                h.iter().map(|s| s.median_ms).collect(),
            )
        };

        let target_sev = self.target_severity(&r.target, &losses, &rtts, comp_idxs);
        self.target_state
            .lock()
            .expect("target_state lock poisoned")
            .insert(r.target.clone(), target_sev);

        // recompute the components this target belongs to
        for &ci in comp_idxs {
            let comp = &self.components[ci];
            let new = self.component_severity(comp);
            let old = {
                let mut cs = self.comp_state.lock().expect("comp_state lock poisoned");
                std::mem::replace(&mut cs[ci], new)
            };
            if new != old {
                tracing::info!(component = %comp.key, from = old.human(), to = new.human(), "status transition");
                if let Err(e) = self.incident_transition(comp, old, new, ts) {
                    tracing::error!(component = %comp.key, error = %e, "incident log write failed");
                }
            }
        }
    }

    /// A target is down/degraded per its components' patterns. With several
    /// components watching one target, the worst verdict wins.
    fn target_severity(
        &self,
        _target: &str,
        losses: &[Option<f64>],
        rtts: &[Option<f64>],
        comp_idxs: &[usize],
    ) -> Severity {
        let mut sev = Severity::Operational;
        for &ci in comp_idxs {
            let c = &self.components[ci];
            if c.down_loss.matches(losses) {
                sev = sev.max(Severity::MajorOutage);
            } else if c.degraded_loss.matches(losses)
                || c.degraded_rtt.as_ref().is_some_and(|p| p.matches(rtts))
            {
                sev = sev.max(Severity::Degraded);
            }
        }
        sev
    }

    fn component_severity(&self, comp: &Component) -> Severity {
        let ts = self.target_state.lock().expect("target_state lock poisoned");
        let states: Vec<Severity> = comp
            .targets
            .iter()
            .map(|t| ts.get(t).copied().unwrap_or(Severity::Operational))
            .collect();
        let down = states.iter().filter(|s| **s == Severity::MajorOutage).count();
        if down == states.len() {
            Severity::MajorOutage
        } else if down > 0 {
            Severity::PartialOutage
        } else if states.iter().any(|s| *s == Severity::Degraded) {
            Severity::Degraded
        } else {
            Severity::Operational
        }
    }

    fn incident_transition(&self, comp: &Component, _old: Severity, new: Severity, ts: i64) -> Result<()> {
        let mut log = self.incidents.lock().expect("incident lock poisoned");
        let open = log.open_for(&comp.key);
        if new >= comp.incident_at {
            match open {
                None => {
                    let id = log.next_id;
                    log.record(Event::Open {
                        id,
                        component: comp.key.clone(),
                        title: format!("{}: {}", comp.name, new.human()),
                        ts,
                    })?;
                    log.record(Event::Update {
                        id,
                        ts,
                        message: format!("Detected: {}", new.human()),
                    })?;
                }
                Some(id) => log.record(Event::Update {
                    id,
                    ts,
                    message: format!("Status changed: {}", new.human()),
                })?,
            }
        } else if let Some(id) = open {
            log.record(Event::Update { id, ts, message: format!("Status changed: {}", new.human()) })?;
            if new == Severity::Operational {
                log.record(Event::Resolve { id, ts })?;
            }
        }
        Ok(())
    }

    // ---- views ----

    pub fn snapshot(&self) -> (Severity, Vec<ComponentSnapshot>) {
        let cs = self.comp_state.lock().expect("comp_state lock poisoned");
        let comps: Vec<ComponentSnapshot> = self
            .components
            .iter()
            .zip(cs.iter())
            .map(|(c, s)| ComponentSnapshot {
                key: c.key.clone(),
                name: c.name.clone(),
                group: c.group.clone(),
                status: *s,
            })
            .collect();
        let overall = cs.iter().copied().max().unwrap_or(Severity::Operational);
        (overall, comps)
    }

    pub fn incidents(&self) -> Vec<Incident> {
        self.incidents.lock().expect("incident lock poisoned").incidents.clone()
    }

    pub fn components(&self) -> &[Component] {
        &self.components
    }

    fn check_token(&self, token: Option<&str>) -> Result<()> {
        let Some(expected) = &self.admin_token else {
            bail!("incident administration is disabled (no status.admin_token configured)");
        };
        if token != Some(expected.as_str()) {
            bail!("bad or missing X-Dabping-Token");
        }
        Ok(())
    }

    pub fn manual_update(&self, id: u64, message: &str, token: Option<&str>, ts: i64) -> Result<()> {
        self.check_token(token)?;
        let mut log = self.incidents.lock().expect("incident lock poisoned");
        if !log.incidents.iter().any(|i| i.id == id) {
            bail!("no incident #{id}");
        }
        log.record(Event::Update { id, ts, message: message.to_string() })
    }

    pub fn manual_resolve(&self, id: u64, token: Option<&str>, ts: i64) -> Result<()> {
        self.check_token(token)?;
        let mut log = self.incidents.lock().expect("incident lock poisoned");
        if !log.incidents.iter().any(|i| i.id == id && i.resolved.is_none()) {
            bail!("no open incident #{id}");
        }
        log.record(Event::Update { id, ts, message: "Manually resolved".into() })?;
        log.record(Event::Resolve { id, ts })
    }
}

/// Daily availability for a set of targets: 100 - mean(loss%) over each
/// day's consolidated points, averaged across the component's targets.
/// None = no data that day.
pub fn uptime_days(
    store: &Store,
    targets: &[String],
    days: u32,
    now: i64,
) -> Vec<(i64, Option<f64>)> {
    let from = now - days as i64 * 86400;
    let mut day_acc: BTreeMap<i64, (f64, u32)> = BTreeMap::new();
    for t in targets {
        let Ok(f) = store.fetch(t, Cf::Average, from, now, days as u64 * 24) else {
            continue; // no data series yet
        };
        for p in f.points {
            if p.loss.is_nan() {
                continue;
            }
            let day = p.ts.div_euclid(86400);
            let e = day_acc.entry(day).or_default();
            e.0 += p.loss;
            e.1 += 1;
        }
    }
    let first_day = from.div_euclid(86400);
    let today = now.div_euclid(86400);
    (first_day..=today)
        .map(|d| {
            let pct = day_acc.get(&d).map(|(sum, n)| 100.0 - sum / *n as f64);
            (d * 86400, pct)
        })
        .collect()
}

/// Emitter wiring, mirroring StoreEmitter.
pub struct StatusEmitter(pub Arc<StatusEngine>);

impl Emitter for StatusEmitter {
    fn emit(&self, r: &RoundResult) {
        self.0.process_round(r);
    }
}

/// Used by Config::validate without touching the incident log.
pub fn validate(cfg: &Config) -> Result<()> {
    parse_components(cfg).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StatusComponentConfig;
    use std::time::{Duration, SystemTime};

    fn cfg(extra: &str) -> Config {
        let mut cfg: Config = toml::from_str(&format!(
            r#"
            [database]
            dir = "{}"

            [status]
            title = "Test Status"
            admin_token = "sekrit"

            {extra}

            [targets.t]
              [targets.t.a]
              host = "192.0.2.1"
              [targets.t.b]
              host = "192.0.2.2"
            "#,
            tempfile::tempdir().unwrap().keep().display()
        ))
        .unwrap();
        cfg.probes.insert("icmp".into(), crate::config::ProbeConfig::Icmp(Default::default()));
        cfg
    }

    fn engine(extra: &str) -> StatusEngine {
        StatusEngine::new(&cfg(extra)).unwrap().expect("engine")
    }

    fn round(target: &str, loss_of_4: u32) -> RoundResult {
        RoundResult {
            target: target.into(),
            host: "192.0.2.1".into(),
            addr: "192.0.2.1".parse().unwrap(),
            started: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
            sent: 4,
            rtts: (0..(4 - loss_of_4)).map(|_| Duration::from_millis(10)).collect(),
        }
    }

    const TWO_TARGET: &str = r#"
        [status.components.svc]
        name = "Service"
        targets = ["t/a", "t/b"]
        down_loss = "==100%,==100%"
        degraded_loss = ">5%,>5%"
    "#;

    #[test]
    fn aggregates_partial_and_major() {
        let e = engine(TWO_TARGET);
        // both healthy
        e.process_round(&round("t/a", 0));
        e.process_round(&round("t/b", 0));
        assert_eq!(e.snapshot().0, Severity::Operational);
        // a goes fully down (2 consecutive 100%-loss rounds)
        e.process_round(&round("t/a", 4));
        e.process_round(&round("t/a", 4));
        assert_eq!(e.snapshot().0, Severity::PartialOutage);
        // b too: major
        e.process_round(&round("t/b", 4));
        e.process_round(&round("t/b", 4));
        assert_eq!(e.snapshot().0, Severity::MajorOutage);
        // a recovers: back to partial
        e.process_round(&round("t/a", 0));
        assert_eq!(e.snapshot().0, Severity::PartialOutage);
    }

    #[test]
    fn degraded_from_loss_pattern() {
        let e = engine(TWO_TARGET);
        e.process_round(&round("t/a", 1)); // 25% loss
        assert_eq!(e.snapshot().0, Severity::Operational); // one round: not yet
        e.process_round(&round("t/a", 1));
        assert_eq!(e.snapshot().0, Severity::Degraded);
        let (_, comps) = e.snapshot();
        assert_eq!(comps[0].status, Severity::Degraded);
    }

    #[test]
    fn incident_lifecycle_and_persistence() {
        let c = cfg(TWO_TARGET);
        let e = StatusEngine::new(&c).unwrap().unwrap();
        for _ in 0..2 {
            e.process_round(&round("t/a", 4));
            e.process_round(&round("t/b", 4));
        }
        let inc = e.incidents();
        assert_eq!(inc.len(), 1, "one incident for the outage");
        assert!(inc[0].resolved.is_none());
        assert!(inc[0].title.contains("Service"));
        // recovery resolves it
        e.process_round(&round("t/a", 0));
        e.process_round(&round("t/b", 0));
        let inc = e.incidents();
        assert!(inc[0].resolved.is_some());
        assert!(inc[0].updates.len() >= 2);

        // replay from disk sees the same state
        let e2 = StatusEngine::new(&c).unwrap().unwrap();
        let inc2 = e2.incidents();
        assert_eq!(inc2.len(), 1);
        assert_eq!(inc2[0].id, inc[0].id);
        assert!(inc2[0].resolved.is_some());
    }

    #[test]
    fn manual_ops_need_token() {
        let e = engine(TWO_TARGET);
        for _ in 0..2 {
            e.process_round(&round("t/a", 4));
            e.process_round(&round("t/b", 4));
        }
        let id = e.incidents()[0].id;
        assert!(e.manual_update(id, "looking into it", None, 1).is_err());
        assert!(e.manual_update(id, "looking into it", Some("wrong"), 1).is_err());
        e.manual_update(id, "looking into it", Some("sekrit"), 1).unwrap();
        e.manual_resolve(id, Some("sekrit"), 2).unwrap();
        assert!(e.incidents()[0].resolved.is_some());
    }

    #[test]
    fn rejects_unknown_component_target() {
        let mut c = cfg("");
        c.status.as_mut().unwrap().components.insert(
            "bad".into(),
            StatusComponentConfig {
                name: None,
                group: None,
                targets: vec!["nope/nothere".into()],
                degraded_loss: None,
                down_loss: None,
                degraded_rtt: None,
                incident_at: None,
            },
        );
        assert!(parse_components(&c).is_err());
    }
}
