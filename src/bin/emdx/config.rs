//! Saved CLI settings: named displays (`emdx add <name>`) and a default.
//!
//! Stored as JSON at `$EMDX_CONFIG`, else `$XDG_CONFIG_HOME/emdx/config.json`,
//! else `~/.config/emdx/config.json`, readable only by the owner (it holds PINs).
//!
//! ```json
//! { "default": "lobby",
//!   "displays": { "lobby": { "host": "192.168.1.34", "pin": "123456", "mac": "…" } } }
//! ```
//! A file from the single-display version (`{ "host", "pin", "mac" }`) loads
//! as one display named [`LEGACY_NAME`].

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Name given to a display saved before names existed, and the default
/// suggestion for the first display.
pub const LEGACY_NAME: &str = "display";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayEntry {
    pub host: String,
    pub pin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

/// A saved display found at a new address (see [`CliConfig::relocate_by_mac`])
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Relocated {
    pub name: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub displays: BTreeMap<String, DisplayEntry>,
}

/// Display names: 1–32 of `A-Z a-z 0-9 _ -`.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        bail!("invalid display name {name:?}: use 1-32 letters, digits, '-' or '_'");
    }
    Ok(())
}

impl CliConfig {
    /// Every saved display as a command target, in name order (for `--all`).
    pub fn targets(&self) -> Vec<Target> {
        self.displays
            .iter()
            .map(|(name, d)| Target {
                name: Some(name.clone()),
                host: d.host.clone(),
                pin: d.pin.clone(),
                mac: d.mac.clone().filter(|m| !m.is_empty()),
            })
            .collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.displays.keys().map(String::as_str).collect()
    }

    fn names_hint(&self) -> String {
        if self.displays.is_empty() {
            "none saved".into()
        } else {
            format!("saved: {}", self.names().join(", "))
        }
    }

    /// The display used when none is named: the explicit default, or the
    /// only saved display.
    pub fn default_name(&self) -> Option<&str> {
        match &self.default {
            Some(d) if self.displays.contains_key(d) => Some(d),
            _ if self.displays.len() == 1 => self.displays.keys().next().map(String::as_str),
            _ => None,
        }
    }

    pub fn get(&self, name: &str) -> Result<&DisplayEntry> {
        self.displays
            .get(name)
            .with_context(|| format!("unknown display {name:?} ({})", self.names_hint()))
    }

    /// Add or replace a display. The first display becomes the default.
    pub fn upsert(&mut self, name: &str, entry: DisplayEntry, make_default: bool) -> Result<()> {
        validate_name(name)?;
        self.displays.insert(name.to_string(), entry);
        if make_default || self.default_name().is_none() || self.displays.len() == 1 {
            self.default = Some(name.to_string());
        }
        Ok(())
    }

    pub fn set_default(&mut self, name: &str) -> Result<()> {
        self.get(name)?;
        self.default = Some(name.to_string());
        Ok(())
    }

    /// Remove a display; if it was the default, the first remaining one takes over.
    pub fn remove(&mut self, name: &str) -> Result<DisplayEntry> {
        let removed = self
            .displays
            .remove(name)
            .with_context(|| format!("unknown display {name:?} ({})", self.names_hint()))?;
        if self.default.as_deref() == Some(name) {
            self.default = self.displays.keys().next().cloned();
        }
        Ok(removed)
    }

