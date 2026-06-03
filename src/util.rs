use anyhow::{Result, bail};

/// Parse "90s", "5m", "3h", "10d", "2w" (bare numbers = seconds).
pub fn parse_range(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.as_bytes().last() {
        Some(b's') => (&s[..s.len() - 1], 1),
        Some(b'm') => (&s[..s.len() - 1], 60),
        Some(b'h') => (&s[..s.len() - 1], 3600),
        Some(b'd') => (&s[..s.len() - 1], 86400),
        Some(b'w') => (&s[..s.len() - 1], 604800),
        _ => (s, 1),
    };
    let n: u64 = num.parse().map_err(|_| anyhow::anyhow!("cannot parse range {s:?}"))?;
    if n == 0 {
        bail!("range must be > 0");
    }
    Ok(n * mult)
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before the epoch")
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("90").unwrap(), 90);
        assert_eq!(parse_range("90s").unwrap(), 90);
        assert_eq!(parse_range("5m").unwrap(), 300);
        assert_eq!(parse_range("3h").unwrap(), 10800);
        assert_eq!(parse_range("10d").unwrap(), 864000);
        assert_eq!(parse_range("2w").unwrap(), 1209600);
        assert!(parse_range("0h").is_err());
        assert!(parse_range("abc").is_err());
    }
}
