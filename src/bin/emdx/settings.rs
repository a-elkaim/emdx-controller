//! Display settings and maintenance commands over MDC (from the ePaper app's
//! device page): sleep time, daily refresh, network standby, storage.

use std::{io::IsTerminal, time::Duration};

use anyhow::{bail, Context, Result};

use emdx_controller::{
    mdc::{self, Device, Session, SleepTime, StorageKind, DEFAULT_CONNECT_TIMEOUT},
    wol,
};

use crate::{
    config::Target,
    label,
    setup::{confirm, Terminal},
};

/// Connect, waking the display with Wake-on-LAN first if it doesn't answer
/// (it drops off the network while asleep). A saved display that isn't at its
/// saved address is looked for on the network by its MAC: the router may have
/// given it a new IP, which is then saved.
pub(crate) async fn connect(device: &Target) -> Result<Session> {
    emdx_controller::say!("🔄 Connecting to {}...", label(device));
    let mut dev = Device::new(&device.host, &device.pin);
    match dev.connect(DEFAULT_CONNECT_TIMEOUT).await {
        Ok(s) => Ok(s),
        Err(mdc::MdcError::AuthFailed(why)) => Err(login_failed(why)),
        Err(first) => {
            let Some(mac) = &device.mac else {
                return Err(anyhow::Error::new(first)
                    .context("the display didn't answer; is it asleep? (`emdx wake`)"));
            };
            // Awake at another address?
            if let Some(host) = find_moved(device).await {
                dev = Device::new(&host, &device.pin);
                match dev.connect(DEFAULT_CONNECT_TIMEOUT).await {
                    Ok(s) => return Ok(s),
                    Err(mdc::MdcError::AuthFailed(why)) => return Err(login_failed(why)),
                    Err(_) => {}
                }
            }
            emdx_controller::say!("💤 No answer ({first}); waking it with Wake-on-LAN...");
            wol::wake(mac).await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
            let mut tries = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                tries += 1;
                // Once awake it may answer at a new address: look every third try
                if tries % 3 == 0 {
                    if let Some(host) = find_moved(device).await {
                        dev = Device::new(&host, &device.pin);
                    }
                }
                match dev.connect(DEFAULT_CONNECT_TIMEOUT).await {
                    Ok(s) => {
                        emdx_controller::say!("✅ Connected to {}", label(device));
                        return Ok(s);
                    }
                    // Awake but refusing the PIN: retrying would only count more
                    // wrong PINs towards the lockout
                    Err(mdc::MdcError::AuthFailed(why)) => return Err(login_failed(why)),
                    Err(e) if tokio::time::Instant::now() >= deadline => {
                        return Err(anyhow::Error::new(e)
                            .context("the display didn't wake up within 45 seconds"));
                    }
                    Err(e) => emdx_controller::say!("   still no answer ({e}), retrying..."),
                }
            }
        }
    }
}

/// Search the network for a saved display by its MAC. When it answers at another
/// address, every saved display found at a new address is updated (and saved), and
/// this one's new address is returned.
async fn find_moved(device: &Target) -> Option<String> {
    let name = device.name.as_deref()?;
    emdx_controller::say!("🔍 Looking for {name} on the network by its MAC...");
    let found = emdx_controller::discovery::discover(Duration::from_secs(3), |_| {})
        .await
        .ok()?;
    let path = crate::config::default_path().ok()?;
    let mut cfg = crate::config::load(&path).ok()?;
    let moved = cfg.relocate_by_mac(
        found
            .iter()
            .filter_map(|d| Some((d.mac.as_deref()?, d.host.as_str()))),
    );
    if moved.is_empty() {
        return None;
    }
    if let Err(e) = crate::config::save(&path, &cfg) {
        emdx_controller::say_err!("⚠️  Couldn't save the new address: {e:#}");
    }
    for m in &moved {
        emdx_controller::say!(
            "📍 \"{}\" answered at {} (was {}): saved address updated",
            m.name,
            m.to,
            m.from
        );
    }
    moved.into_iter().find(|m| m.name == name).map(|m| m.to)
}

