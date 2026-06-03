//! DNS probe: the target host is a DNS *server*; RTT = query → any response.
//! Hand-rolled query packet (like the ICMP probe) — no resolver dependency.
//! NXDOMAIN, SERVFAIL etc. still count as replies: we measure the server,
//! not the zone.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, bail};
use tokio::net::UdpSocket;

use super::{RoundResult, pace, resolve};
use crate::config::DnsConfig;

pub struct DnsProbe {
    port: u16,
    lookup: Option<String>,
    qtype: u16,
    timeout: Duration,
    interval: Duration,
}

pub fn qtype_from_str(s: &str) -> Result<u16> {
    Ok(match s.to_ascii_uppercase().as_str() {
        "A" => 1,
        "NS" => 2,
        "SOA" => 6,
        "MX" => 15,
        "TXT" => 16,
        "AAAA" => 28,
        other => bail!("unsupported dns qtype {other:?} (A|NS|SOA|MX|TXT|AAAA)"),
    })
}

impl DnsProbe {
    pub fn new(cfg: &DnsConfig) -> Result<DnsProbe> {
        Ok(DnsProbe {
            port: cfg.port,
            lookup: cfg.lookup.clone(),
            qtype: qtype_from_str(&cfg.qtype)?,
            timeout: cfg.timeout(),
            interval: cfg.interval(),
        })
    }

    pub async fn round(
        &self,
        target_path: &str,
        host: &str,
        lookup_override: Option<&str>,
        pings: u32,
    ) -> Result<RoundResult> {
        let addr = resolve(host).await?;
        let server = SocketAddr::new(addr, self.port);
        // default lookup: the server's own name — any response (even
        // NXDOMAIN) proves the server answered
        let name = lookup_override.or(self.lookup.as_deref()).unwrap_or(host);
        let started = SystemTime::now();

        let bind: SocketAddr = match addr {
            IpAddr::V4(_) => "0.0.0.0:0".parse().expect("valid bind addr"),
            IpAddr::V6(_) => "[::]:0".parse().expect("valid bind addr"),
        };
        let sock = UdpSocket::bind(bind).await?;
        sock.connect(server).await?;

        let mut rtts = Vec::with_capacity(pings as usize);
        let mut buf = [0u8; 1500];
        for seq in 0..pings as u16 {
            let id = seq.wrapping_add(0x1d0b); // arbitrary, varies per ping
            let query = build_query(id, name, self.qtype)?;
            let t0 = Instant::now();
            sock.send(&query).await?;

            let deadline = t0 + self.timeout;
            loop {
                let now = Instant::now();
                if now >= deadline {
                    break; // loss
                }
                match tokio::time::timeout(deadline - now, sock.recv(&mut buf)).await {
                    Ok(Ok(n)) if is_response(&buf[..n], id) => {
                        rtts.push(t0.elapsed());
                        break;
                    }
                    Ok(Ok(_)) => continue,   // stale/foreign packet
                    Ok(Err(_)) | Err(_) => break, // socket error or timeout: loss
                }
            }
            pace(t0, self.interval, seq as u32 + 1 == pings).await;
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

/// Minimal RFC 1035 query: header + one question, recursion desired.
fn build_query(id: u16, name: &str, qtype: u16) -> Result<Vec<u8>> {
    let mut pkt = Vec::with_capacity(64);
    pkt.extend(id.to_be_bytes());
    pkt.extend([0x01, 0x00]); // RD
    pkt.extend([0, 1, 0, 0, 0, 0, 0, 0]); // QDCOUNT=1
    for label in name.trim_end_matches('.').split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            bail!("invalid dns lookup name {name:?}");
        }
        pkt.push(bytes.len() as u8);
        pkt.extend(bytes);
    }
    pkt.push(0); // root
    pkt.extend(qtype.to_be_bytes());
    pkt.extend(1u16.to_be_bytes()); // IN
    Ok(pkt)
}

/// A reply is ours if the ID matches and the QR bit is set.
fn is_response(pkt: &[u8], id: u16) -> bool {
    pkt.len() >= 12 && pkt[0..2] == id.to_be_bytes() && pkt[2] & 0x80 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_valid_query() {
        let q = build_query(0x1234, "example.com", 1).unwrap();
        assert_eq!(&q[0..2], &[0x12, 0x34]);
        assert_eq!(q[2], 0x01); // RD
        // 7"example"3"com"0
        assert_eq!(q[12], 7);
        assert_eq!(&q[13..20], b"example");
        assert_eq!(q[20], 3);
        assert_eq!(*q.last().unwrap(), 1); // IN
        assert!(build_query(1, "", 1).is_err());
        assert!(build_query(1, &"x".repeat(64), 1).is_err());
    }

    #[test]
    fn matches_responses() {
        let mut resp = build_query(7, "example.com", 1).unwrap();
        assert!(!is_response(&resp, 7)); // QR not set yet
        resp[2] |= 0x80;
        assert!(is_response(&resp, 7));
        assert!(!is_response(&resp, 8)); // wrong id
        assert!(!is_response(&[0u8; 4], 7)); // too short
    }

    #[tokio::test]
    async fn round_against_mock_server() {
        // tiny DNS "server": echo the query back with QR set
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, peer)) = server.recv_from(&mut buf).await else { return };
                buf[2] |= 0x80;
                let _ = server.send_to(&buf[..n], peer).await;
            }
        });

        let probe = DnsProbe {
            port: server_addr.port(),
            lookup: Some("example.com".into()),
            qtype: 1,
            timeout: Duration::from_millis(500),
            interval: Duration::from_millis(10),
        };
        let r = probe.round("t", "127.0.0.1", None, 5).await.unwrap();
        assert_eq!(r.received(), 5);
        assert_eq!(r.loss_pct(), 0.0);
    }

    #[tokio::test]
    async fn round_against_silence_is_loss() {
        let dead = UdpSocket::bind("127.0.0.1:0").await.unwrap(); // never answers
        let probe = DnsProbe {
            port: dead.local_addr().unwrap().port(),
            lookup: None,
            qtype: 1,
            timeout: Duration::from_millis(50),
            interval: Duration::from_millis(5),
        };
        let r = probe.round("t", "127.0.0.1", None, 3).await.unwrap();
        assert_eq!(r.received(), 0);
        assert_eq!(r.loss_pct(), 100.0);
    }
}
