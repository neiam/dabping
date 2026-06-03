# dabping

Network latency monitoring in a single static binary — a Rust reimplementation
of [SmokePing](https://oss.oetiker.ch/smokeping/) crossed with
[vaping](https://github.com/20c/vaping). Smoke graphs, hierarchical targets,
pattern alerting, a Statuspage-style public status page, TSDB emitters, and
distributed agents. No Perl, no RRDtool, no node toolchain.

## Features

- **Smoke graphs** — N pings per round (default 20 every 300s); the full RTT
  distribution is stored and rendered as the classic smoke band with a
  loss-colored median line.
- **Probes** — icmp (native v4/v6, unprivileged ping sockets), tcp (handshake
  time), dns (query a server directly), http(s) (full fetch, no connection
  reuse), exec (anything that prints fping-style output).
- **RRD-style storage** — fixed-size memory-mapped series files with
  AVERAGE/MIN/MAX consolidation; SmokePing's default retention table.
- **Web UI** — target tree, per-target detail at 3h/30h/10d/360d,
  drag-to-zoom (double-click to reset), live WebSocket updates, top-N charts,
  multi-host compare, theme switcher.
- **Alerts** — SmokePing's pattern DSL (`>10%,>10%,>10%`, `*N*` windows,
  `==U`), edge-triggered with clears and optional repeats; log / exec /
  webhook / email notifiers.
- **Status page** — `/status`: named components over targets, auto-opened
  incidents persisted as JSONL, 90-day uptime bars, Statuspage-compatible
  `/api/status.json`.
- **Emitters** — Prometheus `/metrics`, Graphite plaintext, InfluxDB line
  protocol.
- **Distributed** — `dabping agent` on remote hosts pulls its assignment from
  the master and pushes results back (buffered while offline); per-agent
  series overlay on the graphs. `nomasterpoll` for agent-only targets.

## Quickstart

```sh
cargo build --release
./target/release/dabping once 1.1.1.1          # one round, no config needed
./target/release/dabping check-config          # validate dabping.toml
./target/release/dabping run                   # daemon + web UI on :8420
```

Minimal `dabping.toml`:

```toml
[database]
step = 300        # seconds between rounds
pings = 20        # measurements per round

[targets.internet]
title = "Internet"
  [targets.internet.cloudflare]
  host = "1.1.1.1"
  [targets.internet.quad9]
  host = "9.9.9.9"
```

The shipped `dabping.toml` documents every section (probes, alerts, smtp,
status page, emitters, agents) in commented form.

## Subcommands

| command | |
|---|---|
| `run` | the daemon: scheduler + web UI/API |
| `agent -m URL -n NAME -s SECRET` | run as a remote measurement agent |
| `once <host>` | a single ICMP round, printed; exits 1 on total loss |
| `check-config` | validate and print the flattened target list |
| `dump <target> -r 3h [--cf max] [--json]` | print stored data |

`kill -HUP` reloads the config (validated first — a broken file keeps the old
config running). On agents, HUP re-fetches the assignment from the master.

## ICMP privileges

dabping tries an unprivileged ping socket first, controlled by
`sysctl net.ipv4.ping_group_range` (most distros allow all groups; systemd
sets it). Fallback is a raw socket:

```sh
sudo setcap cap_net_raw+ep $(command -v dabping)
```

or `AmbientCapabilities=CAP_NET_RAW` in the unit — see
`deploy/dabping.service`, which also handles hardening and `ExecReload`.

## Docker

```sh
docker build -t dabping .
docker run -v ./dabping.toml:/etc/dabping/dabping.toml -v dabping-data:/data -p 8420:8420 dabping
```

## Distributed agents

Master:

```toml
[agents.lon1]
secret = "change-me"

[targets.internet.cloudflare]
host = "1.1.1.1"
agents = ["lon1"]        # lon1 measures it too → series "…/cloudflare@lon1"
# nomasterpoll = true    # only the agents measure it
```

Agent (same binary):

```sh
dabping agent --master http://master:8420 --name lon1 --secret change-me
# secret also via DABPING_AGENT_SECRET
```

## Development

```sh
cargo test
dabping seed internet/cloudflare --span 30h   # synthetic demo history (stop the daemon first)
```

The UI is plain HTML/CSS/JS embedded at build time (live from disk in debug
builds). `?snap` on any UI URL holds the page's load event until graphs are
drawn — useful for headless screenshots.

See `PLAN.md` for the architecture and milestone history.
