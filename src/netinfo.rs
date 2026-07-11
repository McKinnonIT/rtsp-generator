use std::net::{IpAddr, Ipv4Addr};

use if_addrs::{IfAddr, Interface};

#[derive(Debug, thiserror::Error)]
pub enum NetInfoError {
    #[error("failed to enumerate network interfaces: {0}")]
    Enumerate(#[source] std::io::Error),
    #[error(
        "could not determine a LAN IP to advertise (no non-excluded, non-loopback interface \
         found); set `advertise_ip` in config.yaml"
    )]
    NoCandidate,
}

/// True if `name` starts with any of the configured exclusion prefixes
/// (e.g. "docker0", "br-", "veth", "tailscale0", "zt").
fn is_excluded(name: &str, exclude_prefixes: &[String]) -> bool {
    exclude_prefixes.iter().any(|p| name.starts_with(p.as_str()))
}

/// True if `ip` falls in a common private LAN range (RFC 1918).
fn is_private_lan_ip(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    match o[0] {
        10 => true,
        172 => (16..=31).contains(&o[1]),
        192 => o[1] == 168,
        _ => false,
    }
}

/// Picks the best LAN IP candidate from a list of interfaces: excludes loopback and
/// configured-exclusion interfaces, then prefers an interface in a common LAN range if
/// multiple candidates remain.
fn select_candidate(interfaces: &[Interface], exclude_prefixes: &[String]) -> Option<IpAddr> {
    let candidates: Vec<&Interface> = interfaces
        .iter()
        .filter(|i| !i.is_loopback())
        .filter(|i| matches!(i.addr, IfAddr::V4(_)))
        .filter(|i| !is_excluded(&i.name, exclude_prefixes))
        .collect();

    let lan_range_match = candidates.iter().find(|i| match i.ip() {
        IpAddr::V4(v4) => is_private_lan_ip(&v4),
        IpAddr::V6(_) => false,
    });

    lan_range_match
        .or_else(|| candidates.first())
        .map(|i| i.ip())
}

/// Detects the LAN IP to advertise in `streams.yaml`. `advertise_ip_override` (from
/// `config.yaml`) always wins if set; otherwise interfaces are enumerated and filtered.
pub fn detect_lan_ip(
    advertise_ip_override: Option<IpAddr>,
    exclude_prefixes: &[String],
) -> Result<IpAddr, NetInfoError> {
    if let Some(ip) = advertise_ip_override {
        return Ok(ip);
    }
    let interfaces = if_addrs::get_if_addrs().map_err(NetInfoError::Enumerate)?;
    select_candidate(&interfaces, exclude_prefixes).ok_or(NetInfoError::NoCandidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use if_addrs::Ifv4Addr;

    fn v4_iface(name: &str, ip: &str) -> Interface {
        Interface {
            name: name.to_string(),
            addr: IfAddr::V4(Ifv4Addr {
                ip: ip.parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
                prefixlen: 24,
                broadcast: None,
            }),
            index: Some(1),
            #[cfg(windows)]
            adapter_name: String::new(),
        }
    }

    fn loopback_iface() -> Interface {
        v4_iface("lo", "127.0.0.1")
    }

    #[test]
    fn prefers_lan_range_over_other_candidates() {
        let interfaces = vec![
            v4_iface("tailscale0", "100.64.0.5"),
            v4_iface("eth0", "192.168.1.50"),
        ];
        let excludes = vec!["tailscale0".to_string()];
        assert_eq!(
            select_candidate(&interfaces, &excludes),
            Some("192.168.1.50".parse().unwrap())
        );
    }

    #[test]
    fn excludes_configured_prefixes() {
        let interfaces = vec![
            v4_iface("docker0", "172.17.0.1"),
            v4_iface("br-abcdef123456", "172.18.0.1"),
            v4_iface("eth0", "10.0.0.5"),
        ];
        let excludes = vec!["docker0".to_string(), "br-".to_string()];
        assert_eq!(
            select_candidate(&interfaces, &excludes),
            Some("10.0.0.5".parse().unwrap())
        );
    }

    #[test]
    fn excludes_loopback() {
        let interfaces = vec![loopback_iface(), v4_iface("eth0", "192.168.1.50")];
        assert_eq!(
            select_candidate(&interfaces, &[]),
            Some("192.168.1.50".parse().unwrap())
        );
    }

    #[test]
    fn no_candidates_returns_none() {
        let interfaces = vec![loopback_iface()];
        assert_eq!(select_candidate(&interfaces, &[]), None);
    }

    #[test]
    fn override_wins_without_touching_interfaces() {
        let ip = detect_lan_ip(Some("192.168.1.99".parse().unwrap()), &[]).unwrap();
        assert_eq!(ip, "192.168.1.99".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn recognizes_private_ranges() {
        assert!(is_private_lan_ip(&"10.1.2.3".parse().unwrap()));
        assert!(is_private_lan_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_lan_ip(&"172.31.255.255".parse().unwrap()));
        assert!(is_private_lan_ip(&"192.168.0.1".parse().unwrap()));
        assert!(!is_private_lan_ip(&"172.32.0.1".parse().unwrap()));
        assert!(!is_private_lan_ip(&"8.8.8.8".parse().unwrap()));
    }
}
