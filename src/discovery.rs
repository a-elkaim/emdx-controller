//! Find EMDX displays on the LAN via SSDP (they advertise as DLNA MediaRenderers).

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use serde::Serialize;
use tokio::{
    net::{TcpStream, UdpSocket},
    time::{interval, timeout, Instant},
};

use crate::mdc::DEFAULT_PORT as MDC_PORT;

pub const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
pub const SSDP_PORT: u16 = 1900;

pub fn m_search() -> String {
    [
        "M-SEARCH * HTTP/1.1",
        &format!("HOST: {SSDP_ADDR}:{SSDP_PORT}"),
        "MAN: \"ssdp:discover\"",
        "MX: 2",
        "ST: urn:schemas-upnp-org:device:MediaRenderer:1",
        "",
        "",
    ]
    .join("\r\n")
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Discovered {
    pub host: String,
    pub name: String,
    pub model: String,
    pub serial: Option<String>,
    pub mac: Option<String>,
    pub location: String,
}

/// Lower-cased header map of an SSDP response (status line skipped).
pub fn parse_ssdp_headers(message: &str) -> HashMap<String, String> {
    message
        .split("\r\n")
        .skip(1)
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect()
}

/// Text of the first `<tag>…</tag>` (case-insensitive), trimmed.
pub fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let lower = xml.to_ascii_lowercase();
    let open = format!("<{}>", tag.to_ascii_lowercase());
    let close = format!("</{}>", tag.to_ascii_lowercase());
    let start = lower.find(&open)? + open.len();
    let end = start + lower[start..].find(&close)?;
    let text = &xml[start..end];
    if text.contains('<') {
        return None;
    }
    Some(text.trim().to_string())
}

/// EMDX e-paper displays report e.g. modelName "EM32DX", modelDescription
/// "Samsung LFD DMR". Samsung TVs share the DMR endpoint, so require the LFD/EM signature.
pub fn is_emdx(manufacturer: &str, model_name: &str, model_description: &str) -> bool {
    if !manufacturer.to_ascii_lowercase().contains("samsung") {
        return false;
    }
    let em_model = {
        let m = model_name.as_bytes();
        m.len() >= 3 && m[..2].eq_ignore_ascii_case(b"EM") && m[2].is_ascii_digit()
    };
    model_description.contains("LFD") || em_model
}

pub fn normalize_mac(mac: &str) -> String {
    mac.split([':', '-'])
        .map(|p| format!("{p:0>2}"))
        .collect::<Vec<_>>()
        .join(":")
        .to_ascii_uppercase()
}

fn looks_like_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| (1..=2).contains(&p.len()) && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Look `host` up in a Linux `/proc/net/arp` table.
pub fn mac_from_proc_arp(table: &str, host: &str) -> Option<String> {
    table.lines().skip(1).find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        (cols.len() > 3
            && cols[0] == host
            && looks_like_mac(cols[3])
            && cols[3] != "00:00:00:00:00:00")
            .then(|| normalize_mac(cols[3]))
    })
}

/// Find a MAC in `arp -n` output.
pub fn mac_from_arp_output(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|tok| looks_like_mac(tok))
        .map(normalize_mac)
}

/// MAC of a host on the local network, from the OS ARP cache (populated by any
/// recent traffic to it). `None` across routers or when the host never answered.
pub async fn lookup_mac(host: &str) -> Option<String> {
    if let Ok(table) = tokio::fs::read_to_string("/proc/net/arp").await {
        if let Some(mac) = mac_from_proc_arp(&table, host) {
            return Some(mac);
        }
    }
    let out = timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("arp")
            .args(["-n", host])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    mac_from_arp_output(&String::from_utf8_lossy(&out.stdout))
}

pub async fn is_port_open(host: &str, port: u16, wait: Duration) -> bool {
    matches!(
        timeout(wait, TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

/// Broadcast M-SEARCH (repeated every second — UDP is lossy) and collect the
/// first `(location, server)` reply per responding host.
async fn ssdp_search(wait: Duration) -> std::io::Result<HashMap<String, String>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let packet = m_search();
    let target = SocketAddr::new(IpAddr::V4(SSDP_ADDR), SSDP_PORT);
    let mut responses = HashMap::new();
    let deadline = Instant::now() + wait;
    let mut ticker = interval(Duration::from_secs(1));
    let mut buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            _ = ticker.tick() => { let _ = socket.send_to(packet.as_bytes(), target).await; }
            res = socket.recv_from(&mut buf) => {
                if let Ok((n, from)) = res {
                    let headers = parse_ssdp_headers(&String::from_utf8_lossy(&buf[..n]));
                    if let Some(location) = headers.get("location") {
                        responses.entry(from.ip().to_string()).or_insert_with(|| location.clone());
                    }
                }
            }
        }
    }
    Ok(responses)
}

