//! Native ICMP echo probe.
//!
//! Tries an unprivileged ping socket (`SOCK_DGRAM`/`IPPROTO_ICMP`, gated by
//! the `net.ipv4.ping_group_range` sysctl, which covers v6 too) and falls
//! back to a raw socket (needs `CAP_NET_RAW`).
//!
//! TODO(milestone 8): one shared socket + dispatcher task per probe
//! instance instead of a socket per round, per PLAN.md.

use std::mem::MaybeUninit;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;

use super::RoundResult;
use crate::config::IcmpConfig;

const ECHO_REQUEST_V4: u8 = 8;
const ECHO_REPLY_V4: u8 = 0;
const ECHO_REQUEST_V6: u8 = 128;
const ECHO_REPLY_V6: u8 = 129;

pub struct IcmpProbe {
    payload_size: usize,
    timeout: Duration,
    interval: Duration,
}

impl IcmpProbe {
    pub fn new(cfg: &IcmpConfig) -> IcmpProbe {
        IcmpProbe {
            payload_size: cfg.payload_size,
            timeout: cfg.timeout(),
            interval: cfg.interval(),
        }
    }

    pub async fn round(&self, target_path: &str, host: &str, pings: u32) -> Result<RoundResult> {
        let addr = super::resolve(host).await?;
        let started = SystemTime::now();

        let (socket, raw) = open_socket(addr)?;
        socket.set_nonblocking(true)?;
        socket
            .connect(&SockAddr::from(SocketAddr::new(addr, 0)))
            .with_context(|| format!("cannot connect ICMP socket to {addr}"))?;
        let fd = AsyncFd::new(socket).context("cannot register ICMP socket with tokio")?;

        // On ping sockets the kernel overwrites the ident with the socket's
        // local "port"; it only matters for filtering on raw sockets.
        let ident = std::process::id() as u16;

        let mut rtts = Vec::with_capacity(pings as usize);
        for seq in 0..pings as u16 {
            let t0 = Instant::now();
            send_echo(fd.get_ref(), addr, ident, seq, self.payload_size)?;
            if let Some(rtt) = self.await_reply(&fd, addr, raw, ident, seq, t0).await? {
                rtts.push(rtt);
            }
            super::pace(t0, self.interval, seq as u32 + 1 == pings).await;
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

    async fn await_reply(
        &self,
        fd: &AsyncFd<Socket>,
        addr: IpAddr,
        raw: bool,
        ident: u16,
        seq: u16,
        t0: Instant,
    ) -> Result<Option<Duration>> {
        let deadline = t0 + self.timeout;
        let mut buf = [MaybeUninit::<u8>::uninit(); 2048];
        loop {
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return Ok(None); // timed out: this ping is a loss
            };
            let mut guard = match tokio::time::timeout(remaining, fd.readable()).await {
                Ok(g) => g?,
                Err(_) => return Ok(None),
            };
            match guard.try_io(|inner| inner.get_ref().recv(&mut buf)) {
                Ok(Ok(n)) => {
                    // SAFETY: recv initialized the first n bytes
                    let pkt =
                        unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n) };
                    match parse_reply(pkt, addr, raw, ident) {
                        Some(rseq) if rseq == seq => return Ok(Some(t0.elapsed())),
                        // a stale reply to an earlier, timed-out ping,
                        // or unrelated ICMP traffic on a raw socket
                        _ => continue,
                    }
                }
                Ok(Err(e)) => return Err(e).context("ICMP recv failed"),
                Err(_would_block) => continue,
            }
        }
    }
}

fn open_socket(addr: IpAddr) -> Result<(Socket, bool)> {
    let (domain, proto) = match addr {
        IpAddr::V4(_) => (Domain::IPV4, Protocol::ICMPV4),
        IpAddr::V6(_) => (Domain::IPV6, Protocol::ICMPV6),
    };
    // unprivileged ping socket first
    match Socket::new(domain, Type::DGRAM, Some(proto)) {
        Ok(s) => Ok((s, false)),
        Err(dgram_err) => match Socket::new(domain, Type::RAW, Some(proto)) {
            Ok(s) => Ok((s, true)),
            Err(raw_err) => bail!(
                "cannot open ICMP socket (ping socket: {dgram_err}; raw socket: {raw_err}). \
                 Allow unprivileged ping via `sysctl net.ipv4.ping_group_range` or grant \
                 CAP_NET_RAW: `setcap cap_net_raw+ep $(command -v dabping)`"
            ),
        },
    }
}

