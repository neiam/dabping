//! Web UI / API: axum serving the embedded SPA, JSON endpoints over the
//! store, and a WebSocket live feed of completed rounds.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::config::{self, TargetNode};
use crate::emit::Emitter;
use crate::probe::RoundResult;
use crate::status::{StatusEngine, uptime_days};
use crate::store::{Cf, Store};
use crate::util::{parse_range, unix_now};

#[derive(RustEmbed)]
#[folder = "src/web/assets/"]
struct Assets;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    /// Pre-serialized target tree (static for the daemon's lifetime).
    pub tree: Arc<serde_json::Value>,
    pub live: broadcast::Sender<String>,
    pub status: Option<Arc<StatusEngine>>,
    pub prom: Option<Arc<crate::emit::prometheus::PromState>>,
    /// All emitters; agent-pushed rounds flow through the same pipeline.
    pub emitters: Arc<Vec<Box<dyn Emitter>>>,
    /// name → (secret, assignment, allowed target paths).
    pub agents: Arc<std::collections::HashMap<String, AgentEntry>>,
    /// (path, host) of every leaf target, for the charts ranking.
    pub targets: Arc<Vec<(String, String)>>,
    pub step: u64,
    pub pings: u32,
}

pub struct AgentEntry {
    pub secret: String,
    pub assignment: serde_json::Value,
    pub allowed: std::collections::HashSet<String>,
}

/// Per-agent assignments, computed once from the flattened config.
/// Built as raw JSON (the agent deserializes into agent::Assignment).
pub fn build_agents(cfg: &config::Config) -> anyhow::Result<std::collections::HashMap<String, AgentEntry>> {
    use crate::agent::WireTarget;
    let flat = cfg.flatten_targets()?;
    let mut out = std::collections::HashMap::new();
    for (name, ac) in &cfg.agents {
        let targets: Vec<&config::FlatTarget> =
            flat.iter().filter(|t| t.agents.contains(name)).collect();
        let probes: BTreeMap<&String, &config::ProbeConfig> = targets
            .iter()
            .filter_map(|t| cfg.probes.get_key_value(&t.probe))
            .collect();
        let assignment = serde_json::json!({
            "step": cfg.database.step,
            "pings": cfg.database.pings,
            "probes": probes,
            "targets": targets.iter().map(|t| WireTarget::from_flat(t)).collect::<Vec<_>>(),
        });
        out.insert(
            name.clone(),
            AgentEntry {
                secret: ac.secret.clone(),
                assignment,
                allowed: targets.iter().map(|t| t.path.clone()).collect(),
            },
        );
    }
    Ok(out)
}

/// The target tree as served to the UI.
#[derive(Debug, Serialize)]
pub struct TreeNode {
    pub name: String,
    /// Full slash path of this node.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub menu: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Agents that also measure this target (per-agent series exist
    /// under "path@agent").
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    pub children: Vec<TreeNode>,
}

pub fn build_tree(cfg: &config::Config) -> serde_json::Value {
    // inherited agents are only resolved in the flattened view
    let agent_map: std::collections::HashMap<String, Vec<String>> = cfg
        .flatten_targets()
        .map(|flat| flat.into_iter().map(|t| (t.path, t.agents)).collect())
        .unwrap_or_default();
    fn walk(
        name: &str,
        path: String,
        node: &TargetNode,
        agents: &std::collections::HashMap<String, Vec<String>>,
    ) -> TreeNode {
        TreeNode {
            name: name.to_string(),
            title: node.title.clone(),
            menu: node.menu.clone(),
            host: node.host.clone(),
            agents: agents.get(&path).cloned().unwrap_or_default(),
            children: node
                .children
                .iter()
                .map(|(n, c)| walk(n, format!("{path}/{n}"), c, agents))
                .collect(),
            path,
        }
    }
    let nodes: Vec<TreeNode> =
        cfg.targets.iter().map(|(n, c)| walk(n, n.clone(), c, &agent_map)).collect();
    serde_json::to_value(nodes).expect("tree serializes")
}

/// Emitter that fans completed rounds out to WebSocket clients.
pub struct LiveEmitter(pub broadcast::Sender<String>);

impl Emitter for LiveEmitter {
    fn emit(&self, r: &RoundResult) {
        let msg = serde_json::json!({
            "type": "round",
            "target": r.target,
            "host": r.host,
            "addr": r.addr.to_string(),
            "ts": r.started.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            "sent": r.sent,
            "recv": r.received(),
            "loss": r.loss_pct(),
            "median": r.median().map(|d| d.as_secs_f64()),
            "rtts": r.sorted_rtts().iter().map(|d| d.as_secs_f64()).collect::<Vec<_>>(),
        });
        // no receivers is fine — nobody has the UI open
        let _ = self.0.send(msg.to_string());
    }
}

