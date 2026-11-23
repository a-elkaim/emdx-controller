//! `emdx add`: add displays by scanning the network, or by entering
//! every detail manually.
//!
//! The dialog talks through [`Console`] and reaches the network through
//! [`Probe`], so the whole conversation is unit-tested with scripted
//! answers and a fake network.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use emdx_controller::{
    discovery::{self, Discovered},
    mdc::{Device, DEFAULT_CONNECT_TIMEOUT},
    wol,
};

use crate::config::{self, CliConfig, DisplayEntry};

// ─── I/O seams ───────────────────────────────────────────────────────────────

pub(crate) trait Console {
    /// Ask a question and return the trimmed answer (error on end of input).
    fn ask(&mut self, question: &str) -> Result<String>;
    /// Ask for a secret without echoing it; the answer is kept as typed (not trimmed).
    fn ask_secret(&mut self, question: &str) -> Result<String> {
        self.ask(question)
    }
    fn say(&mut self, line: &str);
}

pub(crate) struct Terminal;

impl Console for Terminal {
    fn ask(&mut self, question: &str) -> Result<String> {
        use std::io::Write;
        print!("{question}");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            bail!("setup cancelled (end of input)");
        }
        Ok(line.trim().to_string())
    }

    fn ask_secret(&mut self, question: &str) -> Result<String> {
        rpassword::prompt_password(question).context("reading the answer")
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

pub(crate) trait Probe {
    async fn scan(&mut self) -> Result<Vec<Discovered>>;
    /// Connect with the PIN; returns the display's own name.
    async fn check(&mut self, host: &str, pin: &str) -> Result<String>;
    /// The host's MAC, learned from the ARP cache (local network only).
    async fn mac_of(&mut self, host: &str) -> Option<String>;
}

pub(crate) struct Network;

impl Probe for Network {
    async fn scan(&mut self) -> Result<Vec<Discovered>> {
        discovery::discover(Duration::from_secs(4), |_| {}).await
    }

    async fn check(&mut self, host: &str, pin: &str) -> Result<String> {
        Ok(Device::new(host, pin)
            .with_session(DEFAULT_CONNECT_TIMEOUT, async |s| s.device_name().await)
            .await?)
    }

    async fn mac_of(&mut self, host: &str) -> Option<String> {
        if let Some(mac) = discovery::lookup_mac(host).await {
            return Some(mac);
        }
        // No recent traffic (e.g. --no-verify): touch the MDC port so the
        // OS resolves the address, then look again
        discovery::is_port_open(
            host,
            emdx_controller::mdc::DEFAULT_PORT,
            Duration::from_secs(2),
        )
        .await;
        discovery::lookup_mac(host).await
    }
}

/// MAC for a display we just reached: an explicit value wins, then the ARP
/// cache, then the saved value; interactively, ask (optional) as a last resort.
async fn resolve_mac(
    con: &mut impl Console,
    probe: &mut impl Probe,
    interactive: bool,
    host: &str,
    explicit: Option<&str>,
    current: Option<&str>,
) -> Result<Option<String>> {
    if let Some(mac) = explicit {
        wol::parse_mac(mac)?;
        return Ok(Some(mac.to_string()));
    }
    if let Some(mac) = probe.mac_of(host).await {
        con.say(&format!("🔎 MAC address detected: {mac}"));
        return Ok(Some(mac));
    }
    if let Some(mac) = current {
        return Ok(Some(mac.to_string()));
    }
    if interactive {
        con.say("Couldn't detect the MAC address (the display may be on another subnet).");
        return ask_mac(con, "MAC address for Wake-on-LAN", None, true);
    }
    con.say("⚠️  MAC address unknown: `emdx wake` won't work for this display until you add one with --mac.");
    Ok(None)
}

// ─── Options ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Scan,
    Manual,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    pub name: Option<String>,
    pub host: Option<String>,
    pub display_pin: Option<String>,
    pub mac: Option<String>,
    /// Forced mode (`--scan` / `--manual`); asked when `None`
    pub mode: Option<Mode>,
    pub make_default: bool,
    pub verify: bool,
    pub interactive: bool,
}