fn send_echo(sock: &Socket, addr: IpAddr, ident: u16, seq: u16, payload_size: usize) -> Result<()> {
    let echo_type = if addr.is_ipv4() { ECHO_REQUEST_V4 } else { ECHO_REQUEST_V6 };
    let mut pkt = Vec::with_capacity(8 + payload_size);
    pkt.push(echo_type);
    pkt.push(0); // code
    pkt.extend([0, 0]); // checksum placeholder
    pkt.extend(ident.to_be_bytes());
    pkt.extend(seq.to_be_bytes());
    pkt.extend((0..payload_size).map(|i| i as u8));
    if addr.is_ipv4() {
        let ck = checksum(&pkt);
        pkt[2..4].copy_from_slice(&ck.to_be_bytes());
    } // v6: the kernel fills in the ICMPv6 checksum
    sock.send(&pkt).context("ICMP send failed")?;
    Ok(())
}

/// Returns the echo-reply sequence number if `pkt` is a reply meant for us.
fn parse_reply(mut pkt: &[u8], addr: IpAddr, raw: bool, ident: u16) -> Option<u16> {
    // raw IPv4 sockets hand us the IP header too
    if raw && addr.is_ipv4() {
        if pkt.len() < 20 {
            return None;
        }
        let ihl = ((pkt[0] & 0x0f) as usize) * 4;
        pkt = pkt.get(ihl..)?;
    }
    if pkt.len() < 8 {
        return None;
    }
    let reply_type = if addr.is_ipv4() { ECHO_REPLY_V4 } else { ECHO_REPLY_V6 };
    if pkt[0] != reply_type || pkt[1] != 0 {
        return None;
    }
    // ping sockets: the kernel rewrites the ident and demuxes per socket,
    // so only check it on raw sockets
    if raw && u16::from_be_bytes([pkt[4], pkt[5]]) != ident {
        return None;
    }
    Some(u16::from_be_bytes([pkt[6], pkt[7]]))
}

/// RFC 1071 internet checksum.
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = data.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn checksum_matches_known_vector() {
        // example from RFC 1071 §3
        let data = [0x00u8, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&data), !0xddf2);
    }

    #[test]
    fn echo_request_checksums_to_zero() {
        // a packet's checksum recomputed over itself must come out 0
        let mut pkt = vec![ECHO_REQUEST_V4, 0, 0, 0, 0x12, 0x34, 0x00, 0x01];
        pkt.extend((0..56).map(|i| i as u8));
        let ck = checksum(&pkt);
        pkt[2..4].copy_from_slice(&ck.to_be_bytes());
        assert_eq!(checksum(&pkt), 0);
    }

    #[test]
    fn parses_dgram_v4_reply() {
        let v4: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        // ident is ignored on ping sockets (kernel-rewritten)
        let pkt = [ECHO_REPLY_V4, 0, 0xab, 0xcd, 0xff, 0xff, 0x00, 0x07];
        assert_eq!(parse_reply(&pkt, v4, false, 0x1234), Some(7));
    }

    #[test]
    fn parses_raw_v4_reply_with_ip_header_and_checks_ident() {
        let v4: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        let mut pkt = vec![0x45u8]; // version 4, IHL 5
        pkt.extend([0u8; 19]); // rest of the IP header
        pkt.extend([ECHO_REPLY_V4, 0, 0, 0, 0x12, 0x34, 0x00, 0x03]);
        assert_eq!(parse_reply(&pkt, v4, true, 0x1234), Some(3));
        assert_eq!(parse_reply(&pkt, v4, true, 0x9999), None); // not our ident
    }

    #[test]
    fn rejects_non_reply_types() {
        let v4: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        let pkt = [ECHO_REQUEST_V4, 0, 0, 0, 0x12, 0x34, 0x00, 0x01];
        assert_eq!(parse_reply(&pkt, v4, false, 0x1234), None);
    }
}