/// Discover EMDX displays. `on_progress` receives human-readable status lines.
pub async fn discover(
    wait: Duration,
    on_progress: impl Fn(&str) + Send + Sync,
) -> anyhow::Result<Vec<Discovered>> {
    on_progress("Searching for UPnP devices...");
    let responses = ssdp_search(wait).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let mut found = Vec::new();
    for (host, location) in responses {
        let Ok(res) = client.get(&location).send().await else {
            continue;
        };
        if !res.status().is_success() {
            continue;
        }
        let Ok(xml) = res.text().await else { continue };
        let manufacturer = xml_tag(&xml, "manufacturer").unwrap_or_default();
        let model = xml_tag(&xml, "modelName").unwrap_or_default();
        let description = xml_tag(&xml, "modelDescription").unwrap_or_default();
        if !is_emdx(&manufacturer, &model, &description) {
            continue;
        }
        on_progress(&format!("Checking {host} ({model})..."));
        if !is_port_open(&host, MDC_PORT, Duration::from_secs(2)).await {
            continue;
        }
        let name = xml_tag(&xml, "friendlyName")
            .filter(|s| !s.is_empty())
            .or_else(|| (!model.is_empty()).then(|| model.clone()))
            .unwrap_or_else(|| host.clone());
        found.push(Discovered {
            mac: lookup_mac(&host).await,
            serial: xml_tag(&xml, "serialNumber"),
            name,
            model,
            host,
            location,
        });
    }
    sort_by_host(&mut found);
    Ok(found)
}

/// Numeric-aware host ordering (192.168.1.9 before 192.168.1.10).
pub fn sort_by_host(list: &mut [Discovered]) {
    list.sort_by(
        |a, b| match (a.host.parse::<Ipv4Addr>(), b.host.parse::<Ipv4Addr>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.host.cmp(&b.host),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m_search_is_well_formed() {
        let m = m_search();
        assert!(m.starts_with("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n"));
        assert!(m.ends_with("\r\n\r\n"));
        assert!(m.contains("ST: urn:schemas-upnp-org:device:MediaRenderer:1"));
    }

    #[test]
    fn parses_ssdp_response() {
        let msg = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLocation: http://192.168.1.34:9197/dmr\r\nSERVER: Samsung\r\n\r\n";
        let h = parse_ssdp_headers(msg);
        assert_eq!(h["location"], "http://192.168.1.34:9197/dmr");
        assert_eq!(h["server"], "Samsung");
        assert!(!h.contains_key("http/1.1 200 ok"));
    }

    #[test]
    fn reads_xml_tags() {
        let xml = "<root><device><friendlyName> Lobby </friendlyName><MODELNAME>EM32DX</MODELNAME></device></root>";
        assert_eq!(xml_tag(xml, "friendlyName").as_deref(), Some("Lobby"));
        assert_eq!(xml_tag(xml, "modelName").as_deref(), Some("EM32DX"));
        assert_eq!(xml_tag(xml, "serialNumber"), None);
        assert_eq!(xml_tag("<device><a>x</a></device>", "device"), None);
    }

    #[test]
    fn recognises_emdx_only() {
        assert!(is_emdx("Samsung Electronics", "EM32DX", "Samsung LFD DMR"));
        assert!(is_emdx("Samsung Electronics", "EM13DX", ""));
        assert!(is_emdx("Samsung", "XYZ", "Samsung LFD DMR"));
        assert!(!is_emdx("Samsung Electronics", "QE55Q80", "Samsung TV DMR"));
        assert!(!is_emdx("Sonos", "EM32DX", "LFD"));
    }

    #[test]
    fn normalizes_macs() {
        assert_eq!(normalize_mac("b0:f2:f6:55:d:b"), "B0:F2:F6:55:0D:0B");
        assert_eq!(normalize_mac("b0-f2-f6-55-0d-0b"), "B0:F2:F6:55:0D:0B");
    }

    #[test]
    fn reads_proc_arp() {
        let table = "IP address       HW type     Flags       HW address            Mask     Device\n\
                     192.168.1.34     0x1         0x2         b0:f2:f6:55:0d:0b     *        wlp9s0\n\
                     192.168.1.50     0x1         0x0         00:00:00:00:00:00     *        wlp9s0\n";
        assert_eq!(
            mac_from_proc_arp(table, "192.168.1.34").as_deref(),
            Some("B0:F2:F6:55:0D:0B")
        );
        assert_eq!(mac_from_proc_arp(table, "192.168.1.50"), None);
        assert_eq!(mac_from_proc_arp(table, "192.168.1.99"), None);
    }

    #[test]
    fn reads_arp_output() {
        let out = "? (192.168.1.34) at b0:f2:f6:55:d:b on en0 ifscope [ethernet]";
        assert_eq!(
            mac_from_arp_output(out).as_deref(),
            Some("B0:F2:F6:55:0D:0B")
        );
        assert_eq!(mac_from_arp_output("no entry"), None);
    }

    #[test]
    fn sorts_hosts_numerically() {
        let d = |h: &str| Discovered {
            host: h.into(),
            name: String::new(),
            model: String::new(),
            serial: None,
            mac: None,
            location: String::new(),
        };
        let mut list = vec![d("192.168.1.10"), d("192.168.1.9"), d("10.0.0.1")];
        sort_by_host(&mut list);
        let hosts: Vec<&str> = list.iter().map(|x| x.host.as_str()).collect();
        assert_eq!(hosts, ["10.0.0.1", "192.168.1.9", "192.168.1.10"]);
    }

    #[tokio::test]
    async fn port_probe() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(is_port_open("127.0.0.1", port, Duration::from_secs(1)).await);
        drop(listener);
        assert!(!is_port_open("127.0.0.1", port, Duration::from_secs(1)).await);
    }
}
