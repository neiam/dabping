//! Distributed mode. The same binary runs as a measurement agent:
//! it pulls its assignment from the master, probes, and pushes results
//! back in batches, buffering locally while the master is unreachable.
//! Master-side ingest lives in web/mod.rs; the wire types live here.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::{FlatTarget, ProbeConfig};
use crate::emit::{Emitter, LogEmitter};
use crate::probe::{ProbeInstance, RoundResult};

/// What the master hands an agent: GET /api/agent/config.
#[derive(Debug, Serialize, Deserialize)]
pub struct Assignment {
    pub step: u64,
    pub pings: u32,
    pub probes: BTreeMap<String, ProbeConfig>,
    pub targets: Vec<WireTarget>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WireTarget {
    pub path: String,
    pub host: String,
    pub probe: String,
    pub port: Option<u16>,
    pub lookup: Option<String>,
}

impl WireTarget {
    pub fn from_flat(t: &FlatTarget) -> WireTarget {
        WireTarget {
            path: t.path.clone(),
            host: t.host.clone(),
            probe: t.probe.clone(),
            port: t.port,
            lookup: t.lookup.clone(),
        }
    }

    fn into_flat(self) -> FlatTarget {
        FlatTarget {
            path: self.path,
            host: self.host,
            probe: self.probe,
            port: self.port,
            lookup: self.lookup,
            alerts: Vec::new(), // alerting stays on the master
            agents: Vec::new(),
            nomasterpoll: false,
        }
    }
}

/// One round on the wire: POST /api/agent/results carries a Vec of these.
#[derive(Debug, Serialize, Deserialize)]
pub struct WireRound {
    pub target: String,
    pub host: String,
    pub addr: String,
    /// Unix seconds the round started.
    pub ts: u64,
    pub sent: u32,
    /// RTTs in seconds, send order.
    pub rtts: Vec<f64>,
}

impl WireRound {
    pub fn from_round(r: &RoundResult) -> WireRound {
        WireRound {
            target: r.target.clone(),
            host: r.host.clone(),
            addr: r.addr.to_string(),
            ts: r.started.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default(),
            sent: r.sent,
            rtts: r.rtts.iter().map(|d| d.as_secs_f64()).collect(),
        }
    }

    /// Master side: rebuild the round under its per-agent series key.
    pub fn into_round(self, agent: &str) -> RoundResult {
        RoundResult {
            target: format!("{}@{agent}", self.target),
            host: self.host,
            addr: self.addr.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED.into()),
            started: UNIX_EPOCH + Duration::from_secs(self.ts),
            sent: self.sent,
            rtts: self
                .rtts
                .into_iter()
                .filter(|s| s.is_finite() && *s >= 0.0)
                .map(Duration::from_secs_f64)
                .collect(),
        }
    }
}

pub struct AgentOpts {
    pub master: String,
    pub name: String,
    pub secret: String,
}

fn endpoint(master: &str, path: &str) -> String {
    format!("{}/api/agent/{path}", master.trim_end_matches('/'))
}

