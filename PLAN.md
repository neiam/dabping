# dabping — Implementation Plan

A Rust reimplementation of SmokePing / vaping: multi-target network latency
measurement with "smoke" distribution graphs, hierarchical target trees,
pattern-based alerting, pluggable probes/emitters, and distributed agents —
shipped as a single static binary.

## Feature parity targets

### From SmokePing
| Feature | Notes |
|---|---|
| N-pings-per-round measurement | e.g. 20 pings every 300s; store full distribution, not just an average |
| Smoke graphs | median line colored by loss %, smoke shading = min→max distribution of the round |
| Hierarchical target tree | nested sections, menu/title per node, per-node probe override |
| Multiple probes | ICMP, DNS, HTTP(S), TCP, SSH banner, exec (external command), etc. |
| RRD-style storage | fixed-size round-robin archives, multi-resolution consolidation (AVERAGE/MIN/MAX) |
| Web UI | overview pages, detail pages w/ standard time ranges (3h/30h/10d/360d), zoom/pan navigation |
| Charts mode | top-N targets by median RTT / loss |
| Multi-host graphs | overlay several targets on one graph |
| Alerts | pattern DSL over consecutive samples (loss & RTT), edge-triggered, repeat interval, email/exec notifiers |
| Master/slave | agents fetch config from master, push results; per-target slave list; comparative display |
| DYNAMIC targets | hosts with changing IPs that check in periodically (low priority / stretch) |
| Hot config reload | SIGHUP / file-watch |

### From vaping
| Feature | Notes |
|---|---|
| Everything-is-a-plugin design | probes (inputs) and emitters (outputs) behind traits |
| Real-time browser graphs | WebSocket push, live smokestack + line views |
| TSDB emitters | Prometheus, Graphite, InfluxDB line protocol; plus log/stdout |
| Message-bus distribution | results streamable to external consumers (NATS or plain HTTP push) |
| Modern config | one YAML/TOML file, containerized deploy |

### New: public status page (Atlassian Statuspage-style)
| Feature | Notes |
|---|---|
| Component status board | targets (or groups of targets) mapped to named "components" with operational / degraded / partial outage / major outage states |
| Status derivation rules | per-component thresholds on the same round data: e.g. degraded = loss >5% or median > 2× baseline; down = loss 100% for N rounds — reuses the alert matcher engine |
| Overall banner | "All Systems Operational" / worst-component rollup at the top |
| Uptime history bars | 90-day per-component daily bars (green/yellow/red) with hover tooltips showing % uptime, computed from consolidated RRAs |
| Incident timeline | open/resolved incidents with updates; auto-opened from alert triggers, manually annotatable; resolved history list |
| Public, unauthenticated route | `/status` standalone page, separate from the operator UI; safe to expose — shows component names only, no hostnames/IPs unless configured |
| Subscribe hooks (stretch) | RSS/Atom feed of incidents; webhook on status change |

## Architecture

Single binary, tokio async runtime. Modules (start as one crate, split into a
workspace only if it gets big):

```
src/
  main.rs            CLI (clap): run | check-config | once <target> | agent
  config/            serde config model + validation + hierarchy flattening
  scheduler/         per-target rounds, jittered offsets, concurrency caps
  probe/             Probe trait + implementations
    icmp.rs          native ICMP v4/v6 (unprivileged DGRAM first, RAW fallback)
    tcp.rs           TCP connect timing (TCPPing equivalent)
    dns.rs           hickory-resolver query timing
    http.rs          reqwest GET/HEAD timing (EchoPingHttp/Curl equivalent)
    exec.rs          external command, fping-output compatible (escape hatch
                     for everything SmokePing does via exotic probes)
  store/             RRD-like storage engine
  alert/             pattern matchers + notifier dispatch
  emit/              Emitter trait: prometheus, graphite, influx, log, ws
  web/               axum: REST API, WebSocket live feed, embedded SPA
  status/            status page: component model, state derivation, incidents
  agent/             distributed mode (master + agent client)
```

### Probe model
- A **round** = `pings` measurements of one target → `RoundResult { sent, received, rtts: Vec<Duration> }`.
- `Probe` trait: `async fn round(&self, target) -> RoundResult`, plus
  per-probe config (timeout, interval-within-round, payload size, source addr).
- ICMP: own implementation over `socket2` — try unprivileged
  `SOCK_DGRAM/IPPROTO_ICMP` first (works with `net.ipv4.ping_group_range`),
  fall back to raw socket (needs `CAP_NET_RAW`; document `setcap` in install).
  One shared socket + dispatcher task, not one socket per target.
