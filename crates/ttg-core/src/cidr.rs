//! IPv4 address ranges: picking a free block inside a network for a new subnet.

/// `10.0.1.0/24` -> (network address, prefix length). The address is masked, so
/// `10.0.1.7/24` reads as `10.0.1.0/24`. IPv4 only.
pub fn parse(s: &str) -> Option<(u32, u8)> {
    let (addr, len) = s.trim().split_once('/')?;
    let len: u8 = len.parse().ok().filter(|l| *l <= 32)?;
    let ip: std::net::Ipv4Addr = addr.parse().ok()?;
    Some((u32::from(ip) & mask(len), len))
}

fn mask(len: u8) -> u32 {
    if len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(len))
    }
}

fn overlaps((a, la): (u32, u8), (b, lb): (u32, u8)) -> bool {
    let m = mask(la.min(lb));
    a & m == b & m
}

fn format((addr, len): (u32, u8)) -> String {
    format!("{}/{len}", std::net::Ipv4Addr::from(addr))
}

/// The size of block a new subnet gets inside `network`: a /24, or a quarter of the
/// network when that is smaller than a /22 (capped at /28).
pub fn subnet_prefix(network: &str) -> Option<u8> {
    let (_, len) = parse(network)?;
    Some(if len <= 22 { 24 } else { (len + 2).min(28) })
}

/// The first block of `prefix` bits inside `network` that overlaps none of `taken`
/// (ranges that do not parse are ignored). `None` when the network is full or does not
/// parse.
pub fn next_free(network: &str, prefix: u8, taken: &[&str]) -> Option<String> {
    let (base, len) = parse(network)?;
    if prefix < len || prefix > 32 {
        return None;
    }
    let used: Vec<(u32, u8)> = taken.iter().filter_map(|t| parse(t)).collect();
    let step = 1u64 << (32 - u32::from(prefix));
    let count = 1u64 << u32::from(prefix - len);
    (0..count)
        .map(|i| ((u64::from(base) + i * step) as u32, prefix))
        .find(|cand| !used.iter().any(|u| overlaps(*cand, *u)))
        .map(format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_first_block_nothing_uses() {
        assert_eq!(next_free("10.0.0.0/16", 24, &[]).as_deref(), Some("10.0.0.0/24"));
        assert_eq!(
            next_free("10.0.0.0/16", 24, &["10.0.0.0/24", "10.0.1.0/24", "10.0.3.0/24"]).as_deref(),
            Some("10.0.2.0/24")
        );
        // A larger block elsewhere covers several /24s.
        assert_eq!(
            next_free("10.20.0.0/16", 24, &["10.20.0.0/22", "bogus"]).as_deref(),
            Some("10.20.4.0/24")
        );
        // Ranges outside the network do not matter; a full network has no room.
        assert_eq!(
            next_free("10.0.0.0/24", 26, &["192.168.0.0/16"]).as_deref(),
            Some("10.0.0.0/26")
        );
        assert_eq!(
            next_free("10.0.0.0/24", 24, &["10.0.0.0/25", "10.0.0.128/25"]),
            None
        );
    }

    #[test]
    fn subnet_size_follows_the_network() {
        assert_eq!(subnet_prefix("10.0.0.0/16"), Some(24));
        assert_eq!(subnet_prefix("10.0.0.0/24"), Some(26));
        assert_eq!(subnet_prefix("10.0.0.0/27"), Some(28));
        assert_eq!(subnet_prefix("nope"), None);
    }
}
