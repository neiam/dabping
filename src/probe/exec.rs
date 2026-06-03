//! Exec probe: run an external command once per round and parse RTTs (ms)
//! from its output. The escape hatch for everything SmokePing does with
//! exotic probes.
//!
//! The command gets %host% and %pings% substituted, and must print one
//! token per ping: a number (RTT in ms) or `-` for a lost ping — exactly
//! fping's `-C` format, so `command = "fping -C %pings% -q %host%"` works
//! out of the box (fping prints to stderr; both streams are parsed).

use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use tokio::process::Command;

use super::{RoundResult, resolve};
use crate::config::ExecConfig;

pub struct ExecProbe {
    command: String,
    round_timeout: Duration,
}

impl ExecProbe {
    pub fn new(cfg: &ExecConfig) -> ExecProbe {
        ExecProbe {
            command: cfg.command.clone(),
            round_timeout: Duration::from_millis(cfg.round_timeout_ms),
        }
    }

    pub async fn round(&self, target_path: &str, host: &str, pings: u32) -> Result<RoundResult> {
        let cmd = self
            .command
            .replace("%host%", host)
            .replace("%pings%", &pings.to_string());
        let started = SystemTime::now();
        let addr = resolve(host).await.unwrap_or(std::net::Ipv4Addr::UNSPECIFIED.into());

        let output = tokio::time::timeout(
            self.round_timeout,
            Command::new("sh").arg("-c").arg(&cmd).kill_on_drop(true).output(),
        )
        .await
        .with_context(|| format!("exec probe timed out after {:?}: {cmd}", self.round_timeout))?
        .with_context(|| format!("exec probe failed to run: {cmd}"))?;

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push(' ');
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let rtts = parse_rtts(&text, pings as usize);

        if rtts.is_empty() && !output.status.success() {
            tracing::warn!(path = %target_path, %cmd, status = %output.status, "exec probe command failed");
        }

        Ok(RoundResult {
            target: target_path.to_string(),
            host: host.to_string(),
            addr,
            started,
            sent: pings,
            rtts,
        })
    }
}

/// Pull up to `max` RTT tokens (ms) out of the command output. Tokens that
/// aren't numbers (hostnames, colons, `-`) are ignored.
fn parse_rtts(text: &str, max: usize) -> Vec<Duration> {
    text.split_whitespace()
        .filter_map(|tok| tok.parse::<f64>().ok())
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
        .take(max)
        .map(|ms| Duration::from_secs_f64(ms / 1000.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fping_style_output() {
        let rtts = parse_rtts("1.1.1.1 : 9.71 10.2 - 11.0", 4);
        assert_eq!(rtts.len(), 3);
        assert_eq!(rtts[0], Duration::from_micros(9710));
        // never more than `max`
        assert_eq!(parse_rtts("1 2 3 4 5", 3).len(), 3);
        // garbage-only output = total loss
        assert!(parse_rtts("host unreachable", 4).is_empty());
    }

    #[tokio::test]
    async fn round_runs_command() {
        let probe = ExecProbe {
            command: "echo '%host% : 10.5 20.25 -'".into(),
            round_timeout: Duration::from_secs(5),
        };
        let r = probe.round("t", "127.0.0.1", 3).await.unwrap();
        assert_eq!(r.sent, 3);
        assert_eq!(r.received(), 2);
        assert_eq!(r.median().unwrap(), Duration::from_secs_f64(0.015375));
    }

    #[tokio::test]
    async fn failing_command_is_total_loss() {
        let probe = ExecProbe {
            command: "exit 3".into(),
            round_timeout: Duration::from_secs(5),
        };
        let r = probe.round("t", "127.0.0.1", 3).await.unwrap();
        assert_eq!(r.received(), 0);
    }
}
