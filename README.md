# dabping

Network latency monitoring in a single static binary — a Rust reimplementation
of [SmokePing](https://oss.oetiker.ch/smokeping/) crossed with
[vaping](https://github.com/20c/vaping), plus an Atlassian-Statuspage-style
public status page. No Perl, no RRDtool, no node toolchain.

**Status: v0.1.0 — feature-complete.** All nine milestones from `PLAN.md` are
implemented, unit-tested (59 tests), and verified live — including a
real-world run against dms.neiam.org (icmp ~11ms / tls ~10ms / https ~45ms,
which is exactly the layered story the probe variety exists to tell).

## Features

- **Smoke graphs** — N pings per round (default 20 every 300s); the full RTT
  distribution is stored and rendered as the classic smoke band with a
  loss-colored median line.
- **Probes** — `icmp` (native v4/v6, unprivileged ping sockets), `tcp`
  (handshake time), `dns` (query a server directly, hand-rolled RFC 1035),
  `http(s)` (full fetch, connection reuse disabled so every ping pays
  connect+TLS), `phoenix` (Elixir/Phoenix channel heartbeats over one
  WebSocket per round — app-level BEAM latency; `scripts/phx-ping.py` is the
  standalone equivalent), `exec` (anything that prints fping `-C` output).
  Multi-instance probes; `probe`/`port`/`lookup` inherit down the target tree.
- **RRD-style storage** — fixed-size memory-mapped series files with
  AVERAGE/MIN/MAX consolidation and xff; SmokePing's default retention table.
- **Web UI** — target tree, section overviews, detail pages at
  3h/30h/10d/360d, drag-to-zoom (double-click resets), live WebSocket
  updates, top-N charts (slowest/lossiest), multi-host compare, 5 OKLCH
  themes (DMS house style, B612 Mono), all embedded.
- **Alerts** — SmokePing's pattern DSL (`>10%,>10%,>10%`, `*N*` windows,
  `==U` unknowns) over loss% or median ms; edge-triggered with clears and
  optional `repeat_every`; `log:` / `exec:` / `webhook:` / `email:` notifiers.
- **Status page** — `/status`: named components mapped to targets, severity
  derived from the same pattern engine (operational → degraded → partial →
  major), auto-opened/auto-resolved incidents persisted as a JSONL event log,
  90-day uptime bars, Statuspage-compatible `/api/status.json`, token-gated
  manual incident updates.
- **Emitters** — Prometheus `/metrics`, Graphite plaintext, InfluxDB line
  protocol (v1/v2), all backpressure-safe (drop + warn, never block probing).
- **Distributed** — `dabping agent` (same binary) pulls its assignment from
  the master and pushes results back, buffering up to 10k rounds while
  offline; per-agent series (`path@agent`) drawn as dashed overlays;
  `nomasterpoll` for agent-only targets.
- **Ops** — `SIGHUP` hot reload (new config validated first; broken file
  keeps the old one running; agents re-fetch their assignment), hardened
  systemd unit, Dockerfile.

## Quickstart

```sh
cargo build --release
./target/release/dabping once 1.1.1.1          # one round, no config needed
./target/release/dabping check-config          # validate dabping.toml
./target/release/dabping run                   # daemon + web UI on :8420
```

Minimal config:

```toml
[database]
step = 300
pings = 20

[targets.internet]
title = "Internet"
  [targets.internet.cloudflare]
  host = "1.1.1.1"
```

The shipped `dabping.toml` documents every section in commented form
(probes, emitters, alerts, smtp, status page, agents).

## Subcommands

| command | |
|---|---|
| `run` | the daemon: scheduler + web UI/API |
| `agent -m URL -n NAME -s SECRET` | remote measurement agent (`DABPING_AGENT_SECRET` works too) |
| `once <host>` | a single ICMP round, printed; exits 1 on total loss |
| `check-config` | validate and print the flattened target list |
| `gen-agent <name> [-m URL]` | mint an agent secret + the master/agent config snippets |
| `dump <target> -r 3h [--cf max] [--json]` | print stored data |
| `seed <target> --span 30h` | *(hidden)* synthetic demo history for UI work — stop the daemon first; it won't overwrite data newer than what it writes |

## ICMP privileges

Unprivileged ping sockets are tried first (`sysctl net.ipv4.ping_group_range`;
most distros allow them). Raw-socket fallback needs:

```sh
sudo setcap cap_net_raw+ep $(command -v dabping)
```

or `AmbientCapabilities=CAP_NET_RAW` — see `deploy/dabping.service`.

## Containers

```sh
podman build -t dabping .
podman run -d --sysctl net.ipv4.ping_group_range="0 65535" \
  -v ./dabping.toml:/etc/dabping/dabping.toml:ro -v dabping-data:/data \
  -p 8420:8420 dabping
```

The image runs non-root and uses unprivileged **ping sockets** — the
`--sysctl` is required for ICMP (same flag on docker; use `0 2147483647`
there, rootless podman needs the range within its mapped gids). There is
deliberately no setcap in the image: file capabilities break `exec` under
rootless podman. CI builds amd64+arm64 images to GHCR
(`.github/workflows/container.yml`) on pushes to main and `v*` tags.

## Multi-region agent deployment

The reference topology: master at home, agents on VPSes in par/ams/nyc/sea.
Agents are **stateless** (no config file, no data dir) — they pull their
assignment from the master at startup and buffer up to ~10k rounds (days at
step 300) through master outages.

Master config:

```toml
[agents.par]
secret = "per-agent-secret-1"   # one distinct secret per agent
[agents.ams]
secret = "per-agent-secret-2"
[agents.nyc]
secret = "per-agent-secret-3"
[agents.sea]
secret = "per-agent-secret-4"

[targets.internet]
title = "Internet"
agents = ["par", "ams", "nyc", "sea"]   # inherited by every child
  [targets.internet.cloudflare]
  host = "1.1.1.1"

[targets.regional.eu-thing]
host = "some-eu-host.example"
agents = ["par", "ams"]                 # only the EU vantage points
nomasterpoll = true                     # master's own view not wanted
```

Each leaf then has the master's series plus `path@par`, `path@ams`, … —
rendered as dashed per-agent overlays with a legend.

On each VPS, either the bare-metal unit (`deploy/dabping-agent.service`) or
the podman quadlet (`deploy/dabping-agent.container`); only `--name` and the
secret differ per host. The secret lives in `/etc/dabping/agent.env` as
`DABPING_AGENT_SECRET=…`.

Operational notes:

- **TLS**: dabping speaks plain HTTP — front the master with a reverse proxy
  (`https://dab.example.org` → `127.0.0.1:8420`) so secrets cross the
  internet inside TLS. Agents handle `https://` master URLs natively.
- **Config changes**: edit the master config, `systemctl reload dabping` on
  the master, then `systemctl reload dabping-agent` on each VPS — agents
  re-fetch their assignment on SIGHUP, no restart needed.
- **Sanity check from a VPS**:
  `curl -H 'X-Dabping-Agent: par' -H 'X-Dabping-Secret: …' https://dab.example.org/api/agent/config`
  shows exactly what that agent will be told to do.
- The overlay palette has 6 distinct colors; ≤6 agents per target renders
  cleanly.
- **Known gap**: alerts and status components evaluate the master's series
  only — "down from par specifically" does not page yet (see ideas list).

---

## Architecture (for picking this back up)

One binary crate, enum dispatch over closed sets, everything async on tokio.

```
src/
  main.rs            clap CLI; run() loops on Outcome::Reload (SIGHUP)
  config.rs          serde TOML model; tree flattening with inheritance
                     (probe/port/lookup/alerts/agents/nomasterpoll);
                     ALL validation happens at load — check-config catches
                     bad patterns, unknown refs, missing ports, etc.
  scheduler.rs       per-target tasks, jittered across the step window;
                     run_targets() core shared with agent mode;
                     wait_for_signal() → Outcome::{Quit,Reload}
  probe/             ProbeInstance enum (icmp/tcp/dns/http/exec);
                     RoundResult { sent, rtts } is the universal currency;
                     icmp.rs has the DGRAM-then-RAW socket dance
  store/             the RRD replacement
    series.rs        on-disk format (documented at the top of the file):
                     slot index = pure fn of time, torn rows self-invalidate
                     via their ts field, consolidation accumulators persist
                     in the header so windows survive restarts
    mod.rs           Store::record/fetch; archive selection by coverage +
                     point budget with stride fallback; target path charset
                     ('@' reserved for agent series)
  emit/              Emitter trait (sync fn emit(&RoundResult)); Log, Store,
                     Live(ws), Alerter, Status, Prom, Graphite, Influx all
                     hang off the same Vec<Box<dyn Emitter>> — agent-pushed
                     rounds re-enter this same pipeline on the master
  alert/             pattern.rs: the DSL (right-anchored, backtracking *N*);
                     mod.rs: per-(alert,target) edge-trigger state machine;
                     notify.rs: dispatch fan-out
  status/            components → severity via the same Pattern type;
                     incidents = replayable JSONL event log (no sqlite)
  agent/             wire types (Assignment/WireRound), agent run loop,
                     PushEmitter with offline ring buffer
  web/               axum: JSON API + ws + /metrics + /status (minijinja,
                     templates/) + agent endpoints; assets/ is the SPA
                     (vanilla JS, canvas smoke renderer in app.js —
                     drawSmoke/drawAgentLine/drawCompare)
```

Key decisions and why:
- **No rrdtool/C deps** — own mmap format, same DS shape as SmokePing's RRDs
  (loss, median, sorted ping_1..N) so smoke renders at every resolution.
- **Enum dispatch for probes/emitters** — closed in-crate sets; no
  async-trait boxing. Add a probe = new variant + config struct + one match
  arm in `probe/mod.rs` + validation-by-construction in `from_config`.
- **minijinja for templates** (house standard); frontend toolchain ported
  from DMS — Tailwind + daisyUI with the DMS theme definitions in
  `assets/tailwind.config.js`, compiled to `src/web/assets/app.css`
  (committed, since rust-embed bakes it in at cargo build time).
- **Same binary for master/agent**; slimming via cargo features is a noted
  option in PLAN.md but not done.
- **JSONL for incidents** — append-only, crash-safe, replayed at startup.

## Development workflow

```sh
cargo test                                   # 59 tests, all hermetic
dabping seed net/cf --span 30h               # fake history incl. loss events
RUST_LOG=dabping=debug dabping run           # request-level logging
```

- Debug builds serve `src/web/assets/` **live from disk** (rust-embed);
  release builds embed them. Edit JS, refresh, no rebuild.
- CSS is compiled with Tailwind/daisyUI (setup ported from DMS):
  `just deps` once, then `just assets` (one-shot) or `just watch` during UI
  work — input is `assets/css/style.css`, markup uses daisyUI components;
  the compiled `src/web/assets/app.css` is committed. The Dockerfile
  rebuilds it in a node stage.
- **Headless UI verification on this machine**: chromium headless is broken
  (loads nothing, any flags); use
  `firefox --no-remote --headless --profile $(mktemp -d) --screenshot out.png
  'http://127.0.0.1:8420/?snap#/t/path'` — `?snap` holds the load event via
  `/api/holdload` until the graphs are drawn, `127.0.0.1` not `localhost`.
- JS gotcha that already bit once: bare `isFinite(null) === true`; always
  `Number.isFinite` (JSON NaN arrives as `null`).
- Changing `step`/`pings`/`rra` invalidates existing series files on purpose
  (clear error at startup) — move the data dir aside.
- A round is recorded only if newer than the series' last update (out-of-order
  pushes and double-seeds are silently skipped).

## Not done / ideas

- SmokePing `.rrd` import (would parse `rrdtool dump` XML)
- DYNAMIC targets (dynamic-IP check-ins), per-agent alerting
- status-page RSS/Atom; agent-slim build behind a cargo feature
- server-side PNG graphs for no-JS clients; auth for the operator UI
  (currently: bind to localhost or front with a reverse proxy)

`PLAN.md` has the original design and the milestone-by-milestone history.