/// A refused login, with what to do about it. The display answers `FAIL:0x01` for
/// a wrong PIN and `FAIL:0x02` ("Blocked") once too many wrong PINs were tried
/// (the app: "entering PIN more than 5 times"); it accepts logins again later.
pub(crate) fn login_failed(why: &str) -> anyhow::Error {
    let refusal = match why {
        "Blocked" => Some(LoginRefusal::Blocked),
        "Incorrect PIN" => Some(LoginRefusal::WrongPin),
        _ => None,
    };
    let message = login_failed_message(why);
    match refusal {
        // Reachable, but refusing us: the web UI says so instead of "unreachable"
        Some(kind) => anyhow::Error::new(LoginRefused { kind, message }),
        None => anyhow::anyhow!(message),
    }
}

/// Why the display refused a login
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginRefusal {
    WrongPin,
    /// Too many wrong PINs: it refuses logins for a while
    Blocked,
}

/// A display that answered but refused the PIN
#[derive(Debug)]
pub(crate) struct LoginRefused {
    pub(crate) kind: LoginRefusal,
    message: String,
}

impl std::fmt::Display for LoginRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LoginRefused {}

fn login_failed_message(why: &str) -> String {
    match why {
        "Blocked" => "MDC login failed: Blocked. The display refuses logins after too many \
             wrong PINs; wait a while before trying again (each new try with a wrong PIN counts \
             too), and check the saved PIN (`emdx displays`, or EMDX Settings in `emdx serve`)"
            .to_string(),
        "Incorrect PIN" => "MDC login failed: wrong PIN. Check the saved PIN (`emdx displays`, \
             or EMDX Settings in `emdx serve`); after about 5 wrong PINs the display blocks logins"
            .to_string(),
        other => format!("MDC login failed: {other}"),
    }
}

/// How to put a display in pairing mode by hand (shown when none is found).
pub(crate) const PAIRING_MODE_HELP: &str = "To put the display in pairing mode, press the \
    Bluetooth button on the back of the screen: the LED blinks blue. If nothing happens, the \
    display is asleep: press its power button first (the LED turns green while it wakes up), \
    then press the Bluetooth button; the LED should blink blue.";

/// Make a configured display advertise for Bluetooth setup (the app's "Pairing Mode").
pub(crate) async fn pairing_mode(device: &Target, value: u16) -> Result<()> {
    let mut s = connect(device).await?;
    s.turn_on_ble(value).await?;
    s.close().await;
    println!("✅ Pairing mode on: the display's LED should blink blue.");
    println!("   Set it up again with `emdx setup` (or \"Set up a new display\" in `emdx serve`).");
    Ok(())
}

/// What the display does when it is forgotten: the app's "Reset All" (settings back
/// to default, user content deleted). Returns sentences describing it.
///
/// Observed on an EM32DX: "Reset All" keeps the network, the display PIN and the
/// "set up" state. Pairing mode isn't used: a display that was set up before pairs
/// with its display PIN, which is confusing; its own factory reset shows the setup
/// screen with a new pairing PIN instead ([`FACTORY_RESET_HELP`]).
pub(crate) async fn forget_on_display(device: &Target) -> Result<Vec<String>> {
    let mut s = connect(device).await?;
    let confirmed = s.reset(mdc::ResetKind::All).await?;
    s.close().await;
    Ok(vec![
        if confirmed {
            "Settings put back to their defaults and user content deleted (the app's \"Reset All\").".to_string()
        } else {
            "\"Reset All\" sent (the display restarted before confirming): settings back to their \
             defaults, user content deleted."
                .to_string()
        },
        FACTORY_RESET_HELP.to_string(),
    ])
}

/// How to set a forgotten display up again: its own factory reset brings back the
/// setup screen (QR code and a new pairing PIN)
pub(crate) const FACTORY_RESET_HELP: &str = "To set it up again, factory reset it on the \
    display: hold its Bluetooth and Next buttons together for 5 seconds, then wait for the \
    setup screen with the QR code.";

