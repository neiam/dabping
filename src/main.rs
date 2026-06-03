mod agent;
mod alert;
mod config;
mod emit;
mod probe;
mod scheduler;
mod status;
mod store;
mod util;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::broadcast;

use config::{Config, IcmpConfig};
use emit::{Emitter, LogEmitter, fmt_opt, fmt_rtt};
use probe::icmp::IcmpProbe;
use store::{Cf, Store, StoreEmitter};
use util::{parse_range, unix_now};
use web::LiveEmitter;

#[derive(Parser)]
#[command(name = "dabping", version, about = "multi-target latency prober — smokeping, reimagined in rust")]
struct Cli {
    /// Path to the config file
    #[arg(short, long, global = true, default_value = "dabping.toml")]
    config: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon: schedule rounds for every configured target
    Run,
    /// Run as a measurement agent for a remote master
    Agent {
        /// Master base URL, e.g. http://master.example:8420
        #[arg(short, long)]
        master: String,
        /// Agent name (must match an [agents.<name>] on the master)
        #[arg(short, long)]
        name: String,
        /// Shared secret (or set DABPING_AGENT_SECRET)
        #[arg(short, long, env = "DABPING_AGENT_SECRET")]
        secret: String,
    },
    /// Validate the config file and print the flattened target list
    CheckConfig,
    /// Probe a single host once and print the round
    Once {
        host: String,
        /// Pings in the round
        #[arg(short, long, default_value_t = 20)]
        pings: u32,
        /// Gap between pings within the round (ms)
        #[arg(short, long, default_value_t = 300)]
        interval_ms: u64,
        /// Per-reply timeout (ms)
        #[arg(short, long, default_value_t = 1500)]
        timeout_ms: u64,
    },
    /// Print stored data for a target
    Dump {
        /// Target path, e.g. internet/cloudflare
        target: String,
        /// How far back to look (e.g. 3h, 30h, 10d, 360d)
        #[arg(short, long, default_value = "3h")]
        range: String,
        /// Consolidation function: average, min or max
        #[arg(long, default_value = "average")]
        cf: Cf,
        /// Max points to return; picks a coarser archive when exceeded (0 = no limit)
        #[arg(short, long, default_value_t = 60)]
        max_points: u64,
        /// Emit JSON (full smoke columns) instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Dev helper: write synthetic demo history for a target.
    /// Stop the daemon first — two processes must not map the same series.
    #[command(hide = true)]
    Seed {
        /// Target path, e.g. internet/cloudflare
        target: String,
        /// How much history to fabricate (e.g. 30h)
        #[arg(short, long, default_value = "30h")]
        span: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dabping=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run => run(&cli.config).await,
        Cmd::Agent { master, name, secret } => {
            agent::run(agent::AgentOpts { master, name, secret }).await
        }
        Cmd::CheckConfig => check_config(&cli.config),
        Cmd::Once { host, pings, interval_ms, timeout_ms } => {
            once(&host, pings, interval_ms, timeout_ms).await
        }
        Cmd::Dump { target, range, cf, max_points, json } => {
            dump(&cli.config, &target, &range, cf, max_points, json)
        }
        Cmd::Seed { target, span } => seed(&cli.config, &target, &span),
    }
}

/// Fabricate plausible rounds: a slow sine swell, a latency step, a couple
/// of loss events, one outage. Deterministic (no rand dep).
fn seed(config: &std::path::Path, target: &str, span: &str) -> Result<()> {
    let cfg = Config::load(config)?;
    let db = &cfg.database;
    let store = Store::open(&db.dir, db.step, db.pings, &db.rras)?;

    let now = unix_now();
    let from = now - parse_range(span)? as i64;
    // tiny deterministic PRNG (xorshift on the timestamp)
    let noise = |t: i64, salt: u64| {
        let mut x = t as u64 ^ (salt.wrapping_mul(0x9E3779B97F4A7C15));
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % 1000) as f64 / 1000.0
    };