- Multiple instances of the same probe type with different settings
  (SmokePing's multi-instance probes), per-target probe/param overrides.

### Storage (the RRD replacement)
- Custom file-per-target round-robin store, memory-mapped:
  - DS per round: `loss`, `median`, `ping_1..ping_N` (sorted RTTs) — same shape as SmokePing's RRDs so smoke rendering works at every resolution.
  - RRAs: configurable consolidation table, default mirrors SmokePing:
    raw @ step for ~3.5d, then AVERAGE/MIN/MAX at 12×, 144×, 288× steps out to ~1y.
  - Fixed file size, append by slot index = O(1) writes, crash-safe (slot header with sequence).
- Query API: `fetch(target, range, max_points)` → picks best-resolution RRA, used by both graph rendering and REST.
- Stretch: import tool for existing SmokePing `.rrd` files (parse rrdtool dump XML rather than binding librrd).

### Web UI
- axum serving:
  - `GET /api/tree`, `GET /api/target/{path}/data?range=`, `GET /api/charts/top?by=median|loss`
  - `WS /api/live` — pushes each completed round (vaping-style realtime)
  - static SPA embedded via `rust-embed` (no node toolchain required at runtime; keep frontend deliberately small — uPlot or hand-rolled canvas for smoke rendering)
- Client-side smoke rendering: median polyline colored by loss-bucket
  (SmokePing's classic green→purple→red scale), translucent band fills between
  sorted-ping quantiles for the smoke. Drag-to-zoom = refetch range (replaces
  SmokePing's AJAX navigator).
- Pages: overview grid per section, detail page with the 4 standard ranges,
  multi-target compare view, top-N charts page.

### Frontend styling (extracted from the DMS app, dms.neiam.org)
Match the DMS look and feel; status page and operator UI share it.
- **Stack**: Tailwind CSS + daisyUI 4 (OKLCH CSS-variable themes). All assets
  self-hosted — fits the rust-embed single-binary goal.
- **Themes** (`data-theme` on `<html>`, persisted in localStorage, "system"
  option follows `prefers-color-scheme`): custom **afterdark** (indigo/purple,
  system dark) and **her** (warm rose, system light), plus **clays** (earth
  orange), **sky** (blue), **stones** (neutral gray) and stock daisyUI black /
  lofi / forest. Raw variable blocks captured in `design/dms-themes.css` —
  reuse verbatim, keyed as `dabping:theme` instead of `dms:theme`. All themes
  share the same pale-yellow accent (`--a: 96.19% 0.058 95.62`) and shape
  tokens (`--rounded-box: 1rem`, `--rounded-btn: 0.5rem`).
- **Typography**: B612 Mono everywhere (self-hosted woff2, 400/700) with
  monospace fallback; brand wordmark = `font-mono font-bold tracking-[0.16em]
  text-accent` — render "DABPING" the same way.
- **Icons**: Font Awesome (self-hosted webfonts), `fas fa-fw` usage.
- **Component idiom**: navbar `bg-base-200 border-b border-base-300`; cards
  `rounded-box p-5 bg-base-200 border border-base-300`; secondary text
  `text-base-content/80`; `btn btn-ghost btn-sm` actions; daisyUI dropdowns
  for theme/user menus. Smoke-graph loss colors should be tinted per-theme via
  the daisyUI success/warning/error variables so graphs sit naturally in both
  dark and light themes (keep the classic green→purple→red scale as the
  values, mapped through `--su`/`--wa`/`--er` where sensible).

### Status page
- Components are declared in config and map to one or more targets (a
  component is healthy only if all its targets are):
  ```toml
  [status]
  title = "Example Corp Network Status"
  public = true                  # serve /status without auth
  history_days = 90

  [status.components.api]
  name = "Public API"
  targets = ["isp/api-lb"]
  group = "Core Services"
  degraded = "loss > 5% or median > 2x baseline"
  down = "loss == 100% for 3"    # 3 consecutive rounds
  ```
- State machine per component: `operational → degraded → partial_outage →
  major_outage`, evaluated on each completed round by the same matcher engine
  the alerts use — one evaluation path, two consumers.
- Incidents: a status transition past a configurable severity auto-opens an
  incident (timestamped); operators can append updates and resolve via the
  operator UI/API; history persisted in a small sqlite or flat JSONL store
  (separate from the round-robin metric store, since incidents are unbounded).
- Daily uptime % computed from the loss RRA, cached; rendered as the familiar
  90-bar strip per component.
- `/status` is server-rendered (minijinja template) + tiny JS for tooltips and
  auto-refresh — loads fast, works without the SPA, safe to expose publicly.
  Component names only by default; no internal hostnames leak.
- Routes: `GET /status`, `GET /api/status.json` (machine-readable, mirrors
  Statuspage's `status.json` shape so existing widgets/integrations can point
  at it), stretch: `GET /status/history.atom`.

### Alerting
- Config DSL closely modeled on SmokePing's:
  ```toml
  [alerts.bigloss]
  type = "loss"                 # loss | rtt
  pattern = ">10%,>10%,>10%"    # consecutive-round patterns, * wildcards, *N* "within N rounds"
  comment = "3 rounds with >10% loss"
  to = ["email:noc@example.com", "exec:/usr/local/bin/page.sh", "webhook:https://..."]
  ```
- Matcher engine over a sliding window of recent rounds; edge-triggered with
  clear notifications, optional `repeat_every`. Built-in matchers beyond raw
  patterns: consecutive-loss, median-ratio (rtt jumped vs baseline), avg-ratio.
- Notifiers: email (lettre), exec, generic webhook (covers Slack/ntfy/PagerDuty).

### Distributed mode (master/agent)
- `dabping agent --master https://host --name lon1 --secret ...`
- Agent pulls its target list from master (HTTP, shared-secret auth, mirrors
  SmokePing's slave protocol shape), runs rounds, POSTs results in batches with
  local buffering when master is unreachable.
- Master stores per-(target, agent) series; UI overlays agents on one graph.
- `nomasterpoll` equivalent: targets measured only by agents.
- Same binary for master and agent (subcommand, like smokeping `--slave`):
  one artifact to deploy, no version-skew between roles. If agent footprint
  ever matters, slim it via cargo features rather than a second bin —
  master-only deps (axum, rust-embed, minijinja, rusqlite, lettre) behind a
  default-on `master` feature; `cargo build --no-default-features` yields an
  agent-only build. Polish-milestone item; just keep web/store/alert module
  boundaries clean so it stays cheap.

### Config
TOML (serde), one file, hierarchical targets:

```toml
[database]
step = 300
pings = 20

[probes.icmp]
type = "icmp"
[probes.dns-cf]
type = "dns"
server = "1.1.1.1"

[targets.isp]
title = "Upstream"
  [targets.isp.gw]
  host = "192.0.2.1"
  probe = "icmp"
  alerts = ["bigloss"]
```

`dabping check-config` validates; SIGHUP/file-watch reloads without losing
in-flight rounds or stored data.

## Key dependencies
tokio, axum, socket2 (ICMP), hickory-resolver (DNS), reqwest (HTTP probe),
serde + toml, clap, tracing, lettre (mail), rust-embed (UI assets), memmap2
(store), minijinja (templates — house standard across projects), rusqlite
(incident history).
Deliberately no rrdtool/C dependencies.

## Milestones

1. ✅ **Walking skeleton** — config model, scheduler, ICMP probe, log emitter; `dabping once <host>` prints a round. *(usable as a CLI pinger)*
2. ✅ **Storage** — round-robin store, consolidation, fetch API + `dump` subcommand; tests against golden data.
3. ✅ **Web UI v1** — axum API + embedded SPA: tree, detail smoke graphs, 4 time ranges, live WebSocket. *(this is the "it's smokeping!" moment)*
4. ✅ **More probes** — TCP, DNS, HTTP, exec; multi-instance probe config; per-target overrides.
5. ✅ **Alerting** — pattern engine, email/exec/webhook notifiers, edge-trigger semantics.
6. ✅ **Status page** — component model + state derivation, public `/status` page with uptime bars, `status.json`, incident auto-open from alerts.
7. ✅ **Emitters** — Prometheus `/metrics`, Graphite, Influx line protocol.
8. ✅ **Distributed** — master/agent protocol, per-agent series, overlay graphs.
9. ✅ **Polish** — top-N charts, multi-host compare, hot reload, zoom/pan, systemd unit + Dockerfile + setcap docs. *(SmokePing RRD import deferred — would parse `rrdtool dump` XML if ever needed.)*

Each milestone leaves a working binary; 1–3 are the MVP. **All nine shipped.**

Remaining stretch ideas beyond the original plan: DYNAMIC targets (dynamic-IP
check-ins), per-agent alerting, status-page RSS/Atom, agent feature-flag slim
build, server-side PNG rendering for no-JS clients.

## Open questions
- Frontend: hand-rolled canvas vs uPlot vs server-side rendered PNGs (plotters) for environments without JS? Plan assumes client-side canvas; server-side PNG could be added behind the same fetch API later.
- Store granularity: file-per-target (simple, matches SmokePing ops habits) vs single DB file. Plan assumes file-per-target.
- IPv6 day one for ICMP (yes — same code path, `ICMPV6`).