/// `emdx forget`: settings back to default and user content deleted ("Reset All"),
/// then the saved display removed.
pub(crate) async fn forget(device: &Target, yes: bool) -> Result<()> {
    let name = device
        .name
        .as_deref()
        .context("forget works on a saved display: pick one with -d <name>")?;
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("pass --yes to forget {name}");
        }
        println!(
            "Its settings go back to their defaults and its user content is deleted (the app's \
             \"Reset All\"), then it is removed from emdx. {FACTORY_RESET_HELP}"
        );
        if !confirm(&mut Terminal, &format!("Forget {}?", label(device)), false)? {
            println!("Nothing changed.");
            return Ok(());
        }
    }
    let lines = forget_on_display(device).await?;
    for line in lines {
        println!("✅ {line}");
    }
    let path = crate::config::default_path()?;
    let mut cfg = crate::config::load(&path)?;
    cfg.remove(name)?;
    crate::config::save(&path, &cfg)?;
    println!("🗑  \"{name}\" was removed from emdx.");
    Ok(())
}

/// The app's "Reset Settings" (`settings`) and "Reset All" (`all`, factory reset).
pub(crate) async fn reset(device: &Target, kind: mdc::ResetKind, yes: bool) -> Result<()> {
    let (what, consequence) = match kind {
        mdc::ResetKind::Settings => (
            "reset the settings of",
            "The app describes it as \"Reset all device settings to default\". On an EM32DX only the \
             sleep time went back to its default (20 minutes).",
        ),
        mdc::ResetKind::All => (
            "reset to default settings and delete the content of",
            "The app describes it as \"Restores a device to its factory settings\". On an EM32DX it \
             erased the stored content and reset some settings (content player back to USB, daily \
             refresh 01:00), but kept the network, the display PIN and the \"set up\" state, and \
             didn't enter pairing mode."),
    };
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("pass --yes to {what} the display");
        }
        println!("{consequence}");
        if !confirm(
            &mut Terminal,
            &format!("Really {what} {}?", label(device)),
            false,
        )? {
            println!("Nothing was reset.");
            return Ok(());
        }
    }
    let mut s = connect(device).await?;
    let confirmed = s.reset(kind).await?;
    s.close().await;
    println!(
        "{}",
        if confirmed {
            "✅ Reset sent and acknowledged"
        } else {
            "✅ Reset sent (the display restarted before confirming)"
        }
    );
    if kind == mdc::ResetKind::All {
        let name = device.name.as_deref().unwrap_or("<name>");
        println!(
            "   The content player is now USB: run `emdx -d {name} content-player mobile` before \
             sending images.\n   To set it up again instead: `emdx -d {name} pairing-mode`, then \
             `emdx setup` with its display PIN as the pairing PIN."
        );
    }
    Ok(())
}

/// `off`, minutes (`45`, `45min`) or hours and minutes (`1:30`, `1h30`), from
/// 5 minutes to 23 h 59 min
pub(crate) fn parse_sleep_time(value: &str) -> Result<SleepTime> {
    let v = value.trim().to_ascii_lowercase().replace(' ', "");
    if v == "off" || v == "0" {
        return Ok(SleepTime::Off);
    }
    let bad = || {
        anyhow::anyhow!(
            "invalid sleep time {value:?}: off, minutes (45) or hours and minutes (1:30, 1h30), \
             from 5 minutes to 23:59"
        )
    };
    let minutes: u16 = match v.split_once([':', 'h']) {
        Some((h, m)) => {
            let h: u16 = h.parse().map_err(|_| bad())?;
            let m = m.trim_end_matches("min").trim_end_matches('m');
            let m: u16 = if m.is_empty() {
                0
            } else {
                m.parse().map_err(|_| bad())?
            };
            if m > 59 {
                return Err(bad());
            }
            h * 60 + m
        }
        None => v
            .trim_end_matches("min")
            .trim_end_matches('m')
            .parse()
            .map_err(|_| bad())?,
    };
    Ok(SleepTime::from_minutes(minutes)?)
}