    /// Displays found on the network (`(mac, host)` pairs) whose MAC matches a saved
    /// display at another address: that display was given a new IP by the router, so
    /// its address is updated. A MAC saved for more than one display is left alone.
    /// Returns what moved, in name order.
    pub fn relocate_by_mac<'a>(
        &mut self,
        found: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Vec<Relocated> {
        let found: Vec<(String, &str)> = found
            .into_iter()
            .map(|(mac, host)| (emdx_controller::discovery::normalize_mac(mac), host))
            .collect();
        let mut moved = Vec::new();
        let saved_macs: Vec<String> = self
            .displays
            .values()
            .filter_map(|e| e.mac.as_deref())
            .map(emdx_controller::discovery::normalize_mac)
            .collect();
        for (name, entry) in &mut self.displays {
            let Some(mac) = entry
                .mac
                .as_deref()
                .map(emdx_controller::discovery::normalize_mac)
            else {
                continue;
            };
            if saved_macs.iter().filter(|m| **m == mac).count() > 1 {
                continue;
            }
            let mut hosts = found.iter().filter(|(m, _)| *m == mac).map(|(_, h)| *h);
            let (Some(host), None) = (hosts.next(), hosts.next()) else {
                continue;
            };
            if host != entry.host {
                moved.push(Relocated {
                    name: name.clone(),
                    from: std::mem::replace(&mut entry.host, host.to_string()),
                    to: host.to_string(),
                });
            }
        }
        moved
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        validate_name(to)?;
        if self.displays.contains_key(to) {
            bail!("a display named {to:?} already exists");
        }
        let entry = self.remove_keep_default(from)?;
        self.displays.insert(to.to_string(), entry);
        if self.default.as_deref() == Some(from) {
            self.default = Some(to.to_string());
        }
        Ok(())
    }

    fn remove_keep_default(&mut self, name: &str) -> Result<DisplayEntry> {
        self.displays
            .remove(name)
            .with_context(|| format!("unknown display {name:?} ({})", self.names_hint()))
    }

    /// Parse the current format, or the single-display format it replaced.
    pub fn from_json(value: Value) -> Result<CliConfig> {
        if value.get("displays").is_some() {
            let config: CliConfig = serde_json::from_value(value)?;
            for name in config.displays.keys() {
                validate_name(name)?;
            }
            return Ok(config);
        }
        if value.get("host").is_some() {
            let entry: DisplayEntry = serde_json::from_value(value)?;
            let mut config = CliConfig::default();
            config.upsert(LEGACY_NAME, entry, true)?;
            return Ok(config);
        }
        bail!("unrecognized settings format")
    }
}

/// Config file location from the relevant environment values.
pub fn path_from(
    emdx_config: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    let non_empty = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(PathBuf::from);
    if let Some(p) = non_empty(emdx_config) {
        return Some(p);
    }
    if let Some(xdg) = non_empty(xdg_config_home) {
        return Some(xdg.join("emdx/config.json"));
    }
    non_empty(home).map(|h| h.join(".config/emdx/config.json"))
}

pub fn default_path() -> Result<PathBuf> {
    let var = |k: &str| std::env::var(k).ok();
    path_from(
        var("EMDX_CONFIG").as_deref(),
        var("XDG_CONFIG_HOME").as_deref(),
        var("HOME").as_deref(),
    )
    .context("cannot locate a config folder: set EMDX_CONFIG or HOME")
}

