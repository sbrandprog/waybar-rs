//! Pure parsers for the network module.
//!
//! Everything here takes file contents as `&str` so it can be unit-tested
//! without touching the filesystem, mirroring the other `sysinfo` parsers.

/// Sum of received/transmitted bytes for `ifname` from `/proc/net/dev`.
pub fn parse_netdev(content: &str, ifname: &str) -> Option<(u64, u64)> {
    // The first two lines are headers.
    for line in content.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != ifname {
            continue;
        }
        let mut fields = rest.split_whitespace();
        let received: u64 = fields.next()?.parse().ok()?;
        // Skip packets, errs, drop, fifo, frame, compressed, multicast.
        for _ in 0..7 {
            fields.next()?;
        }
        let transmitted: u64 = fields.next()?.parse().ok()?;
        return Some((received, transmitted));
    }
    None
}

/// The default route (interface, gateway) from `/proc/net/route`.
///
/// Picks the entry with destination `0.0.0.0`, a non-zero gateway and the
/// lowest metric, matching the interface upstream selects from netlink.
pub fn parse_default_route(content: &str) -> Option<(String, String)> {
    let mut best: Option<(u32, String, String)> = None;
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        let (iface, destination, gateway, metric) = (fields[0], fields[1], fields[2], fields[6]);
        if destination != "00000000" || gateway == "00000000" {
            continue;
        }
        let Ok(metric) = metric.parse::<u32>() else {
            continue;
        };
        if let Some(gateway) = parse_hex_ipv4(gateway) {
            let candidate = (metric, iface.to_string(), gateway);
            if best.as_ref().is_none_or(|current| metric < current.0) {
                best = Some(candidate);
            }
        }
    }
    best.map(|(_, iface, gateway)| (iface, gateway))
}

/// Decode a little-endian hex IPv4 address as printed by `/proc/net/route`.
fn parse_hex_ipv4(hex: &str) -> Option<String> {
    let value = u32::from_str_radix(hex, 16).ok()?;
    let bytes = value.to_ne_bytes();
    Some(format!(
        "{}.{}.{}.{}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    ))
}

/// Split an `addr/prefix` string into its parts.
pub fn parse_cidr(addr: &str) -> Option<(String, u32)> {
    let (ip, prefix) = addr.split_once('/')?;
    Some((ip.to_string(), prefix.parse().ok()?))
}

/// Dotted-quad netmask for an IPv4 prefix length.
pub fn netmask_from_cidr(cidr: u32) -> String {
    let mask = if cidr == 0 {
        0
    } else {
        u32::MAX << (32 - cidr.min(32))
    };
    let bytes = mask.to_be_bytes();
    format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3])
}

/// IPv6 netmask for a prefix length, in the form `inet_ntop` prints it.
pub fn netmask6_from_cidr(cidr: u32) -> String {
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let shift = ((i + 1) * 8) as i32 - cidr as i32;
        let shift = shift.clamp(0, 8) as u32;
        *byte = (!0u16 << shift) as u8;
    }
    std::net::Ipv6Addr::from(bytes).to_string()
}

/// Human-readable connectivity descriptor derived from a dBm value.
pub fn signal_strength_app(dbm: i32) -> &'static str {
    if dbm >= -50 {
        "Great Connectivity"
    } else if dbm >= -60 {
        "Good Connectivity"
    } else if dbm >= -67 {
        "Streaming"
    } else if dbm >= -70 {
        "Web Surfing"
    } else if dbm >= -80 {
        "Basic Connectivity"
    } else {
        "Poor Connectivity"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETDEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0
wlan0: 500000 400 0 0 0 0 0 0 120000 300 0 0 0 0 0 0
";

    #[test]
    fn netdev_picks_interface() {
        assert_eq!(parse_netdev(NETDEV, "wlan0"), Some((500_000, 120_000)));
        assert_eq!(parse_netdev(NETDEV, "lo"), Some((1000, 2000)));
        assert_eq!(parse_netdev(NETDEV, "eth0"), None);
    }

    #[test]
    fn default_route_decodes_gateway() {
        let content = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
wlan0\t00000000\t0100A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
eth0\t00000000\t0100A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
wlan0\t0000A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0
";
        assert_eq!(
            parse_default_route(content),
            Some(("eth0".to_string(), "192.168.0.1".to_string()))
        );
    }

    #[test]
    fn default_route_missing() {
        let content = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
wlan0\t0000A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0
";
        assert_eq!(parse_default_route(content), None);
    }

    #[test]
    fn parses_cidr() {
        assert_eq!(
            parse_cidr("192.168.0.13/24"),
            Some(("192.168.0.13".to_string(), 24))
        );
        assert_eq!(parse_cidr("no-prefix"), None);
    }

    #[test]
    fn netmasks() {
        assert_eq!(netmask_from_cidr(24), "255.255.255.0");
        assert_eq!(netmask_from_cidr(0), "0.0.0.0");
        assert_eq!(netmask_from_cidr(32), "255.255.255.255");
        assert_eq!(netmask6_from_cidr(64), "ffff:ffff:ffff:ffff::");
        assert_eq!(netmask6_from_cidr(0), "::");
    }

    #[test]
    fn signal_descriptors() {
        assert_eq!(signal_strength_app(-40), "Great Connectivity");
        assert_eq!(signal_strength_app(-65), "Streaming");
        assert_eq!(signal_strength_app(-85), "Poor Connectivity");
    }
}
