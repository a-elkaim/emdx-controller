//! Wi-Fi networks around this computer, for picking the display's network during
//! setup. The display has an access-point scan over Bluetooth, but the ePaper app
//! never uses it (its reply format is unknown), so the list comes from this
//! computer, through the `wifi_scan` crate: nl80211 on Linux (whatever manages the
//! network), the WLAN API on Windows, CoreWLAN on macOS (needs location access for
//! the names). No external program.
//!
//! On Linux a scan needs root or the `CAP_NET_ADMIN` capability. Without them the
//! scan fails and the setup dialog keeps the free Wi-Fi name field.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::Result;
use serde::Serialize;

/// One network the display could join
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WifiNetwork {
    pub ssid: String,
    /// 0–100 (the strongest access point of that name)
    pub signal: u8,
    /// e.g. `WPA2`, empty for an open network
    pub security: String,
    /// WPA Enterprise (802.1X): the display's setup only takes a password
    pub enterprise: bool,
}

/// One access point, before networks are merged by name
struct Seen {
    network: WifiNetwork,
    mhz: u32,
}

/// Reading the results: at least this long (a full 2.4 + 5 GHz scan takes a few
/// seconds), at most that long, and done once reads stop finding new access points
const SCAN_MIN: Duration = Duration::from_secs(4);
const SCAN_MAX: Duration = Duration::from_secs(10);
const QUIET_READS: u32 = 2;
const READ_EVERY: Duration = Duration::from_secs(1);

/// One scan at a time: a second caller waits and shares the radio
static SCANNING: Mutex<()> = Mutex::new(());

/// 2.4 GHz networks (the only ones the display joins), strongest first, one entry
/// per name.
///
/// `wifi_scan::scan()` starts a scan, waits 1.5 s and keeps what was seen in the
/// last 2.5 s: often only part of a scan. While a scan runs, another call can't
/// start one and just reads what was found so far, so this reads every second and
/// gathers every access point (by BSSID) until the reads stop finding new ones.
pub(crate) fn scan() -> Result<Vec<WifiNetwork>> {
    let _one_at_a_time = SCANNING.lock().unwrap_or_else(|e| e.into_inner());
    let started = Instant::now();
    let mut by_bssid: HashMap<String, wifi_scan::Wifi> = HashMap::new();
    let mut quiet = 0;
    let mut first = true;
    loop {
        let read = wifi_scan::scan();
        let was_first = std::mem::replace(&mut first, false);
        match read {
            Ok(found) => {
                let before = by_bssid.len();
                for w in found {
                    // The latest reading of each access point
                    by_bssid.insert(w.mac.clone(), w);
                }
                quiet = if by_bssid.len() > before {
                    0
                } else {
                    quiet + 1
                };
            }
            // Not allowed to scan at all (e.g. not root on Linux): no list
            Err(e) if was_first => {
                return Err(anyhow::anyhow!("{e}"));
            }
            // A later read failing: keep what was gathered
            Err(_) => quiet += 1,
        }
        let elapsed = started.elapsed();
        if (elapsed >= SCAN_MIN && quiet >= QUIET_READS) || elapsed >= SCAN_MAX {
            break;
        }
        std::thread::sleep(READ_EVERY);
    }
    let found: Vec<_> = by_bssid.into_values().collect();
    // EMDX_WIFI_DEBUG=1: every access point gathered, before filtering
    if std::env::var_os("EMDX_WIFI_DEBUG").is_some() {
        emdx_controller::say!(
            "📶 {} access points in {:.1}s:",
            found.len(),
            started.elapsed().as_secs_f32()
        );
        for w in &found {
            emdx_controller::say!(
                "   {:?} channel {} ({} MHz) {} dBm {:?}",
                w.ssid,
                w.channel,
                w.get_frequency(),
                w.signal_level,
                w.security.iter().map(|s| s.to_string()).collect::<Vec<_>>()
            );
        }
    }
    Ok(merge(
        found
            .into_iter()
            .map(|w| {
                let security = if w.is_open() {
                    String::new()
                } else if w.is_wpa3() {
                    "WPA3".to_string()
                } else if w.is_wpa2() {
                    "WPA2".to_string()
                } else {
                    w.security
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                Seen {
                    mhz: w.get_frequency(),
                    network: WifiNetwork {
                        enterprise: w.is_enterprise(),
                        signal: percent_from_dbm(w.signal_level),
                        ssid: w.ssid,
                        security,
                    },
                }
            })
            .collect(),
    ))
}

/// dBm to a 0–100 scale (as NetworkManager shows it): -100 dBm or less is 0,
/// -50 dBm or more is 100
fn percent_from_dbm(dbm: i32) -> u8 {
    (2 * (dbm + 100)).clamp(0, 100) as u8
}

/// Named 2.4 GHz networks, one per name (its best signal), strongest first
fn merge(seen: Vec<Seen>) -> Vec<WifiNetwork> {
    let mut by_name: BTreeMap<String, WifiNetwork> = BTreeMap::new();
    for Seen { network, mhz } in seen {
        // Hidden networks have no name to pick; the display only joins 2.4 GHz
        if network.ssid.is_empty() || !(2400..2500).contains(&mhz) {
            continue;
        }
        by_name
            .entry(network.ssid.clone())
            .and_modify(|seen| seen.signal = seen.signal.max(network.signal))
            .or_insert(network);
    }
    let mut list: Vec<_> = by_name.into_values().collect();
    list.sort_by(|a, b| b.signal.cmp(&a.signal).then_with(|| a.ssid.cmp(&b.ssid)));
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_named_2_4_ghz_networks_strongest_first() {
        let seen = |ssid: &str, mhz, signal| Seen {
            mhz,
            network: WifiNetwork {
                ssid: ssid.into(),
                signal,
                security: "WPA2".into(),
                enterprise: false,
            },
        };
        let list = merge(vec![
            seen("Home", 2412, 62),
            seen("Home", 5500, 100),
            seen("Home", 2437, 80),
            seen("Cafe", 2462, 85),
            seen("", 2462, 90),
            seen("Fast", 5300, 99),
        ]);
        let names: Vec<_> = list.iter().map(|n| n.ssid.as_str()).collect();
        // 5 GHz-only and hidden ones are left out; "Home" keeps its best 2.4 GHz signal
        assert_eq!(names, ["Cafe", "Home"]);
        assert_eq!(list[1].signal, 80);
    }

    #[test]
    fn dbm_maps_to_percent() {
        assert_eq!(percent_from_dbm(-40), 100);
        assert_eq!(percent_from_dbm(-50), 100);
        assert_eq!(percent_from_dbm(-70), 60);
        assert_eq!(percent_from_dbm(-100), 0);
        assert_eq!(percent_from_dbm(-110), 0);
    }
}