pub(crate) async fn sleep_after_update(device: &Target, value: Option<&str>) -> Result<()> {
    let wanted = match value.map(|v| v.trim().to_ascii_lowercase()) {
        None => None,
        Some(v) if v == "on" => Some(true),
        Some(v) if v == "off" => Some(false),
        Some(v) => bail!("invalid value {v:?} (on or off)"),
    };
    let mut s = connect(device).await?;
    if let Some(on) = wanted {
        s.set_immediate_sleep(on).await?;
    }
    let on = s.immediate_sleep().await?;
    s.close().await;
    println!(
        "Sleep right after a content update: {}",
        if on {
            "on (sleeps as soon as new content is shown)"
        } else {
            "off (waits for the sleep time)"
        }
    );
    Ok(())
}

pub(crate) async fn sleep_time(device: &Target, value: Option<&str>) -> Result<()> {
    let wanted = value.map(parse_sleep_time).transpose()?;
    let mut s = connect(device).await?;
    if let Some(t) = wanted {
        if t == SleepTime::Off {
            ensure_plugged_in(&mut s).await?;
        }
        s.set_sleep_time(t).await?;
    }
    println!("Sleep time: {}", s.sleep_time().await?);
    Ok(())
}

/// `HH:MM` → `Some((h, m))`, `now` → `None`.
pub(crate) fn parse_refresh(value: &str) -> Result<Option<(u8, u8)>> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("now") {
        return Ok(None);
    }
    let (h, m) = v
        .split_once(':')
        .with_context(|| format!("invalid time {value:?} (HH:MM or now)"))?;
    let (h, m): (u8, u8) = (
        h.parse().context("invalid hour")?,
        m.parse().context("invalid minute")?,
    );
    if h > 23 || m > 59 {
        bail!("invalid time {value:?}");
    }
    Ok(Some((h, m)))
}

pub(crate) fn describe_refresh(r: &mdc::DailyRefresh) -> String {
    match r.next {
        Some((day, month)) => format!(
            "{:02}:{:02} (next on {day:02}/{month:02})",
            r.hour, r.minute
        ),
        None => format!("every day at {:02}:{:02}", r.hour, r.minute),
    }
}

pub(crate) async fn daily_refresh(device: &Target, value: Option<&str>) -> Result<()> {
    let wanted = value.map(parse_refresh).transpose()?;
    let mut s = connect(device).await?;
    match wanted {
        Some(None) => {
            s.set_daily_refresh(None).await?;
            println!("✅ Refreshing the screen now");
            return Ok(());
        }
        Some(at) => s.set_daily_refresh(at).await?,
        None => {}
    }
    let r = s.daily_refresh().await?;
    println!("Screen protection time: {}", describe_refresh(&r));
    Ok(())
}

pub(crate) async fn network_standby(device: &Target, value: Option<&str>) -> Result<()> {
    let wanted = match value.map(|v| v.trim().to_ascii_lowercase()) {
        None => None,
        Some(v) if v == "on" => Some(true),
        Some(v) if v == "off" => Some(false),
        Some(v) => bail!("invalid value {v:?} (on or off)"),
    };
    let mut s = connect(device).await?;
    if let Some(on) = wanted {
        s.set_network_standby(on).await?;
    }
    let state = s
        .network_standby()
        .await
        .context("the display didn't report its network standby state")?;
    println!("Network standby: {}", if state { "on" } else { "off" });
    if !state {
        println!("   Asleep, the display can only be woken with its button or Wake-on-LAN.");
    }
    Ok(())
}

/// The ePaper app's "Sleep mode" setting. It isn't a display setting of its own:
/// the app derives it from the sleep time and network standby, and sets those two
/// (`PowerAndEnergySavingSetting.SleepModeSetting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SleepMode {
    /// Sleeps after the sleep time, network standby on: Wake-on-LAN wakes it
    Sleep,
    /// Sleeps after the sleep time, network standby off: only its power button wakes it
    DeepSleep,
    /// Never sleeps (sleep time off, network standby on)
    Off,
}

/// The app's message when sleep mode off is picked on battery
pub(crate) const SLEEP_MODE_OFF_NEEDS_POWER: &str = "To turn off sleep mode, connect the \
    device to a power cord. If not connected, it will enter sleep mode after 3 hours.";

