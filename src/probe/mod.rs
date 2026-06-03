pub mod dns;
pub mod exec;
pub mod http;
pub mod icmp;
pub mod tcp;

use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;

use crate::config::{FlatTarget, ProbeConfig};

/// Result of one measurement round against a single target.
#[derive(Debug, Clone)]
pub struct RoundResult {
    /// Slash-separated path of the target in the config tree.
    pub target: String,
    pub host: String,
    pub addr: IpAddr,
    /// Wall-clock time the round started.
    pub started: SystemTime,
    pub sent: u32,
    /// RTTs of the replies that came back, in send order.
    pub rtts: Vec<Duration>,
}

impl RoundResult {
    pub fn received(&self) -> u32 {
        self.rtts.len() as u32
    }

    pub fn loss_pct(&self) -> f64 {
        if self.sent == 0 {
            return 0.0;
        }
        100.0 * (self.sent - self.received()) as f64 / self.sent as f64
    }

    /// RTTs sorted ascending — the layout the smoke store keeps.
    pub fn sorted_rtts(&self) -> Vec<Duration> {
        let mut v = self.rtts.clone();
        v.sort_unstable();
        v
    }

    pub fn min(&self) -> Option<Duration> {
        self.rtts.iter().min().copied()
    }

    pub fn max(&self) -> Option<Duration> {
        self.rtts.iter().max().copied()
    }

    pub fn avg(&self) -> Option<Duration> {
        if self.rtts.is_empty() {
            return None;
        }
        Some(self.rtts.iter().sum::<Duration>() / self.rtts.len() as u32)
    }

    pub fn median(&self) -> Option<Duration> {
        let sorted = self.sorted_rtts();
        match sorted.len() {
            0 => None,
            n if n % 2 == 1 => Some(sorted[n / 2]),
            n => Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2),
        }
    }
}

/// A configured probe instance, ready to run rounds.
///
/// Enum dispatch: probes are a closed in-crate set, and this keeps the
/// async story simple (no boxed futures).
pub enum ProbeInstance {
    Icmp(icmp::IcmpProbe),
    Tcp(tcp::TcpProbe),
    Dns(dns::DnsProbe),
    Http(http::HttpProbe),
    Exec(exec::ExecProbe),
}

impl ProbeInstance {
    pub fn from_config(cfg: &ProbeConfig) -> Result<ProbeInstance> {
        Ok(match cfg {
            ProbeConfig::Icmp(c) => ProbeInstance::Icmp(icmp::IcmpProbe::new(c)),
            ProbeConfig::Tcp(c) => ProbeInstance::Tcp(tcp::TcpProbe::new(c)),
            ProbeConfig::Dns(c) => ProbeInstance::Dns(dns::DnsProbe::new(c)?),
            ProbeConfig::Http(c) => ProbeInstance::Http(http::HttpProbe::new(c)?),
            ProbeConfig::Exec(c) => ProbeInstance::Exec(exec::ExecProbe::new(c)),
        })
    }

    pub async fn round(&self, t: &FlatTarget, pings: u32) -> Result<RoundResult> {
        match self {
            ProbeInstance::Icmp(p) => p.round(&t.path, &t.host, pings).await,
            ProbeInstance::Tcp(p) => p.round(&t.path, &t.host, t.port, pings).await,
            ProbeInstance::Dns(p) => p.round(&t.path, &t.host, t.lookup.as_deref(), pings).await,
            ProbeInstance::Http(p) => p.round(&t.path, &t.host, pings).await,
            ProbeInstance::Exec(p) => p.round(&t.path, &t.host, pings).await,
        }
    }
}

/// Sleep out the rest of the inter-ping gap (skipped after the last ping).
pub(crate) async fn pace(t0: Instant, interval: Duration, last: bool) {
    let elapsed = t0.elapsed();
    if !last && elapsed < interval {
        tokio::time::sleep(interval - elapsed).await;
    }
}

/// Resolve a hostname (or IP literal) to a single address for this round.
/// Resolution happens per round so DNS changes are picked up, like
/// SmokePing re-invoking fping.
pub async fn resolve(host: &str) -> Result<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }
    let addr = tokio::net::lookup_host((host, 0u16))
        .await
        .map_err(|e| anyhow::anyhow!("cannot resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("no addresses for {host}"))?;
    Ok(addr.ip())
}