    let mut rounds = 0u64;
    let mut t = from;
    while t <= now {
        let phase = (t - from) as f64 / (now - from) as f64;
        // base 12ms, swell to ~25ms mid-window, +8ms step in the last quarter
        let mut base = 12.0 + 13.0 * (phase * std::f64::consts::PI).sin();
        if phase > 0.75 {
            base += 8.0;
        }
        // loss events: 5-15% around 40%, total outage in a short band at 60%
        let lost = if (0.60..0.62).contains(&phase) {
            db.pings
        } else if (0.38..0.45).contains(&phase) {
            1 + (noise(t, 7) * 0.15 * db.pings as f64) as u32
        } else {
            0
        };
        let rtts: Vec<std::time::Duration> = (0..db.pings.saturating_sub(lost))
            .map(|i| {
                let jitter = noise(t, i as u64) * base * 0.4;
                std::time::Duration::from_secs_f64((base + jitter) / 1000.0)
            })
            .collect();
        let r = probe::RoundResult {
            target: target.to_string(),
            host: "seeded".into(),
            addr: "192.0.2.1".parse().expect("valid addr"),
            started: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t as u64),
            sent: db.pings,
            rtts,
        };
        store.record(&r)?;
        rounds += 1;
        t += db.step as i64;
    }
    println!("seeded {rounds} rounds for {target} over {span}");
    Ok(())
}

async fn run(config: &std::path::Path) -> Result<()> {
    loop {
        match run_once(config).await? {
            scheduler::Outcome::Quit => return Ok(()),
            scheduler::Outcome::Reload => {
                tracing::info!("reloading configuration");
                continue;
            }
        }
    }
}

async fn run_once(config: &std::path::Path) -> Result<scheduler::Outcome> {
    let cfg = Config::load(config)?;
    let db = &cfg.database;
    let store = Arc::new(Store::open(&db.dir, db.step, db.pings, &db.rras)?);
    let (live_tx, _) = broadcast::channel(256);

    let mut emitters: Vec<Box<dyn Emitter>> = vec![
        Box::new(LogEmitter),
        Box::new(StoreEmitter(store.clone())),
        Box::new(LiveEmitter(live_tx.clone())),
    ];
    if !cfg.web.enabled {
        emitters.pop(); // no UI, no live feed
    }
    if let Some(alerter) = alert::Alerter::new(&cfg)? {
        emitters.push(Box::new(alerter));
    }
    let status_engine = status::StatusEngine::new(&cfg)?.map(Arc::new);
    if let Some(eng) = &status_engine {
        emitters.push(Box::new(status::StatusEmitter(eng.clone())));
    }
    let mut prom_state = None;
    for (name, ec) in &cfg.emitters {
        match ec {
            config::EmitterConfig::Prometheus => {
                if !cfg.web.enabled {
                    tracing::warn!(emitter = %name, "prometheus emitter needs the web server; /metrics will not be served");
                }
                let st = Arc::new(emit::prometheus::PromState::default());
                emitters.push(Box::new(emit::prometheus::PromEmitter(st.clone())));
                prom_state = Some(st);
            }
            config::EmitterConfig::Graphite(g) => {
                emitters.push(Box::new(emit::graphite::GraphiteEmitter::spawn(g)));
            }
            config::EmitterConfig::Influx(i) => {
                emitters.push(Box::new(emit::influx::InfluxEmitter::spawn(i)));
            }
        }
    }

    let emitters = Arc::new(emitters);
    let web_cfg = cfg.web.clone();
    if !cfg.agents.is_empty() && !web_cfg.enabled {
        anyhow::bail!("[agents] are configured but the web server is disabled — they cannot connect");
    }
    let state = web::AppState {
        store,
        tree: Arc::new(web::build_tree(&cfg)),
        live: live_tx,
        status: status_engine,
        prom: prom_state,
        emitters: emitters.clone(),
        agents: Arc::new(web::build_agents(&cfg)?),
        targets: Arc::new(
            cfg.flatten_targets()?.into_iter().map(|t| (t.path, t.host)).collect(),
        ),
        step: db.step,
        pings: db.pings,
    };

    let config_path = Some(config.to_path_buf());
    if web_cfg.enabled {
        // scheduler returns on signals; the web server is dropped with the
        // select (and rebuilt on reload). A web bind failure takes the
        // daemon down — better loud than silently headless.
        tokio::select! {
            r = scheduler::run(cfg, emitters, config_path) => r,
            r = web::serve(web_cfg, state) => r.map(|()| scheduler::Outcome::Quit),
        }
    } else {
        scheduler::run(cfg, emitters, config_path).await
    }
}