/// Which flow to run. Connection flags imply manual mode; without a
/// terminal, scanning (which needs choices) is not possible.
pub(crate) fn decide_mode(opts: &Options) -> Result<Option<Mode>> {
    let has_details = opts.host.is_some() || opts.display_pin.is_some() || opts.mac.is_some();
    if opts.mode == Some(Mode::Scan) && has_details {
        bail!("--scan can't be combined with --host/--display-pin/--mac");
    }
    if has_details {
        return Ok(Some(Mode::Manual));
    }
    if !opts.interactive {
        if opts.mode == Some(Mode::Scan) {
            bail!("scan mode needs a terminal to choose displays; use NAME --host --display-pin");
        }
        bail!(
            "no terminal to ask questions: pass the details, e.g. \
             `emdx add lobby --host 192.168.1.34 --display-pin 000000`"
        );
    }
    Ok(opts.mode) // None → ask "scan?"
}

/// Persist after every added display so an aborted session keeps earlier ones.
pub(crate) type Saver<'a> = &'a mut dyn FnMut(&CliConfig) -> Result<()>;

// ─── Prompts ─────────────────────────────────────────────────────────────────

pub(crate) fn confirm(con: &mut impl Console, question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    loop {
        match con
            .ask(&format!("{question} {hint} "))?
            .to_ascii_lowercase()
            .as_str()
        {
            "" => return Ok(default_yes),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => con.say("Please answer y or n."),
        }
    }
}

/// Ask until the answer is non-empty (or `default` is used for an empty answer).
pub(crate) fn ask_required(
    con: &mut impl Console,
    question: &str,
    default: Option<&str>,
) -> Result<String> {
    let hint = default.map(|d| format!(" [{d}]")).unwrap_or_default();
    loop {
        let answer = con.ask(&format!("{question}{hint}: "))?;
        match (answer.is_empty(), default) {
            (false, _) => return Ok(answer),
            (true, Some(d)) => return Ok(d.to_string()),
            (true, None) => con.say("This is required."),
        }
    }
}

pub(crate) fn ask_name(
    con: &mut impl Console,
    saved: &CliConfig,
    suggestion: Option<&str>,
    allow_existing: bool,
) -> Result<String> {
    loop {
        let name = ask_required(con, "Name for this display", suggestion)?;
        if let Err(e) = config::validate_name(&name) {
            con.say(&format!("{e}"));
            continue;
        }
        if !allow_existing && saved.displays.contains_key(&name) {
            con.say(&format!("\"{name}\" is already used; pick another name."));
            continue;
        }
        return Ok(name);
    }
}

fn ask_mac(
    con: &mut impl Console,
    question: &str,
    default: Option<&str>,
    optional: bool,
) -> Result<Option<String>> {
    loop {
        let hint = match (default, optional) {
            (Some(d), _) => format!(" [{d}]"),
            (None, true) => " (Enter to skip)".into(),
            (None, false) => String::new(),
        };
        let answer = con.ask(&format!("{question}{hint}: "))?;
        let value = match (answer.as_str(), default) {
            ("", Some(d)) => d.to_string(),
            ("", None) if optional => return Ok(None),
            ("", None) => {
                con.say("This is required.");
                continue;
            }
            (a, _) => a.to_string(),
        };
        match wol::parse_mac(&value) {
            Ok(_) => return Ok(Some(value)),
            Err(e) => con.say(&format!("{e}")),
        }
    }
}

/// Check the PIN by connecting. Interactively a failure offers another PIN;
/// returns the PIN that worked, or `None` when the user gives up.
async fn verify_pin(
    con: &mut impl Console,
    probe: &mut impl Probe,
    opts: &Options,
    host: &str,
    mut pin: String,
) -> Result<Option<String>> {
    if !opts.verify {
        return Ok(Some(pin));
    }
    loop {
        con.say(&format!("🔄 Checking {host}..."));
        match probe.check(host, &pin).await {
            Ok(device_name) => {
                con.say(&format!("✅ Connected to \"{device_name}\""));
                return Ok(Some(pin));
            }
            Err(e) if opts.interactive => {
                con.say(&format!("❌ Could not connect: {e:#}"));
                if !confirm(con, "Try another PIN?", true)? {
                    return Ok(None);
                }
                pin = ask_required(con, "Display PIN", None)?;
            }
            Err(e) => bail!(
                "could not connect to {host}: {e:#}\n   Nothing was saved. Check the IP and PIN \
                 (a sleeping display must be woken first), or use --no-verify to save anyway."
            ),
        }
    }
}