fn client(opts: &AgentOpts) -> Result<reqwest::Client> {
    use reqwest::header::{HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert("X-Dabping-Agent", HeaderValue::from_str(&opts.name).context("agent name")?);
    headers.insert("X-Dabping-Secret", HeaderValue::from_str(&opts.secret).context("secret")?);
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .default_headers(headers)
        .user_agent(concat!("dabping-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("cannot build http client")
}

/// `dabping agent` entry point: fetch assignment, probe, push.
/// SIGHUP re-fetches the assignment from the master.
pub async fn run(opts: AgentOpts) -> Result<()> {
    loop {
        match run_once(&opts).await? {
            crate::scheduler::Outcome::Quit => return Ok(()),
            crate::scheduler::Outcome::Reload => {
                tracing::info!("SIGHUP: re-fetching assignment from master");
                continue;
            }
        }
    }
}

async fn run_once(opts: &AgentOpts) -> Result<crate::scheduler::Outcome> {
    let client = client(opts)?;
    let cfg_url = endpoint(&opts.master, "config");

    let assignment: Assignment = loop {
        match client.get(&cfg_url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json().await {
                Ok(a) => break a,
                Err(e) => tracing::error!(error = %e, "bad assignment from master"),
            },
            Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
                anyhow::bail!("master rejected agent credentials (check name/secret)");
            }
            Ok(resp) => tracing::warn!(status = %resp.status(), "master not ready"),
            Err(e) => tracing::warn!(error = %e, "master unreachable; retrying"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };

    tracing::info!(
        agent = %opts.name,
        targets = assignment.targets.len(),
        step = assignment.step,
        "assignment received"
    );
    if assignment.targets.is_empty() {
        tracing::warn!("master assigned no targets to this agent; idling (restart after config changes)");
    }

    let probes: Arc<HashMap<String, ProbeInstance>> = Arc::new(
        assignment
            .probes
            .iter()
            .map(|(name, pc)| Ok((name.clone(), ProbeInstance::from_config(pc)?)))
            .collect::<Result<_>>()?,
    );
    let targets: Vec<FlatTarget> = assignment.targets.into_iter().map(WireTarget::into_flat).collect();

    let push = PushEmitter::spawn(client, endpoint(&opts.master, "results"));
    let emitters: Arc<Vec<Box<dyn Emitter>>> = Arc::new(vec![Box::new(LogEmitter), Box::new(push)]);

    crate::scheduler::run_targets(targets, probes, assignment.step, assignment.pings, emitters, None)
        .await
}

/// Buffers rounds and ships them to the master every few seconds.
/// Unreachable master ⇒ rounds accumulate (bounded); oldest drop first.
pub struct PushEmitter {
    tx: mpsc::Sender<WireRound>,
}

const PUSH_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BUFFER: usize = 10_000;

impl PushEmitter {
    pub fn spawn(client: reqwest::Client, url: String) -> PushEmitter {
        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(push_loop(client, url, rx, PUSH_INTERVAL));
        PushEmitter { tx }
    }
}

impl Emitter for PushEmitter {
    fn emit(&self, r: &RoundResult) {
        if self.tx.try_send(WireRound::from_round(r)).is_err() {
            tracing::warn!(path = %r.target, "push queue full; dropping round");
        }
    }
}

async fn push_loop(
    client: reqwest::Client,
    url: String,
    mut rx: mpsc::Receiver<WireRound>,
    interval: Duration,
) {
    let mut buffer: VecDeque<WireRound> = VecDeque::new();
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Some(r) => {
                    buffer.push_back(r);
                    while buffer.len() > MAX_BUFFER {
                        buffer.pop_front();
                    }
                }
                None => {
                    let _ = try_push(&client, &url, &mut buffer).await;
                    return;
                }
            },
            _ = tick.tick() => {
                if let Err(e) = try_push(&client, &url, &mut buffer).await {
                    tracing::warn!(error = %e, buffered = buffer.len(), "push to master failed; buffering");
                }
            }
        }
    }
}

async fn try_push(
    client: &reqwest::Client,
    url: &str,
    buffer: &mut VecDeque<WireRound>,
) -> Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }
    let batch: Vec<&WireRound> = buffer.iter().collect();
    let resp = client.post(url).json(&batch).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("master answered {}", resp.status());
    }
    buffer.clear();
    Ok(())
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
    fn wire_roundtrip_suffixes_agent() {
        let w = WireRound::from_round(&round());
        let json = serde_json::to_string(&w).unwrap();
        let back: WireRound = serde_json::from_str(&json).unwrap();
        let r = back.into_round("lon1");
        assert_eq!(r.target, "net/cf@lon1");
        assert_eq!(r.sent, 4);
        assert_eq!(r.received(), 2);
        assert_eq!(r.rtts[0], Duration::from_millis(10));
        assert_eq!(
            r.started.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1000
        );
    }

    #[test]
    fn hostile_rtts_are_dropped() {
        let w = WireRound {
            target: "t".into(),
            host: "h".into(),
            addr: "not-an-ip".into(),
            ts: 0,
            sent: 4,
            rtts: vec![0.01, f64::NAN, -5.0, f64::INFINITY],
        };
        let r = w.into_round("a");
        assert_eq!(r.received(), 1);
        assert!(r.addr.is_unspecified());
    }

    #[tokio::test]
    async fn push_buffers_through_failure_then_delivers() {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU32, Ordering};
        let hits = Arc::new(AtomicU32::new(0));
        let bodies: Arc<Mutex<Vec<Vec<WireRound>>>> = Arc::default();
        let (h2, b2) = (hits.clone(), bodies.clone());
        let app = axum::Router::new().route(
            "/api/agent/results",
            axum::routing::post(move |axum::Json(v): axum::Json<Vec<WireRound>>| {
                let n = h2.fetch_add(1, Ordering::SeqCst);
                let b = b2.clone();
                async move {
                    if n == 0 {
                        // first delivery attempt fails: agent must keep the rounds
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        b.lock().unwrap().push(v);
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/api/agent/results",
            listener.local_addr().unwrap().port()
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(push_loop(client, url, rx, Duration::from_millis(50)));
        tx.send(WireRound::from_round(&round())).await.unwrap();
        tx.send(WireRound::from_round(&round())).await.unwrap();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if !bodies.lock().unwrap().is_empty() {
                break;
            }
        }
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "one successful delivery");
        assert_eq!(bodies[0].len(), 2, "both rounds survived the failed attempt");
        assert!(hits.load(Ordering::SeqCst) >= 2, "first attempt failed, second succeeded");
    }
}