pub async fn serve(cfg: config::Web, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/api/meta", get(meta))
        .route("/api/tree", get(tree))
        .route("/api/charts/top", get(charts_top))
        .route("/api/data/{*path}", get(data))
        .route("/api/holdload", get(holdload))
        .route("/api/live", get(live))
        .route("/api/agent/config", get(agent_config))
        .route("/api/agent/results", axum::routing::post(agent_results))
        .route("/metrics", get(metrics))
        .route("/status", get(status_page))
        .route("/api/status.json", get(status_json))
        .route("/api/status/incident/{id}/update", axum::routing::post(incident_update))
        .route("/api/status/incident/{id}/resolve", axum::routing::post(incident_resolve))
        .fallback(asset)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("cannot listen on {}", cfg.listen))?;
    tracing::info!(listen = %cfg.listen, "web ui up");
    axum::serve(listener, app).await.context("web server failed")
}

async fn meta(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "step": st.step, "pings": st.pings }))
}

async fn tree(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json((*st.tree).clone())
}

#[derive(Deserialize)]
struct DataQuery {
    #[serde(default = "default_range")]
    range: String,
    #[serde(default = "default_cf")]
    cf: String,
    #[serde(default = "default_points")]
    points: u64,
    /// Explicit window (unix secs) — overrides `range` when both are set.
    /// This is what drag-to-zoom uses.
    from: Option<i64>,
    to: Option<i64>,
}

fn default_range() -> String {
    "3h".into()
}
fn default_cf() -> String {
    "average".into()
}
fn default_points() -> u64 {
    400
}

async fn data(
    Path(path): Path<String>,
    Query(q): Query<DataQuery>,
    State(st): State<AppState>,
) -> Response {
    tracing::debug!(%path, range = %q.range, points = q.points, "data request");
    let result = (|| {
        let cf: Cf = q.cf.parse()?;
        let now = unix_now();
        let (from, to) = match (q.from, q.to) {
            (Some(f), Some(t)) if f < t => (f, t),
            _ => (now - parse_range(&q.range)? as i64, now),
        };
        st.store.fetch(&path, cf, from, to, q.points)
    })();
    match result {
        Ok(f) => Json(f).into_response(),
        Err(e) => {
            let code = if e.to_string().contains("no data recorded") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, Json(serde_json::json!({ "error": e.to_string() }))).into_response()
        }
    }
}

/// Testing aid: ?snap mode adds an <img> pointing here, which delays the
/// window load event until we answer — so headless screenshots are taken
/// after the graphs have drawn.
async fn holdload(Query(q): Query<std::collections::HashMap<String, String>>) -> StatusCode {
    let ms = q.get("ms").and_then(|v| v.parse().ok()).unwrap_or(3000).min(10_000);
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    StatusCode::NO_CONTENT
}

async fn live(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |sock| live_loop(sock, st.live.subscribe()))
}

async fn live_loop(sock: WebSocket, mut rx: broadcast::Receiver<String>) {
    let (mut tx, mut from_client) = sock.split();
    loop {
        tokio::select! {
            m = rx.recv() => match m {
                Ok(s) => {
                    if tx.send(Message::Text(s.into())).await.is_err() {
                        return; // client went away
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
            m = from_client.next() => match m {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {} // ignore pings/chatter
            },
        }
    }
}

// ---- charts (top-N) ----

#[derive(Deserialize)]
struct ChartsQuery {
    #[serde(default = "default_by")]
    by: String,
    #[serde(default = "default_n")]
    n: usize,
}

fn default_by() -> String {
    "median".into()
}
fn default_n() -> usize {
    10
}

#[derive(Debug, Serialize)]
pub struct TopEntry {
    pub path: String,
    pub host: String,
    pub median: Option<f64>,
    pub loss: f64,
    pub ts: i64,
}

/// SmokePing's charts mode: rank targets by their latest round.
pub fn top_targets(
    store: &Store,
    targets: &[(String, String)],
    by: &str,
    n: usize,
    now: i64,
    step: u64,
) -> Vec<TopEntry> {
    let mut entries: Vec<TopEntry> = targets
        .iter()
        .filter_map(|(path, host)| {
            // look back a few steps for the newest valid raw point
            let f = store.fetch(path, Cf::Average, now - 5 * step as i64, now, 0).ok()?;
            let p = f.points.iter().rev().find(|p| !p.loss.is_nan())?;
            Some(TopEntry {
                path: path.clone(),
                host: host.clone(),
                median: (!p.median.is_nan()).then_some(p.median),
                loss: p.loss,
                ts: p.ts,
            })
        })
        .collect();
    match by {
        "loss" => entries.sort_by(|a, b| {
            b.loss.total_cmp(&a.loss).then(
                b.median.unwrap_or(f64::NEG_INFINITY).total_cmp(&a.median.unwrap_or(f64::NEG_INFINITY)),
            )
        }),
        _ => entries.sort_by(|a, b| {
            b.median
                .unwrap_or(f64::NEG_INFINITY)
                .total_cmp(&a.median.unwrap_or(f64::NEG_INFINITY))
        }),
    }
    entries.truncate(n);
    entries
}

async fn charts_top(State(st): State<AppState>, Query(q): Query<ChartsQuery>) -> Json<Vec<TopEntry>> {
    Json(top_targets(&st.store, &st.targets, &q.by, q.n.min(100), unix_now(), st.step))
}

// ---- distributed agents ----

/// Validate X-Dabping-Agent/-Secret; returns the agent name.
fn agent_auth<'a>(st: &'a AppState, headers: &axum::http::HeaderMap) -> Result<&'a str, Response> {
    let name = headers.get("x-dabping-agent").and_then(|v| v.to_str().ok());
    let secret = headers.get("x-dabping-secret").and_then(|v| v.to_str().ok());
    let unauthorized =
        || (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "bad agent credentials"}))).into_response();
    let (Some(name), Some(secret)) = (name, secret) else {
        return Err(unauthorized());
    };
    match st.agents.get_key_value(name) {
        Some((key, entry)) if entry.secret == secret => Ok(key.as_str()),
        _ => Err(unauthorized()),
    }
}