fn store(
    con: &mut impl Console,
    saved: &mut CliConfig,
    save: Saver,
    name: &str,
    entry: DisplayEntry,
    make_default: bool,
) -> Result<()> {
    saved.upsert(name, entry, make_default)?;
    save(saved)?;
    let default = if saved.default_name() == Some(name) {
        " (default)"
    } else {
        ""
    };
    con.say(&format!("💾 Saved \"{name}\"{default}"));
    Ok(())
}

// ─── Flows ───────────────────────────────────────────────────────────────────

/// Run setup; returns the names that were saved.
pub(crate) async fn run(
    con: &mut impl Console,
    probe: &mut impl Probe,
    saved: &mut CliConfig,
    save: Saver<'_>,
    opts: &Options,
) -> Result<Vec<String>> {
    let mode = match decide_mode(opts)? {
        Some(mode) => mode,
        None => {
            if confirm(con, "Scan the network for displays?", true)? {
                Mode::Scan
            } else {
                Mode::Manual
            }
        }
    };
    match mode {
        Mode::Scan => scan(con, probe, saved, save, opts).await,
        Mode::Manual => manual(con, probe, saved, save, opts)
            .await
            .map(|n| n.into_iter().collect()),
    }
}

async fn scan(
    con: &mut impl Console,
    probe: &mut impl Probe,
    saved: &mut CliConfig,
    save: Saver<'_>,
    opts: &Options,
) -> Result<Vec<String>> {
    con.say("🔍 Scanning the network...");
    let found = probe.scan().await?;
    if found.is_empty() {
        con.say("No displays found. Sleeping displays don't answer — wake them first.");
        if confirm(con, "Enter the details manually instead?", true)? {
            return manual(con, probe, saved, save, opts)
                .await
                .map(|n| n.into_iter().collect());
        }
        return Ok(Vec::new());
    }

    let saved_as = |d: &Discovered, saved: &CliConfig| {
        saved
            .displays
            .iter()
            .find(|(_, e)| e.host == d.host)
            .map(|(n, _)| n.clone())
    };
    con.say(&format!(
        "Found {} screen{}:",
        found.len(),
        if found.len() == 1 { "" } else { "s" }
    ));
    for (i, d) in found.iter().enumerate() {
        let tag = saved_as(d, saved)
            .map(|n| format!("  — already saved as \"{n}\""))
            .unwrap_or_default();
        con.say(&format!(
            "  {}. {} ({}) — {}{}{tag}",
            i + 1,
            d.name,
            d.model,
            d.host,
            d.mac
                .as_deref()
                .map(|m| format!(", MAC {m}"))
                .unwrap_or_default()
        ));
    }
    let mut pending: Vec<usize> = (0..found.len())
        .filter(|&i| saved_as(&found[i], saved).is_none())
        .collect();
    if pending.is_empty() {
        con.say("All of them are already saved. Use `emdx add <name> --manual` to change one.");
        return Ok(Vec::new());
    }

    // First pick
    let first = if found.len() == 1 {
        if !confirm(con, "Add it?", true)? {
            return Ok(Vec::new());
        }
        0
    } else {
        loop {
            let answer = con.ask(&format!(
                "Select the one you want to add [1-{}]: ",
                found.len()
            ))?;
            match config::parse_choice(&answer, found.len()) {
                Some(i) if pending.contains(&i) => break i,
                Some(i) => con.say(&format!(
                    "That one is already saved as \"{}\".",
                    saved_as(&found[i], saved).unwrap_or_default()
                )),
                None => con.say("Please enter one of the numbers above."),
            }
        }
    };
    pending.retain(|&i| i != first);

    let mut added = Vec::new();
    if let Some(name) = add_discovered(
        con,
        probe,
        saved,
        save,
        opts,
        &found[first],
        opts.make_default,
    )
    .await?
    {
        added.push(name);
    }

    if !pending.is_empty() {
        let others = pending.len();
        let question = format!(
            "Add the other {others} screen{} too?",
            if others == 1 { "" } else { "s" }
        );
        if confirm(con, &question, false)? {
            for i in pending {
                let d = &found[i];
                con.say(&format!("── {} ({})", d.name, d.host));
                if let Some(name) = add_discovered(con, probe, saved, save, opts, d, false).await? {
                    added.push(name);
                }
            }
        }
    }
    Ok(added)
}