/// Never sleeping (sleep mode off, sleep time off) is only for a display on mains
/// power, as in the app. A display that doesn't report its power source is let through.
pub(crate) async fn ensure_plugged_in(s: &mut Session) -> Result<()> {
    if s.battery().await.ok().and_then(|b| b.plugged_in()) == Some(false) {
        bail!("{SLEEP_MODE_OFF_NEEDS_POWER}");
    }
    Ok(())
}

/// Sleep time used when a sleep mode is picked while the display never sleeps:
/// 20 minutes, what "Reset Settings" sets on an EM32DX.
pub(crate) const DEFAULT_SLEEP_TIME: SleepTime = SleepTime::Minutes(20);

impl SleepMode {
    pub(crate) fn parse(v: &str) -> Result<Self> {
        match v.trim().to_ascii_lowercase().as_str() {
            "sleep" | "on" => Ok(Self::Sleep),
            "deep-sleep" | "deep" => Ok(Self::DeepSleep),
            "off" => Ok(Self::Off),
            other => bail!("invalid sleep mode {other:?} (sleep, deep-sleep or off)"),
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::DeepSleep => "deep-sleep",
            Self::Off => "off",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Sleep => "sleep mode (Wake-on-LAN wakes it)",
            Self::DeepSleep => "deep sleep mode (only its power button wakes it)",
            Self::Off => "off (never sleeps)",
        }
    }

    /// The app's rule: a sleep time of "off" means always on, whatever the standby
    pub(crate) fn of(sleep_time: SleepTime, network_standby: bool) -> Self {
        match (sleep_time, network_standby) {
            (SleepTime::Off, _) => Self::Off,
            (_, true) => Self::Sleep,
            (_, false) => Self::DeepSleep,
        }
    }

    /// Set the two settings behind it. Picking a sleep mode on a display that never
    /// sleeps also sets [`DEFAULT_SLEEP_TIME`]; otherwise its sleep time is kept.
    pub(crate) async fn apply(self, s: &mut Session) -> Result<()> {
        match self {
            Self::Off => {
                ensure_plugged_in(s).await?;
                s.set_sleep_time(SleepTime::Off).await?;
                s.set_network_standby(true).await?;
            }
            Self::Sleep | Self::DeepSleep => {
                if s.sleep_time().await? == SleepTime::Off {
                    s.set_sleep_time(DEFAULT_SLEEP_TIME).await?;
                }
                s.set_network_standby(self == Self::Sleep).await?;
            }
        }
        Ok(())
    }

    /// Read it back from the display
    pub(crate) async fn read(s: &mut Session) -> Result<Self> {
        let sleep_time = s.sleep_time().await?;
        let standby = s
            .network_standby()
            .await
            .context("the display didn't report its network standby state")?;
        Ok(Self::of(sleep_time, standby))
    }
}

pub(crate) async fn sleep_mode(device: &Target, value: Option<&str>) -> Result<()> {
    let wanted = value.map(SleepMode::parse).transpose()?;
    let mut s = connect(device).await?;
    if let Some(mode) = wanted {
        mode.apply(&mut s).await?;
    }
    let mode = SleepMode::read(&mut s).await?;
    let sleep_time = s.sleep_time().await.ok();
    s.close().await;
    println!("Sleep mode: {}", mode.label());
    if mode != SleepMode::Off {
        if let Some(t) = sleep_time {
            println!("   Sleeps {t} after new content (`emdx sleep-time` changes it).");
        }
    }
    Ok(())
}

/// A display PIN: exactly 6 digits (the app: "Your PIN must contain at least 6 numbers",
/// and the display's PIN command takes 6)
pub(crate) fn check_new_pin(pin: &str) -> Result<()> {
    if pin.len() != 6 || !pin.bytes().all(|b| b.is_ascii_digit()) {
        bail!("the display PIN must be 6 digits");
    }
    Ok(())
}

