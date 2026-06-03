use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

use crate::config::{Config, FlatTarget};
use crate::emit::Emitter;
use crate::probe::ProbeInstance;

/// Why the scheduler stopped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    /// ctrl-c / SIGTERM — shut down.
    Quit,
    /// SIGHUP — the caller should rebuild config and run again.
    Reload,
}

/// Run rounds for every configured target until ctrl-c / SIGTERM / SIGHUP.
/// Targets marked nomasterpoll are left to their agents.
pub async fn run(
    cfg: Config,
    emitters: Arc<Vec<Box<dyn Emitter>>>,
    config_path: Option<std::path::PathBuf>,
) -> Result<Outcome> {
    let probes: Arc<HashMap<String, ProbeInstance>> = Arc::new(
        cfg.probes
            .iter()
            .map(|(name, pc)| Ok((name.clone(), ProbeInstance::from_config(pc)?)))
            .collect::<Result<_>>()?,
    );
    let targets: Vec<FlatTarget> =
        cfg.flatten_targets()?.into_iter().filter(|t| !t.nomasterpoll).collect();
    run_targets(targets, probes, cfg.database.step, cfg.database.pings, emitters, config_path)
        .await
}

/// The scheduling core, shared by master and agent modes.
pub async fn run_targets(
    targets: Vec<FlatTarget>,
    probes: Arc<HashMap<String, ProbeInstance>>,
    step_secs: u64,
    pings: u32,
    emitters: Arc<Vec<Box<dyn Emitter>>>,
    config_path: Option<std::path::PathBuf>,
) -> Result<Outcome> {
    let step = Duration::from_secs(step_secs);

    tracing::info!(targets = targets.len(), step = step_secs, pings, "scheduler starting");

    let mut tasks = JoinSet::new();
    for t in targets {
        let probes = probes.clone();
        let emitters = emitters.clone();
        tasks.spawn(async move {
            // deterministic jitter: spread targets across the step window so
            // rounds don't all fire at once (SmokePing's `offset = random`)
            let offset = Duration::from_millis(stable_hash(&t.path) % step.as_millis() as u64);
            tokio::time::sleep(offset).await;

            let mut tick = tokio::time::interval(step);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            let probe = &probes[&t.probe];
            loop {
                tick.tick().await;
                match probe.round(&t, pings).await {
                    Ok(round) => {
                        for e in emitters.iter() {
                            e.emit(&round);
                        }
                    }
                    Err(e) => tracing::warn!(path = %t.path, host = %t.host, error = %e, "round failed"),
                }
            }
        });
    }

    let outcome = wait_for_signal(config_path.as_deref()).await;
    tracing::info!(?outcome, "scheduler stopping");
    tasks.shutdown().await;
    Ok(outcome)
}

/// Quit on ctrl-c/SIGTERM. On SIGHUP, validate the new config first —
/// a broken file logs an error and the old config keeps running.
/// Agents (no config path) treat SIGHUP as "re-fetch the assignment".
async fn wait_for_signal(config_path: Option<&std::path::Path>) -> Outcome {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("cannot install SIGTERM handler");
    let mut hup = signal(SignalKind::hangup()).expect("cannot install SIGHUP handler");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Outcome::Quit,
            _ = term.recv() => return Outcome::Quit,
            _ = hup.recv() => {
                let Some(path) = config_path else { return Outcome::Reload };
                match Config::load(path) {
                    Ok(_) => return Outcome::Reload,
                    Err(e) => {
                        tracing::error!(error = %e, "SIGHUP: new config is invalid; keeping the old one");
                    }
                }
            }
        }
    }
}

fn stable_hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