async fn agent_config(State(st): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    match agent_auth(&st, &headers) {
        Ok(name) => Json(st.agents[name].assignment.clone()).into_response(),
        Err(r) => r,
    }
}

async fn agent_results(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(rounds): Json<Vec<crate::agent::WireRound>>,
) -> Response {
    let name = match agent_auth(&st, &headers) {
        Ok(n) => n.to_string(),
        Err(r) => return r,
    };
    let entry = &st.agents[&name];
    let mut stored = 0usize;
    let mut rejected = 0usize;
    for w in rounds {
        if !entry.allowed.contains(&w.target) {
            rejected += 1;
            continue; // not this agent's target (stale assignment?)
        }
        let r = w.into_round(&name);
        for e in st.emitters.iter() {
            e.emit(&r);
        }
        stored += 1;
    }
    if rejected > 0 {
        tracing::warn!(agent = %name, rejected, "agent pushed rounds for unassigned targets");
    }
    Json(serde_json::json!({"stored": stored, "rejected": rejected})).into_response()
}

async fn metrics(State(st): State<AppState>) -> Response {
    match &st.prom {
        Some(p) => (
            [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
            p.render(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no prometheus emitter configured").into_response(),
    }
}

// ---- status page ----

fn status_env() -> &'static minijinja::Environment<'static> {
    static ENV: std::sync::OnceLock<minijinja::Environment<'static>> = std::sync::OnceLock::new();
    ENV.get_or_init(|| {
        let mut env = minijinja::Environment::new();
        env.add_template("status", include_str!("templates/status.html.j2"))
            .expect("status template parses");
        env
    })
}

fn fmt_ts(ts: i64) -> String {
    jiff::Timestamp::from_second(ts)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M %Z")
                .to_string()
        })
        .unwrap_or_else(|_| ts.to_string())
}

fn incident_json(i: &crate::status::Incident) -> serde_json::Value {
    serde_json::json!({
        "id": i.id,
        "component": i.component,
        "title": i.title,
        "status": if i.resolved.is_some() { "resolved" } else { "open" },
        "opened_at": i.opened,
        "opened_human": fmt_ts(i.opened),
        "resolved_at": i.resolved,
        "resolved_human": i.resolved.map(fmt_ts),
        "updates": i.updates.iter().map(|u| serde_json::json!({
            "ts": u.ts, "when": fmt_ts(u.ts), "message": u.message,
        })).collect::<Vec<_>>(),
    })
}

/// Shared context for the HTML page and status.json.
fn status_context(st: &AppState, eng: &StatusEngine) -> serde_json::Value {
    let now = unix_now();
    let (overall, comps) = eng.snapshot();

    // group components, preserving config order
    let mut groups: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
    for (snap, comp) in comps.iter().zip(eng.components()) {
        let days = uptime_days(&st.store, &comp.targets, eng.history_days, now);
        let known: Vec<f64> = days.iter().filter_map(|(_, p)| *p).collect();
        let uptime_label = if known.is_empty() {
            "no data".to_string()
        } else {
            format!("{:.2}% uptime", known.iter().sum::<f64>() / known.len() as f64)
        };
        let bars: Vec<serde_json::Value> = days
            .iter()
            .map(|(ts, pct)| {
                let cls = match pct {
                    None => "",
                    Some(p) if *p >= 99.5 => "ok",
                    Some(p) if *p >= 95.0 => "warn",
                    Some(_) => "bad",
                };
                let date = jiff::Timestamp::from_second(*ts)
                    .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%m-%d").to_string())
                    .unwrap_or_default();
                let label = match pct {
                    Some(p) => format!("{date}: {p:.2}% uptime"),
                    None => format!("{date}: no data"),
                };
                serde_json::json!({ "cls": cls, "label": label })
            })
            .collect();
        let cjson = serde_json::json!({
            "key": snap.key,
            "name": snap.name,
            "status": snap.status.as_statuspage(),
            "status_human": snap.status.human(),
            "group": snap.group,
            "bars": bars,
            "uptime_label": uptime_label,
        });
        match groups.iter_mut().find(|(g, _)| *g == snap.group) {
            Some((_, v)) => v.push(cjson),
            None => groups.push((snap.group.clone(), vec![cjson])),
        }
    }

    let mut incidents = eng.incidents();
    incidents.sort_by_key(|i| std::cmp::Reverse(i.opened));
    let open: Vec<_> = incidents.iter().filter(|i| i.resolved.is_none()).map(incident_json).collect();
    let resolved: Vec<_> =
        incidents.iter().filter(|i| i.resolved.is_some()).take(10).map(incident_json).collect();

    serde_json::json!({
        "title": eng.title,
        "updated": fmt_ts(now),
        "updated_at": now,
        "history_days": eng.history_days,
        "overall": {
            "indicator": overall.indicator(),
            "banner": overall.banner(),
            "status": overall.as_statuspage(),
        },
        "groups": groups.into_iter().map(|(name, components)| serde_json::json!({
            "name": name, "components": components,
        })).collect::<Vec<_>>(),
        "open_incidents": open,
        "resolved_incidents": resolved,
    })
}

async fn status_page(State(st): State<AppState>) -> Response {
    let Some(eng) = &st.status else {
        return (StatusCode::NOT_FOUND, "no status page configured").into_response();
    };
    let ctx = status_context(&st, eng);
    match status_env().get_template("status").and_then(|t| t.render(&ctx)) {
        Ok(html) => axum::response::Html(html).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "status template render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response()
        }
    }
}