/// An empty config when nothing has been saved yet.
pub fn load(path: &Path) -> Result<CliConfig> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(anyhow::Error::from)
            .and_then(CliConfig::from_json)
            .with_context(|| format!("invalid config file {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CliConfig::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write atomically with owner-only permissions.
pub fn save(path: &Path, config: &CliConfig) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(config)?;
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Connection details for a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    /// Saved display this came from, if any
    pub name: Option<String>,
    pub host: String,
    pub pin: String,
    pub mac: Option<String>,
}

/// Pick the saved display (`--display`, else the default), then let explicit
/// flags override its fields. Flags alone (host + pin) need no saved display.
pub fn resolve(
    display: Option<&str>,
    host: Option<String>,
    pin: Option<String>,
    mac: Option<String>,
    saved: &CliConfig,
) -> Result<Target> {
    let named = match display {
        Some(name) => Some((name, saved.get(name)?)),
        None if host.is_some() && pin.is_some() => None,
        None => match saved.default_name() {
            Some(name) => Some((name, saved.get(name)?)),
            None if saved.displays.len() > 1 => bail!(
                "several displays are saved ({}); pick one with --display <name> \
                 or set a default with `emdx displays default <name>`",
                saved.names().join(", ")
            ),
            None => None,
        },
    };
    let entry = named.map(|(_, e)| e);
    let non_empty = |v: Option<String>| v.filter(|s| !s.is_empty());
    let host = non_empty(host.or_else(|| entry.map(|e| e.host.clone())));
    let pin = non_empty(pin.or_else(|| entry.map(|e| e.pin.clone())));
    let mac = non_empty(mac.or_else(|| entry.and_then(|e| e.mac.clone())));
    match (host, pin) {
        (Some(host), Some(pin)) => Ok(Target {
            name: named.map(|(n, _)| n.to_string()),
            host,
            pin,
            mac,
        }),
        (host, _) => bail!(
            "no display configured: run `emdx add` (or `emdx setup` for a new display) or pass {}",
            if host.is_none() {
                "--host and --display-pin"
            } else {
                "--display-pin"
            }
        ),
    }
}

/// MAC for `wake`: flag, else the selected/default saved display.
pub fn resolve_mac(
    display: Option<&str>,
    mac: Option<String>,
    saved: &CliConfig,
) -> Result<String> {
    if let Some(mac) = mac.filter(|s| !s.is_empty()) {
        return Ok(mac);
    }
    let name = match display {
        Some(name) => name,
        None => saved
            .default_name()
            .context("no MAC address configured: run `emdx add` or pass --mac")?,
    };
    saved
        .get(name)?
        .mac
        .clone()
        .filter(|s| !s.is_empty())
        .with_context(|| {
            format!("display {name:?} has no MAC address: run `emdx add {name}` or pass --mac")
        })
}

/// `123456` → `1****6`, for display.
pub fn mask_pin(pin: &str) -> String {
    match pin.len() {
        0 => String::new(),
        1..=2 => "*".repeat(pin.len()),
        n => format!("{}{}{}", &pin[..1], "*".repeat(n - 2), &pin[n - 1..]),
    }
}

/// Parse a 1-based menu choice.
pub fn parse_choice(input: &str, count: usize) -> Option<usize> {
    input
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=count).contains(n))
        .map(|n| n - 1)
}

/// Turn a display's own name ("Samsung EMDX 743D") into a valid saved name.
pub fn suggest_name(friendly: &str) -> String {
    let mut out = String::new();
    for c in friendly.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_end_matches('-').chars().take(29).collect();
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        LEGACY_NAME.into()
    } else {
        out
    }
}