/// Change the display PIN like the app's "Change PIN" (`MDCPinCommand.Set`,
/// `1B 87` + the digits), then log in with the new PIN to make sure the display
/// took it. Connecting with the current PIN already proves it is right, which is
/// what the app asks for first. The caller saves the new PIN on success.
pub(crate) async fn change_pin_on_display(device: &Target, new_pin: &str) -> Result<()> {
    check_new_pin(new_pin)?;
    if new_pin == device.pin {
        bail!("that is already the display PIN");
    }
    let mut s = connect(device).await?;
    let sent = s.set_pin(new_pin).await;
    s.close().await;
    sent.context("the display refused the new PIN")?;

    emdx_controller::say!("🔑 Checking that the display takes the new PIN...");
    let with_new = Device::new(&device.host, new_pin);
    let mut last = None;
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        match with_new.connect(DEFAULT_CONNECT_TIMEOUT).await {
            Ok(s) => {
                s.close().await;
                return Ok(());
            }
            // A refused PIN counts towards the display's lockout ("Blocked" after
            // about 5 wrong PINs): never retry that, only connection errors
            Err(e @ mdc::MdcError::AuthFailed(_)) => {
                last = Some(e);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    // Not with the new one: say whether the old one still works
    let old = Device::new(&device.host, &device.pin)
        .connect(DEFAULT_CONNECT_TIMEOUT)
        .await;
    let why = last.map(|e| e.to_string()).unwrap_or_default();
    match old {
        Ok(s) => {
            s.close().await;
            bail!("the display didn't take the new PIN ({why}); its PIN is still the old one")
        }
        Err(_) => bail!(
            "couldn't log in with the new PIN ({why}) nor the old one: try both later \
             (`emdx displays` keeps the old one for now)"
        ),
    }
}

pub(crate) async fn empty_storage(device: &Target, kind: StorageKind, yes: bool) -> Result<()> {
    let what = match kind {
        StorageKind::Files => "all images and content stored on",
        StorageKind::NetworkCertificates => "the network certificates on",
        StorageKind::AppCertificates => "the app certificates on",
    };
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("pass --yes to erase {what} the display");
        }
        if !confirm(
            &mut Terminal,
            &format!("Erase {what} {}?", label(device)),
            false,
        )? {
            println!("Nothing erased.");
            return Ok(());
        }
    }
    let mut s = connect(device).await?;
    s.empty_storage(kind).await?;
    println!("✅ Erased");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_time_values() {
        assert_eq!(parse_sleep_time("off").unwrap(), SleepTime::Off);
        assert_eq!(parse_sleep_time("20").unwrap(), SleepTime::Minutes(20));
        assert_eq!(parse_sleep_time("60min").unwrap(), SleepTime::Minutes(60));
        // Custom times, in minutes, from 5 min to 23:59
        assert_eq!(parse_sleep_time("15").unwrap(), SleepTime::Custom(15));
        assert_eq!(parse_sleep_time("1:30").unwrap(), SleepTime::Custom(90));
        assert_eq!(parse_sleep_time("2h").unwrap(), SleepTime::Custom(120));
        assert_eq!(parse_sleep_time("23h59").unwrap(), SleepTime::Custom(1439));
        assert!(parse_sleep_time("4").is_err());
        assert!(parse_sleep_time("24:00").is_err());
        assert!(parse_sleep_time("1:75").is_err());
        assert!(parse_sleep_time("soon").is_err());
    }

    #[test]
    fn refresh_values() {
        assert_eq!(parse_refresh("23:00").unwrap(), Some((23, 0)));
        assert_eq!(parse_refresh("now").unwrap(), None);
        assert!(parse_refresh("24:00").is_err());
        assert!(parse_refresh("7h").is_err());
    }
}

#[cfg(test)]
mod sleep_mode_tests {
    use super::*;

    #[test]
    fn derived_like_the_app() {
        assert_eq!(SleepMode::of(SleepTime::Off, true), SleepMode::Off);
        assert_eq!(SleepMode::of(SleepTime::Off, false), SleepMode::Off);
        assert_eq!(
            SleepMode::of(SleepTime::Minutes(20), true),
            SleepMode::Sleep
        );
        assert_eq!(
            SleepMode::of(SleepTime::Minutes(5), false),
            SleepMode::DeepSleep
        );
    }

    #[test]
    fn keys_round_trip() {
        for m in [SleepMode::Sleep, SleepMode::DeepSleep, SleepMode::Off] {
            assert_eq!(SleepMode::parse(m.key()).unwrap(), m);
        }
        assert!(SleepMode::parse("hibernate").is_err());
    }
}