/// Statuspage-compatible shape (page/status/components/incidents).
async fn status_json(State(st): State<AppState>) -> Response {
    let Some(eng) = &st.status else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no status page configured"})))
            .into_response();
    };
    let ctx = status_context(&st, eng);
    let comps: Vec<_> = ctx["groups"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .flat_map(|g| g["components"].as_array().cloned().unwrap_or_default())
        .map(|c| {
            serde_json::json!({
                "id": c["key"], "name": c["name"], "status": c["status"], "group": c["group"],
            })
        })
        .collect();
    Json(serde_json::json!({
        "page": { "name": ctx["title"], "updated_at": ctx["updated_at"] },
        "status": {
            "indicator": ctx["overall"]["indicator"],
            "description": ctx["overall"]["banner"],
        },
        "components": comps,
        "incidents": ctx["open_incidents"],
        "resolved_incidents": ctx["resolved_incidents"],
    }))
    .into_response()
}

#[derive(Deserialize)]
struct IncidentMsg {
    message: String,
}

fn token_of(headers: &axum::http::HeaderMap) -> Option<String> {
    headers.get("x-dabping-token").and_then(|v| v.to_str().ok()).map(str::to_string)
}

async fn incident_update(
    Path(id): Path<u64>,
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<IncidentMsg>,
) -> Response {
    incident_op(&st, |eng| eng.manual_update(id, &body.message, token_of(&headers).as_deref(), unix_now()))
}

async fn incident_resolve(
    Path(id): Path<u64>,
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    incident_op(&st, |eng| eng.manual_resolve(id, token_of(&headers).as_deref(), unix_now()))
}

fn incident_op(st: &AppState, f: impl FnOnce(&StatusEngine) -> anyhow::Result<()>) -> Response {
    let Some(eng) = &st.status else {
        return (StatusCode::NOT_FOUND, "no status page configured").into_response();
    };
    match f(eng) {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => {
            let code = if e.to_string().contains("Token") || e.to_string().contains("disabled") {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, Json(serde_json::json!({"error": e.to_string()}))).into_response()
        }
    }
}

async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(f) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::CACHE_CONTROL, "no-cache".to_string()),
                ],
                f.data,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
