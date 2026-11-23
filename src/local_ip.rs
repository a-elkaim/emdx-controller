//! Choose the address a display should use to reach this machine.

use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub addr: Ipv4Addr,
    pub netmask: Ipv4Addr,
}

const VIRTUAL_PREFIXES: &[&str] = &[
    "docker",
    "br-",
    "veth",
    "virbr",
    "vmnet",
    "vboxnet",
    "tun",
    "tap",
    "wg",
    "zt",
    "tailscale",
    "utun",
    "lo",
];

/// Interfaces that never face the LAN the displays live on.
pub fn is_virtual(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    VIRTUAL_PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// Pick from `candidates`:
/// - with an IPv4 `target`, the interface on the same subnet;
/// - otherwise the first non-virtual private address, then any non-virtual one, then anything.
pub fn pick(candidates: &[Candidate], target: Option<&str>) -> Option<Ipv4Addr> {
    if let Some(target) = target.and_then(|t| t.parse::<Ipv4Addr>().ok()) {
        let t = u32::from(target);
        if let Some(c) = candidates.iter().find(|c| {
            let mask = u32::from(c.netmask);
            u32::from(c.addr) & mask == t & mask
        }) {
            return Some(c.addr);
        }
    }
    let physical: Vec<&Candidate> = candidates.iter().filter(|c| !is_virtual(&c.name)).collect();
    physical
        .iter()
        .find(|c| c.addr.is_private())
        .or_else(|| physical.first())
        .map(|c| c.addr)
        .or_else(|| candidates.first().map(|c| c.addr))
}

/// Non-loopback IPv4 interfaces of this machine.
pub fn candidates() -> Vec<Candidate> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|iface| !iface.is_loopback())
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) => Some(Candidate {
                name: iface.name,
                addr: v4.ip,
                netmask: v4.netmask,
            }),
            _ => None,
        })
        .collect()
}

/// `LOCAL_IP` env override, else [`pick`] over this machine's interfaces.
/// Returns an empty string when no usable address exists.
pub fn get_local_ip(target: Option<&str>) -> String {
    if let Ok(ip) = std::env::var("LOCAL_IP") {
        if !ip.is_empty() {
            return ip;
        }
    }
    pick(&candidates(), target)
        .map(|ip| ip.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, addr: &str, mask: &str) -> Candidate {
        Candidate {
            name: name.into(),
            addr: addr.parse().unwrap(),
            netmask: mask.parse().unwrap(),
        }
    }

    fn sample() -> Vec<Candidate> {
        vec![
            c("docker0", "172.17.0.1", "255.255.0.0"),
            c("wlp9s0", "192.168.1.39", "255.255.255.0"),
            c("br-c20b", "172.18.0.1", "255.255.0.0"),
        ]
    }

    #[test]
    fn prefers_same_subnet_as_target() {
        assert_eq!(
            pick(&sample(), Some("192.168.1.34")),
            Some("192.168.1.39".parse().unwrap())
        );
        assert_eq!(
            pick(&sample(), Some("172.18.4.2")),
            Some("172.18.0.1".parse().unwrap())
        );
    }

    #[test]
    fn default_skips_virtual_interfaces() {
        assert_eq!(pick(&sample(), None), Some("192.168.1.39".parse().unwrap()));
        // Unknown subnet falls back to the default choice
        assert_eq!(
            pick(&sample(), Some("10.9.9.9")),
            Some("192.168.1.39".parse().unwrap())
        );
        // Hostnames are not subnet-matched
        assert_eq!(
            pick(&sample(), Some("display.local")),
            Some("192.168.1.39".parse().unwrap())
        );
    }

    #[test]
    fn falls_back_to_public_then_virtual() {
        let public = vec![
            c("docker0", "172.17.0.1", "255.255.0.0"),
            c("eth0", "8.8.4.4", "255.255.255.0"),
        ];
        assert_eq!(pick(&public, None), Some("8.8.4.4".parse().unwrap()));
        let only_virtual = vec![c("docker0", "172.17.0.1", "255.255.0.0")];
        assert_eq!(
            pick(&only_virtual, None),
            Some("172.17.0.1".parse().unwrap())
        );
        assert_eq!(pick(&[], None), None);
    }

    #[test]
    fn virtual_names() {
        for name in ["docker0", "br-abc", "veth1", "tailscale0", "lo", "WG0"] {
            assert!(is_virtual(name), "{name}");
        }
        for name in ["eth0", "wlp9s0", "en0", "enp11s0"] {
            assert!(!is_virtual(name), "{name}");
        }
    }
}