fn check_config(path: &std::path::Path) -> Result<()> {
    let cfg = Config::load(path)?;
    let flat = cfg.flatten_targets()?;
    println!("{} OK: {} targets, step {}s, {} pings/round", path.display(), flat.len(), cfg.database.step, cfg.database.pings);
    for t in flat {
        println!("  {:<40} {:<30} probe={}", t.path, t.host, t.probe);
    }
    Ok(())
}

async fn once(host: &str, pings: u32, interval_ms: u64, timeout_ms: u64) -> Result<()> {
    let probe = IcmpProbe::new(&IcmpConfig {
        interval_ms,
        timeout_ms,
        ..IcmpConfig::default()
    });
    let r = probe.round("once", host, pings).await?;

    println!("dabping: {} pings to {} ({})", r.sent, r.host, r.addr);
    println!(
        "sent {}  recv {}  loss {:.1}%",
        r.sent,
        r.received(),
        r.loss_pct()
    );
    println!(
        "min {}  med {}  avg {}  max {}",
        fmt_opt(r.min()),
        fmt_opt(r.median()),
        fmt_opt(r.avg()),
        fmt_opt(r.max())
    );
    if !r.rtts.is_empty() {
        println!(
            "rtts: {}",
            r.rtts.iter().map(|d| fmt_rtt(*d)).collect::<Vec<_>>().join(" ")
        );
    }
    // nonzero exit on total loss, so `once` works in scripts
    if r.received() == 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn dump(
    config: &std::path::Path,
    target: &str,
    range: &str,
    cf: Cf,
    max_points: u64,
    json: bool,
) -> Result<()> {
    let cfg = Config::load(config)?;
    let db = &cfg.database;
    let store = Store::open(&db.dir, db.step, db.pings, &db.rras)?;

    let now = unix_now();
    let from = now - parse_range(range)? as i64;
    let fetched = store.fetch(target, cf, from, now, max_points)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&fetched)?);
        return Ok(());
    }

    println!(
        "{target}: {} points @ {}s ({cf:?})",
        fetched.points.len(),
        fetched.period
    );
    println!("{:<20} {:>7} {:>10} {:>10} {:>10}", "time", "loss%", "median", "min", "max");
    for p in &fetched.points {
        let t = jiff::Timestamp::from_second(p.ts)
            .context("timestamp out of range")?
            .to_zoned(jiff::tz::TimeZone::system());
        println!(
            "{:<20} {:>7} {:>10} {:>10} {:>10}",
            t.strftime("%Y-%m-%d %H:%M:%S"),
            fmt_pct(p.loss),
            fmt_ms(p.median),
            fmt_ms(p.smoke_min()),
            fmt_ms(p.smoke_max()),
        );
    }
    Ok(())
}

fn fmt_ms(secs: f64) -> String {
    if secs.is_nan() { "-".into() } else { format!("{:.2}ms", secs * 1000.0) }
}

fn fmt_pct(v: f64) -> String {
    if v.is_nan() { "-".into() } else { format!("{v:.1}") }
}