async fn add_discovered(
    con: &mut impl Console,
    probe: &mut impl Probe,
    saved: &mut CliConfig,
    save: Saver<'_>,
    opts: &Options,
    d: &Discovered,
    make_default: bool,
) -> Result<Option<String>> {
    let suggestion = config::unique_name(&config::suggest_name(&d.name), saved);
    let name = ask_name(con, saved, Some(&suggestion), false)?;
    let pin = ask_required(
        con,
        &format!(
            "Display PIN for \"{name}\" (set during its setup, not the one on a setup screen)"
        ),
        None,
    )?;
    let Some(pin) = verify_pin(con, probe, opts, &d.host, pin).await? else {
        con.say(&format!("Skipped \"{name}\" (not saved)."));
        return Ok(None);
    };
    let mac = match &d.mac {
        Some(mac) => Some(mac.clone()),
        None => resolve_mac(con, probe, opts.interactive, &d.host, None, None).await?,
    };
    let entry = DisplayEntry {
        host: d.host.clone(),
        pin,
        mac,
    };
    store(con, saved, save, &name, entry, make_default)?;
    Ok(Some(name))
}

/// Name, IP and PIN entered by hand (or passed as flags); the MAC is detected
/// after connecting. When `NAME` already exists, its saved values are the defaults.
async fn manual(
    con: &mut impl Console,
    probe: &mut impl Probe,
    saved: &mut CliConfig,
    save: Saver<'_>,
    opts: &Options,
) -> Result<Option<String>> {
    let interactive = opts.interactive;
    let name = match &opts.name {
        Some(n) => {
            config::validate_name(n)?;
            n.clone()
        }
        None if interactive => {
            let suggestion = saved.displays.is_empty().then_some(config::LEGACY_NAME);
            ask_name(con, saved, suggestion, true)?
        }
        None => bail!("a display name is required: `emdx add <name> --host … --display-pin …`"),
    };
    let current = saved.displays.get(&name).cloned();
    if interactive {
        con.say(&match &current {
            Some(_) => format!("Updating \"{name}\" (Enter keeps the current value)"),
            None => format!("Adding \"{name}\""),
        });
    }
    let cur = current.clone().unwrap_or_default();
    let cur_or = |v: &str| (!v.is_empty()).then(|| v.to_string());

    let host = match &opts.host {
        Some(h) => h.clone(),
        None if interactive => ask_required(con, "IP address", cur_or(&cur.host).as_deref())?,
        None => cur_or(&cur.host).context("--host is required")?,
    };
    if let Some(m) = &opts.mac {
        wol::parse_mac(m)?; // fail before connecting
    }
    let pin = match &opts.display_pin {
        Some(p) if !p.is_empty() => p.clone(),
        Some(_) => bail!("--display-pin can't be empty"),
        None if interactive => {
            let keep = cur_or(&cur.pin).map(|_| "keep current");
            let answer = ask_required(con, "Display PIN", keep)?;
            if answer == "keep current" {
                cur.pin.clone()
            } else {
                answer
            }
        }
        None => cur_or(&cur.pin).context("--display-pin is required")?,
    };

    let Some(pin) = verify_pin(con, probe, opts, &host, pin).await? else {
        con.say("Nothing was saved.");
        return Ok(None);
    };
    // Connecting put the display in the ARP cache: read its MAC from there
    let mac = resolve_mac(
        con,
        probe,
        interactive,
        &host,
        opts.mac.as_deref(),
        cur.mac.as_deref(),
    )
    .await?;
    store(
        con,
        saved,
        save,
        &name,
        DisplayEntry { host, pin, mac },
        opts.make_default,
    )?;
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Script {
        answers: VecDeque<&'static str>,
        transcript: Vec<String>,
    }

    impl Script {
        fn new(answers: &[&'static str]) -> Script {
            Script {
                answers: answers.iter().copied().collect(),
                transcript: Vec::new(),
            }
        }
        fn saw(&self, needle: &str) -> bool {
            self.transcript.iter().any(|l| l.contains(needle))
        }
    }

    impl Console for Script {
        fn ask(&mut self, question: &str) -> Result<String> {
            self.transcript.push(question.to_string());
            match self.answers.pop_front() {
                Some(a) => Ok(a.to_string()),
                None => bail!("script ran out of answers at: {question}"),
            }
        }
        fn say(&mut self, line: &str) {
            self.transcript.push(line.to_string());
        }
    }

    struct FakeNet {
        found: Vec<Discovered>,
        good_pin: &'static str,
        checks: Vec<(String, String)>,
        /// The OS ARP cache: host → MAC
        arp: Vec<(&'static str, &'static str)>,
    }

    impl Probe for FakeNet {
        async fn scan(&mut self) -> Result<Vec<Discovered>> {
            Ok(self.found.clone())
        }
        async fn check(&mut self, host: &str, pin: &str) -> Result<String> {
            self.checks.push((host.into(), pin.into()));
            if pin == self.good_pin {
                Ok(format!("EMDX at {host}"))
            } else {
                bail!("Authentication Failed: Incorrect PIN")
            }
        }
        async fn mac_of(&mut self, host: &str) -> Option<String> {
            self.arp
                .iter()
                .find(|(h, _)| *h == host)
                .map(|(_, m)| m.to_string())
        }
    }

    fn screen(name: &str, host: &str, mac: Option<&str>) -> Discovered {
        Discovered {
            host: host.into(),
            name: name.into(),
            model: "EM32DX".into(),
            serial: None,
            mac: mac.map(Into::into),
            location: String::new(),
        }
    }

    fn net(found: Vec<Discovered>) -> FakeNet {
        FakeNet {
            found,
            good_pin: "123456",
            checks: Vec::new(),
            arp: Vec::new(),
        }
    }

    fn with_arp(mut n: FakeNet, host: &'static str, mac: &'static str) -> FakeNet {
        n.arp.push((host, mac));
        n
    }

    fn interactive() -> Options {
        Options {
            interactive: true,
            verify: true,
            ..Options::default()
        }
    }

    async fn go(
        con: &mut Script,
        probe: &mut FakeNet,
        saved: &mut CliConfig,
        opts: &Options,
    ) -> (Result<Vec<String>>, usize) {
        let mut saves = 0;
        let mut save = |_: &CliConfig| -> Result<()> {
            saves += 1;
            Ok(())
        };
        let r = run(con, probe, saved, &mut save, opts).await;
        (r, saves)
    }

    #[tokio::test]
    async fn scan_adds_the_only_screen_found() {
        let mut con = Script::new(&["", "", "", "123456"]); // scan? Y · add it? Y · name [suggested] · PIN
        let mut probe = net(vec![screen(
            "Samsung EMDX 743D",
            "192.168.1.34",
            Some("B0:F2:F6:55:0D:0B"),
        )]);
        let mut saved = CliConfig::default();
        let (r, saves) = go(&mut con, &mut probe, &mut saved, &interactive()).await;
        assert_eq!(r.unwrap(), ["samsung-emdx-743d"]);
        assert_eq!(saves, 1);
        assert!(con.saw("Found 1 screen:"));
        let e = saved.get("samsung-emdx-743d").unwrap();
        assert_eq!(
            (e.host.as_str(), e.pin.as_str(), e.mac.as_deref()),
            ("192.168.1.34", "123456", Some("B0:F2:F6:55:0D:0B"))
        );
        assert_eq!(saved.default_name(), Some("samsung-emdx-743d"));
        assert_eq!(
            probe.checks,
            [("192.168.1.34".to_string(), "123456".to_string())]
        );
    }

    #[tokio::test]
    async fn scan_selects_one_then_adds_the_others() {
        let mut saved = CliConfig::default();
        saved
            .upsert(
                "lobby",
                DisplayEntry {
                    host: "10.0.0.1".into(),
                    pin: "1".into(),
                    mac: None,
                },
                false,
            )
            .unwrap();
        let mut probe = net(vec![
            screen("Lobby", "10.0.0.1", Some("AA:AA:AA:AA:AA:AA")),
            screen("Office", "10.0.0.2", Some("BB:BB:BB:BB:BB:BB")),
            screen("Hall", "10.0.0.3", None),
        ]);
        let mut con = Script::new(&[
            "y",                 // scan?
            "1",                 // already saved → re-asked
            "2",                 // Office
            "office",            // name
            "123456",            // PIN
            "y",                 // add the other 1 screen?
            "",                  // name [hall]
            "123456",            // PIN
            "cc:cc:cc:cc:cc:cc", // MAC not found by scan
        ]);
        let (r, saves) = go(&mut con, &mut probe, &mut saved, &interactive()).await;
        assert_eq!(r.unwrap(), ["office", "hall"]);
        assert_eq!(saves, 2);
        assert!(con.saw("Found 3 screens:"));
        assert!(con.saw("already saved as \"lobby\""));
        assert!(con.saw("Add the other 1 screen too?"));
        assert_eq!(saved.names(), ["hall", "lobby", "office"]);
        assert_eq!(
            saved.get("hall").unwrap().mac.as_deref(),
            Some("cc:cc:cc:cc:cc:cc")
        );
        assert_eq!(saved.default_name(), Some("lobby"), "existing default kept");
    }

    #[tokio::test]
    async fn declining_the_others_stops_after_the_first() {
        let mut probe = net(vec![
            screen("A", "10.0.0.1", Some("AA:AA:AA:AA:AA:AA")),
            screen("B", "10.0.0.2", None),
        ]);
        let mut con = Script::new(&["", "1", "", "123456", ""]); // last: add others? [y/N] → no
        let mut saved = CliConfig::default();
        let (r, _) = go(&mut con, &mut probe, &mut saved, &interactive()).await;
        assert_eq!(r.unwrap(), ["a"]);
        assert_eq!(saved.names(), ["a"]);
    }

    #[tokio::test]
    async fn wrong_pin_can_be_retried_or_skipped() {
        let mut probe = net(vec![screen("A", "10.0.0.1", Some("AA:AA:AA:AA:AA:AA"))]);
        let mut con = Script::new(&["", "", "", "000000", "", "123456"]); // wrong PIN, retry? Y, right PIN
        let mut saved = CliConfig::default();
        let (r, saves) = go(&mut con, &mut probe, &mut saved, &interactive()).await;
        assert_eq!(r.unwrap(), ["a"]);
        assert_eq!((saves, probe.checks.len()), (1, 2));
        assert!(con.saw("Incorrect PIN"));

        let mut probe = net(vec![screen("A", "10.0.0.1", Some("AA:AA:AA:AA:AA:AA"))]);
        let mut con = Script::new(&["", "", "", "000000", "n"]);
        let mut saved = CliConfig::default();
        let (r, saves) = go(&mut con, &mut probe, &mut saved, &interactive()).await;
        assert!(r.unwrap().is_empty());
        assert_eq!(saves, 0);
        assert!(con.saw("Skipped \"a\" (not saved)."));
    }

    #[tokio::test]
    async fn names_are_validated_and_unique() {
        let mut saved = CliConfig::default();
        saved
            .upsert(
                "office",
                DisplayEntry {
                    host: "10.0.0.9".into(),
                    pin: "1".into(),
                    mac: None,
                },
                false,
            )
            .unwrap();
        let mut probe = net(vec![screen(
            "Office",
            "10.0.0.2",
            Some("AA:AA:AA:AA:AA:AA"),
        )]);
        let mut con = Script::new(&["", "", "bad name", "office", "", "123456"]);
        let (r, _) = go(&mut con, &mut probe, &mut saved, &interactive()).await;
        assert_eq!(r.unwrap(), ["office-2"], "suggestion avoids the taken name");
        assert!(con.saw("invalid display name"));
        assert!(con.saw("\"office\" is already used"));
    }

    #[tokio::test]
    async fn nothing_found_offers_manual_entry() {
        let mut probe = with_arp(net(vec![]), "10.0.0.5", "AA:BB:CC:DD:EE:FF");
        // scan? Y · manual instead? Y · name [display] · IP · PIN
        let mut con = Script::new(&["", "", "", "10.0.0.5", "123456"]);
        let mut saved = CliConfig::default();
        let (r, _) = go(&mut con, &mut probe, &mut saved, &interactive()).await;
        assert_eq!(r.unwrap(), ["display"]);
        assert!(con.saw("No displays found"));
        assert!(
            !con.transcript.iter().any(|l| l.starts_with("MAC address")),
            "MAC is not asked"
        );
        assert!(con.saw("MAC address detected: AA:BB:CC:DD:EE:FF"));
        assert_eq!(
            saved.get("display").unwrap().mac.as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
    }

    #[tokio::test]
    async fn manual_mode_detects_the_mac_after_connecting() {
        let mut probe = with_arp(net(vec![]), "192.168.1.34", "B0:F2:F6:55:0D:0B");
        let mut con = Script::new(&["n", "lobby", "192.168.1.34", "123456"]);
        let mut saved = CliConfig::default();
        let (r, _) = go(&mut con, &mut probe, &mut saved, &interactive()).await;
        assert_eq!(r.unwrap(), ["lobby"]);
        assert!(!con.saw("Scanning"), "scan was declined");
        let e = saved.get("lobby").unwrap();
        assert_eq!(
            (e.host.as_str(), e.mac.as_deref()),
            ("192.168.1.34", Some("B0:F2:F6:55:0D:0B"))
        );
    }

    #[tokio::test]
    async fn undetectable_mac_is_asked_and_optional() {
        // Another subnet: nothing in the ARP cache
        let mut con = Script::new(&["n", "far", "10.9.0.5", "123456", "zz", ""]);
        let mut saved = CliConfig::default();
        let (r, _) = go(&mut con, &mut net(vec![]), &mut saved, &interactive()).await;
        assert_eq!(r.unwrap(), ["far"]);
        assert!(con.saw("Couldn't detect the MAC address"));
        assert!(con.saw("invalid MAC address"));
        assert_eq!(saved.get("far").unwrap().mac, None, "Enter skipped it");

        let mut con = Script::new(&["n", "far2", "10.9.0.6", "123456", "cc:cc:cc:cc:cc:cc"]);
        let (r, _) = go(&mut con, &mut net(vec![]), &mut saved, &interactive()).await;
        assert_eq!(r.unwrap(), ["far2"]);
        assert_eq!(
            saved.get("far2").unwrap().mac.as_deref(),
            Some("cc:cc:cc:cc:cc:cc")
        );
    }

    #[tokio::test]
    async fn explicit_mac_wins_over_detection() {
        let opts = Options {
            name: Some("lobby".into()),
            host: Some("10.0.0.1".into()),
            display_pin: Some("123456".into()),
            mac: Some("11:11:11:11:11:11".into()),
            verify: true,
            ..Options::default()
        };
        let mut probe = with_arp(net(vec![]), "10.0.0.1", "AA:AA:AA:AA:AA:AA");
        let mut saved = CliConfig::default();
        let (r, _) = go(&mut Script::new(&[]), &mut probe, &mut saved, &opts).await;
        assert_eq!(r.unwrap(), ["lobby"]);
        assert_eq!(
            saved.get("lobby").unwrap().mac.as_deref(),
            Some("11:11:11:11:11:11")
        );

        let bad = Options {
            mac: Some("nope".into()),
            ..opts
        };
        let mut untouched = net(vec![]);
        let (r, _) = go(&mut Script::new(&[]), &mut untouched, &mut saved, &bad).await;
        assert!(r.unwrap_err().to_string().contains("invalid MAC"));
        assert!(
            untouched.checks.is_empty(),
            "bad MAC rejected before connecting"
        );
    }

    #[tokio::test]
    async fn manual_update_keeps_current_values() {
        let mut saved = CliConfig::default();
        saved
            .upsert(
                "lobby",
                DisplayEntry {
                    host: "10.0.0.1".into(),
                    pin: "123456".into(),
                    mac: Some("aa:aa:aa:aa:aa:aa".into()),
                },
                false,
            )
            .unwrap();
        let opts = Options {
            name: Some("lobby".into()),
            mode: Some(Mode::Manual),
            ..interactive()
        };
        let mut con = Script::new(&["10.0.0.7", ""]); // new IP, keep PIN
        let (r, _) = go(&mut con, &mut net(vec![]), &mut saved, &opts).await;
        assert_eq!(r.unwrap(), ["lobby"]);
        assert!(con.saw("Updating \"lobby\""));
        let e = saved.get("lobby").unwrap();
        assert_eq!(
            (e.host.as_str(), e.pin.as_str(), e.mac.as_deref()),
            ("10.0.0.7", "123456", Some("aa:aa:aa:aa:aa:aa")),
            "undetectable → saved MAC kept"
        );

        // A detected MAC replaces the saved one (e.g. the panel was swapped)
        let mut con = Script::new(&["", ""]);
        let mut probe = with_arp(net(vec![]), "10.0.0.7", "BB:BB:BB:BB:BB:BB");
        go(&mut con, &mut probe, &mut saved, &opts).await.0.unwrap();
        assert_eq!(
            saved.get("lobby").unwrap().mac.as_deref(),
            Some("BB:BB:BB:BB:BB:BB")
        );
    }

    #[tokio::test]
    async fn flags_run_without_any_prompt() {
        let opts = Options {
            name: Some("lobby".into()),
            host: Some("10.0.0.1".into()),
            display_pin: Some("123456".into()),
            verify: true,
            ..Options::default()
        };
        let mut con = Script::new(&[]);
        let mut probe = with_arp(net(vec![]), "10.0.0.1", "AA:AA:AA:AA:AA:AA");
        let mut saved = CliConfig::default();
        let (r, saves) = go(&mut con, &mut probe, &mut saved, &opts).await;
        assert_eq!(r.unwrap(), ["lobby"]);
        assert_eq!(saves, 1);
        assert_eq!(
            saved.get("lobby").unwrap().mac.as_deref(),
            Some("AA:AA:AA:AA:AA:AA")
        );
    }

    #[tokio::test]
    async fn non_interactive_rules() {
        let base = Options {
            name: Some("lobby".into()),
            host: Some("h".into()),
            display_pin: Some("123456".into()),
            verify: false,
            ..Options::default()
        };
        // No MAC anywhere: saved anyway, with a warning
        let mut saved = CliConfig::default();
        let mut con = Script::new(&[]);
        let (r, _) = go(&mut con, &mut net(vec![]), &mut saved, &base).await;
        assert_eq!(r.unwrap(), ["lobby"]);
        assert_eq!(saved.get("lobby").unwrap().mac, None);
        assert!(con.saw("`emdx wake` won't work"));

        let no_name = Options {
            name: None,
            ..base.clone()
        };
        let (r, _) = go(
            &mut Script::new(&[]),
            &mut net(vec![]),
            &mut saved,
            &no_name,
        )
        .await;
        assert!(r
            .unwrap_err()
            .to_string()
            .contains("display name is required"));

        let no_host = Options {
            name: Some("x".into()),
            host: None,
            ..base.clone()
        };
        let (r, _) = go(
            &mut Script::new(&[]),
            &mut net(vec![]),
            &mut saved,
            &no_host,
        )
        .await;
        assert!(r.unwrap_err().to_string().contains("--host is required"));

        let (r, _) = go(
            &mut Script::new(&[]),
            &mut net(vec![]),
            &mut saved,
            &Options::default(),
        )
        .await;
        assert!(r.unwrap_err().to_string().contains("no terminal"));

        let wrong_pin = Options {
            display_pin: Some("000000".into()),
            verify: true,
            ..base
        };
        let (r, saves) = go(
            &mut Script::new(&[]),
            &mut net(vec![]),
            &mut saved,
            &wrong_pin,
        )
        .await;
        assert!(r.unwrap_err().to_string().contains("Nothing was saved"));
        assert_eq!(saves, 0);
    }

    #[test]
    fn mode_decision() {
        let ask = Options {
            interactive: true,
            ..Options::default()
        };
        assert_eq!(decide_mode(&ask).unwrap(), None);
        let flags = Options {
            host: Some("h".into()),
            ..Options::default()
        };
        assert_eq!(decide_mode(&flags).unwrap(), Some(Mode::Manual));
        let forced = Options {
            mode: Some(Mode::Scan),
            ..ask.clone()
        };
        assert_eq!(decide_mode(&forced).unwrap(), Some(Mode::Scan));
        let scan_no_tty = Options {
            mode: Some(Mode::Scan),
            ..Options::default()
        };
        assert!(decide_mode(&scan_no_tty)
            .unwrap_err()
            .to_string()
            .contains("needs a terminal"));
        let conflict = Options {
            mode: Some(Mode::Scan),
            display_pin: Some("1".into()),
            ..ask
        };
        assert!(decide_mode(&conflict).is_err());
    }
}
