//! TCP connect probe: RTT = time to a completed three-way handshake.
//! SmokePing's TCPPing equivalent.

use std::net::SocketAddr;
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result};
use tokio::net::TcpStream;

use super::{RoundResult, pace, resolve};
use crate::config::TcpConfig;

pub struct TcpProbe {
    port: Option<u16>,
    timeout: std::time::Duration,
    interval: std::time::Duration,
}

impl TcpProbe {
    pub fn new(cfg: &TcpConfig) -> TcpProbe {
        TcpProbe { port: cfg.port, timeout: cfg.timeout(), interval: cfg.interval() }
    }

    pub async fn round(
        &self,
        target_path: &str,
        host: &str,
        port_override: Option<u16>,
        pings: u32,
    ) -> Result<RoundResult> {
        let port = port_override
            .or(self.port)
            .with_context(|| format!("target {target_path}: tcp probe needs a port (set it on the probe or the target)"))?;
        let addr = resolve(host).await?;
        let sa = SocketAddr::new(addr, port);
        let started = SystemTime::now();

        let mut rtts = Vec::with_capacity(pings as usize);
        for i in 0..pings {
            let t0 = Instant::now();
            match tokio::time::timeout(self.timeout, TcpStream::connect(sa)).await {
                Ok(Ok(stream)) => {
                    rtts.push(t0.elapsed());
                    drop(stream); // immediate close; we only wanted the handshake
                }
                // refused / unreachable / timeout — all count as loss
                Ok(Err(_)) | Err(_) => {}
            }
            pace(t0, self.interval, i + 1 == pings).await;
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