/// `base`, or `base-2`, `base-3`… when already taken.
pub fn unique_name(base: &str, saved: &CliConfig) -> String {
    if !saved.displays.contains_key(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !saved.displays.contains_key(candidate))
        .expect("an unused suffix exists")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(host: &str, mac: Option<&str>) -> DisplayEntry {
        DisplayEntry {
            host: host.into(),
            pin: "123456".into(),
            mac: mac.map(Into::into),
        }
    }

    #[test]
    fn relocates_displays_by_mac() {
        let mut c = two();
        // Same MAC in another notation, new address; unknown MACs are ignored
        let moved = c.relocate_by_mac([
            ("AA-AA-AA-AA-AA-AA", "10.0.0.9"),
            ("bb:bb:bb:bb:bb:bb", "10.0.0.2"),
        ]);
        assert_eq!(
            moved,
            vec![Relocated {
                name: "lobby".into(),
                from: "10.0.0.1".into(),
                to: "10.0.0.9".into()
            }]
        );
        assert_eq!(c.displays["lobby"].host, "10.0.0.9");
        assert_eq!(c.displays["office"].host, "10.0.0.2");
        // Already at that address: nothing to do
        assert!(c
            .relocate_by_mac([("aa:aa:aa:aa:aa:aa", "10.0.0.9")])
            .is_empty());
        // Ambiguous: the MAC answers from two addresses
        assert!(c
            .relocate_by_mac([
                ("aa:aa:aa:aa:aa:aa", "10.0.0.3"),
                ("aa:aa:aa:aa:aa:aa", "10.0.0.4")
            ])
            .is_empty());
        // Ambiguous: two saved displays share the MAC
        c.upsert("copy", entry("10.0.0.5", Some("aa:aa:aa:aa:aa:aa")), false)
            .unwrap();
        assert!(c
            .relocate_by_mac([("aa:aa:aa:aa:aa:aa", "10.0.0.6")])
            .is_empty());
    }

    fn two() -> CliConfig {
        let mut c = CliConfig::default();
        c.upsert("lobby", entry("10.0.0.1", Some("aa:aa:aa:aa:aa:aa")), false)
            .unwrap();
        c.upsert("office", entry("10.0.0.2", None), false).unwrap();
        c
    }

    #[test]
    fn path_precedence() {
        assert_eq!(
            path_from(Some("/x/c.json"), Some("/xdg"), Some("/home/u")),
            Some("/x/c.json".into())
        );
        assert_eq!(
            path_from(None, Some("/xdg"), Some("/home/u")),
            Some("/xdg/emdx/config.json".into())
        );
        assert_eq!(
            path_from(Some(""), Some(""), Some("/home/u")),
            Some("/home/u/.config/emdx/config.json".into())
        );
        assert_eq!(path_from(None, None, None), None);
    }

    #[test]
    fn names_are_validated() {
        for ok in ["lobby", "Office_2", "a", "x-y"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "has space", "../etc", "é", &"x".repeat(33)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn first_display_becomes_default() {
        let c = two();
        assert_eq!(c.default.as_deref(), Some("lobby"));
        assert_eq!(c.default_name(), Some("lobby"));
        assert_eq!(c.names(), ["lobby", "office"]);
    }

    #[test]
    fn upsert_with_default_flag_and_replace() {
        let mut c = two();
        c.upsert("office", entry("10.0.0.9", None), true).unwrap();
        assert_eq!(c.default_name(), Some("office"));
        assert_eq!(c.get("office").unwrap().host, "10.0.0.9");
        assert!(c.upsert("bad name", entry("h", None), false).is_err());
    }

    #[test]
    fn removing_the_default_hands_it_over() {
        let mut c = two();
        c.remove("lobby").unwrap();
        assert_eq!(c.default.as_deref(), Some("office"));
        c.remove("office").unwrap();
        assert_eq!(c.default, None);
        assert!(c
            .remove("office")
            .unwrap_err()
            .to_string()
            .contains("none saved"));
    }

    #[test]
    fn rename_keeps_default() {
        let mut c = two();
        c.rename("lobby", "hall").unwrap();
        assert_eq!(c.default.as_deref(), Some("hall"));
        assert_eq!(c.get("hall").unwrap().host, "10.0.0.1");
        assert!(c.rename("hall", "office").is_err(), "target exists");
        assert!(c.rename("nope", "x").is_err());
    }

    #[test]
    fn set_default_requires_known_name() {
        let mut c = two();
        c.set_default("office").unwrap();
        assert_eq!(c.default_name(), Some("office"));
        let err = c.set_default("garage").unwrap_err().to_string();
        assert!(err.contains("saved: lobby, office"), "{err}");
    }

    #[test]
    fn loads_legacy_single_display_file() {
        let c = CliConfig::from_json(json!({ "host": "192.168.1.34", "pin": "1", "mac": "m" }))
            .unwrap();
        assert_eq!(c.default_name(), Some(LEGACY_NAME));
        assert_eq!(c.get(LEGACY_NAME).unwrap().host, "192.168.1.34");
        assert!(CliConfig::from_json(json!({ "nonsense": true })).is_err());
        assert!(CliConfig::from_json(
            json!({ "displays": { "bad name": { "host": "h", "pin": "p" } } })
        )
        .is_err());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.json");
        assert_eq!(load(&path).unwrap(), CliConfig::default());
        save(&path, &two()).unwrap();
        assert_eq!(load(&path).unwrap(), two());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"default\": \"lobby\""), "{text}");
    }

    #[test]
    fn upgrades_legacy_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"host":"h","pin":"p"}"#).unwrap();
        let c = load(&path).unwrap();
        save(&path, &c).unwrap();
        assert_eq!(load(&path).unwrap().get(LEGACY_NAME).unwrap().host, "h");
        assert!(fs::read_to_string(&path).unwrap().contains("\"displays\""));
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        save(&path, &two()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn invalid_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, "not json").unwrap();
        assert!(load(&path)
            .unwrap_err()
            .to_string()
            .contains("invalid config file"));
    }

    #[test]
    fn resolve_uses_default_or_named_display() {
        let c = two();
        let t = resolve(None, None, None, None, &c).unwrap();
        assert_eq!(
            (t.name.as_deref(), t.host.as_str()),
            (Some("lobby"), "10.0.0.1")
        );
        let t = resolve(Some("office"), None, None, None, &c).unwrap();
        assert_eq!(
            (t.name.as_deref(), t.host.as_str(), t.mac),
            (Some("office"), "10.0.0.2", None)
        );
        let err = resolve(Some("garage"), None, None, None, &c)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown display \"garage\""), "{err}");
    }

    #[test]
    fn resolve_flags_override_fields() {
        let c = two();
        let t = resolve(
            Some("lobby"),
            Some("10.9.9.9".into()),
            None,
            Some("bb:bb:bb:bb:bb:bb".into()),
            &c,
        )
        .unwrap();
        assert_eq!(
            (t.host.as_str(), t.pin.as_str(), t.mac.as_deref()),
            ("10.9.9.9", "123456", Some("bb:bb:bb:bb:bb:bb"))
        );
        // host + pin alone don't touch saved displays
        let t = resolve(None, Some("h".into()), Some("p".into()), None, &c).unwrap();
        assert_eq!((t.name, t.mac), (None, None));
    }

    #[test]
    fn resolve_without_default_asks_to_choose() {
        let mut c = two();
        c.default = Some("gone".into());
        let err = resolve(None, None, None, None, &c).unwrap_err().to_string();
        assert!(
            err.contains("several displays are saved (lobby, office)"),
            "{err}"
        );
        let err = resolve(None, None, None, None, &CliConfig::default())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("emdx add") && err.contains("--host and --display-pin"),
            "{err}"
        );
        let err = resolve(None, Some("h".into()), None, None, &CliConfig::default())
            .unwrap_err()
            .to_string();
        assert!(err.ends_with("pass --display-pin"), "{err}");
    }

    #[test]
    fn wake_mac_resolution() {
        let c = two();
        assert_eq!(resolve_mac(None, None, &c).unwrap(), "aa:aa:aa:aa:aa:aa");
        assert_eq!(
            resolve_mac(Some("office"), Some("x".into()), &c).unwrap(),
            "x"
        );
        let err = resolve_mac(Some("office"), None, &c)
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"office\" has no MAC"), "{err}");
        assert!(resolve_mac(None, None, &CliConfig::default()).is_err());
    }

    #[test]
    fn pin_masking() {
        assert_eq!(mask_pin("123456"), "1****6");
        assert_eq!(mask_pin("12"), "**");
        assert_eq!(mask_pin(""), "");
    }

    #[test]
    fn menu_choices() {
        assert_eq!(parse_choice(" 2 ", 3), Some(1));
        assert_eq!(parse_choice("0", 3), None);
        assert_eq!(parse_choice("4", 3), None);
        assert_eq!(parse_choice("x", 3), None);
    }

    #[test]
    fn name_suggestions() {
        assert_eq!(suggest_name("Samsung EMDX 743D"), "samsung-emdx-743d");
        assert_eq!(suggest_name("  Lobby / North  "), "lobby-north");
        assert_eq!(suggest_name("***"), LEGACY_NAME);
        assert!(validate_name(&suggest_name(&"long name ".repeat(10))).is_ok());
        let c = two();
        assert_eq!(unique_name("hall", &c), "hall");
        assert_eq!(unique_name("lobby", &c), "lobby-2");
        let mut c = c;
        c.upsert("lobby-2", entry("h", None), false).unwrap();
        assert_eq!(unique_name("lobby", &c), "lobby-3");
        assert!(validate_name(&unique_name(&suggest_name(&"x".repeat(40)), &c)).is_ok());
    }

    #[test]
    fn targets_cover_every_display() {
        let targets = two().targets();
        let summary: Vec<_> = targets
            .iter()
            .map(|t| {
                (
                    t.name.as_deref().unwrap(),
                    t.host.as_str(),
                    t.mac.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("lobby", "10.0.0.1", Some("aa:aa:aa:aa:aa:aa")),
                ("office", "10.0.0.2", None)
            ]
        );
        assert!(CliConfig::default().targets().is_empty());
    }
}
