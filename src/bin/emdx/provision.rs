//! `emdx setup`: first-time setup of a display over Bluetooth, then MDC.

use std::{io::IsTerminal, time::Duration};

use anyhow::{bail, Context, Result};
use clap::Args;

use emdx_controller::{
    ble::{
        bluez::{self, BluezLink, Found},
        proto,
        session::{BleSession, NetworkPlan},
    },
    discovery, mdc,
    provision::{self, Plan, Timing},
};

use crate::{
    config::{self, DisplayEntry, Target},
    setup::{ask_name, ask_required, confirm, Console, Terminal},
};

#[derive(Args, Debug)]
pub(crate) struct ProvisionArgs {
    /// Name to save the display under (prompted, or derived from its own name)
    pub name: Option<String>,
    /// Bluetooth address of the display (default: scan)
    #[arg(long)]
    pub address: Option<String>,
    /// The pairing PIN: the 6 digits under the QR code (or, for a display set up before, its display PIN)
    #[arg(long)]
    pub pairing_pin: Option<String>,
    /// Wi-Fi network name (2.4 GHz); the address is assigned automatically (DHCP)
    #[arg(long)]
    pub wifi: Option<String>,
    /// Wi-Fi password (omit for an open network)
    #[arg(long, requires = "wifi")]
    pub wifi_password: Option<String>,
    /// Accept the display's terms & conditions (read them first; the URL is printed)
    #[arg(long, conflicts_with = "decline_terms")]
    pub accept_terms: bool,
    /// Decline the terms & conditions without asking (images work without them)
    #[arg(long)]
    pub decline_terms: bool,
    /// Display PIN to use from now on: 6 digits, required (asked twice, hidden, when
    /// omitted in a terminal). The pairing PIN is only for the setup.
    #[arg(long)]
    pub display_pin: Option<String>,
    /// Name set on the display itself (default: the name it is saved under in emdx)
    #[arg(long)]
    pub device_name: Option<String>,
    /// How the panel is mounted (default: asked, or the display's own)
    #[arg(long, value_enum)]
    pub orientation: Option<crate::OrientationArg>,
    /// Sleep after new content: off, minutes (45) or 1:30, from 5 minutes to 23:59 (default: asked, or the display's own)
    #[arg(long)]
    pub sleep_time: Option<String>,
    /// Stay reachable while asleep: on or off (default: asked, or the display's own)
    #[arg(long)]
    pub network_standby: Option<String>,
    /// Refresh the screen every day at HH:MM (default: asked, or the display's own)
    #[arg(long)]
    pub daily_refresh: Option<String>,
    /// Only list displays seen over Bluetooth
    #[arg(long)]
    pub list: bool,
    /// Developer check: connect, authorize, read status/address/Wi-Fi name; change nothing
    #[arg(long, hide = true, conflicts_with_all = ["list", "wifi", "accept_terms", "decline_terms", "display_pin", "device_name", "orientation", "sleep_time", "network_standby", "daily_refresh"])]
    pub probe: bool,
    /// Bluetooth scan duration in seconds
    #[arg(long, default_value_t = 8)]
    pub scan_time: u64,
    /// Don't save the display to the emdx settings
    #[arg(long)]
    pub no_save: bool,
    /// Don't show the identification image (name and details) after setup
    #[arg(long)]
    pub no_placeholder: bool,
}

fn describe(f: &Found) -> String {
    let name = f.name.as_deref().unwrap_or("(no name)");
    let state = match f.advert {
        Some(a) if a.configured => "setup mode, set up before",
        Some(_) => "setup mode, new",
        None => "not in setup mode",
    };
    let rssi = f.rssi.map(|r| format!(", {r} dBm")).unwrap_or_default();
    format!("{name} — {} ({state}{rssi})", f.address)
}

async fn scan(seconds: u64) -> Result<Vec<Found>> {
    println!("🔍 Scanning Bluetooth for EMDX displays ({seconds}s)...");
    bluez::scan(Duration::from_secs(seconds)).await
}

/// Answers to the settings questions; `None` keeps the display's own value.
struct SetupChoices {
    settings: provision::SetupSettings,
    new_pin: Option<String>,
}

/// Ask `question` until `parse` accepts the answer; Enter (empty) gives `None`.
/// `on_enter` says what Enter does, shown in brackets: `Question [unchanged]: `.
fn ask_optional<T>(
    con: &mut impl Console,
    question: &str,
    on_enter: &str,
    parse: impl Fn(&str) -> Result<T>,
) -> Result<Option<T>> {
    loop {
        let answer = con.ask(&format!("{question} [{on_enter}]: "))?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(None);
        }
        match parse(answer) {
            Ok(v) => return Ok(Some(v)),
            Err(e) => con.say(&format!("{e:#}")),
        }
    }
}

/// `true` for portrait
fn parse_orientation(v: &str) -> Result<bool> {
    use clap::ValueEnum;
    let o = crate::OrientationArg::from_str(v, true)
        .map_err(|_| anyhow::anyhow!("use landscape or portrait"))?;
    Ok(o.is_portrait())
}

fn parse_on_off(v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "y" | "yes" => Ok(true),
        "off" | "n" | "no" => Ok(false),
        _ => bail!("answer on or off"),
    }
}

fn parse_hh_mm(v: &str) -> Result<(u8, u8)> {
    crate::settings::parse_refresh(v)?.context("give a time as HH:MM")
}

fn parse_new_pin(v: &str) -> Result<String> {
    if !valid_pin(v) {
        bail!("the PIN is 6 digits");
    }
    Ok(v.to_string())
}

/// The settings the ePaper app asks at the end of its setup: from flags, else
/// asked (Enter keeps the display's own value), else left alone.
fn setup_choices(
    args: &ProvisionArgs,
    interactive: bool,
    con: &mut impl Console,
) -> Result<SetupChoices> {
    let from_flag = |v: Option<&str>, parse: &dyn Fn(&str) -> Result<_>| v.map(parse).transpose();
    let mut orientation = args.orientation.map(|o| o.is_portrait());
    let mut sleep_time = args
        .sleep_time
        .as_deref()
        .map(crate::settings::parse_sleep_time)
        .transpose()?;
    let mut standby = from_flag(args.network_standby.as_deref(), &parse_on_off)?;
    let mut refresh = args.daily_refresh.as_deref().map(parse_hh_mm).transpose()?;
    let mut new_pin = args.display_pin.as_deref().map(parse_new_pin).transpose()?;
    if interactive {
        con.say("Display settings (press Enter to leave a setting unchanged):");
        if orientation.is_none() {
            orientation = ask_optional(
                con,
                "  Orientation (landscape or portrait)",
                "unchanged",
                parse_orientation,
            )?;
        }
        if sleep_time.is_none() {
            sleep_time = ask_optional(
                con,
                "  Sleep after new content (off, minutes or H:MM, 5 minutes to 23:59)",
                "unchanged",
                crate::settings::parse_sleep_time,
            )?;
        }
        if standby.is_none() {
            standby = ask_optional(
                con,
                "  Network standby, to stay reachable while asleep (on/off)",
                "unchanged",
                parse_on_off,
            )?;
        }
        if refresh.is_none() {
            refresh = ask_optional(
                con,
                "  Screen protection time, when the screen is refreshed (HH:MM)",
                "unchanged",
                parse_hh_mm,
            )?;
        }
        if new_pin.is_none() {
            // Required, and typed twice since it can't be seen
            new_pin = Some(loop {
                let first =
                    con.ask_secret("  Display PIN to use from now on (6 digits, hidden): ")?;
                if let Err(e) = parse_new_pin(first.trim()) {
                    con.say(&format!("{e:#}"));
                    continue;
                }
                let again = con.ask_secret("  Enter the display PIN again: ")?;
                if again.trim() != first.trim() {
                    con.say("The PINs you entered do not match. Please try again.");
                    continue;
                }
                con.say("  Remember this PIN. If you forget it, you'll need to reset your device.");
                break first.trim().to_string();
            });
        }
    }
    if new_pin.is_none() {
        bail!("pass --display-pin (6 digits): the PIN the display asks from now on");
    }
    Ok(SetupChoices {
        settings: provision::SetupSettings {
            portrait: orientation,
            sleep_time,
            network_standby: standby,
            daily_refresh: refresh,
        },
        new_pin,
    })
}

fn valid_pin(pin: &str) -> bool {
    pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit())
}

pub(crate) async fn run(args: ProvisionArgs) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();
    let mut con = Terminal;

    if args.list {
        let found = scan(args.scan_time).await?;
        if found.is_empty() {
            println!("No EMDX displays found over Bluetooth.");
        }
        for f in &found {
            println!("  {}", describe(f));
        }
        if found.iter().any(|f| !f.in_setup_mode()) {
            println!(
                "\nDisplays not in setup mode can be switched with `emdx pairing-mode -d <name>`."
            );
        }
        return Ok(());
    }

    // ── Which display ───────────────────────────────────────────────────────
    let mut display_name = None;
    // A display set up before shows no PIN: its pairing PIN is its display PIN
    let mut set_up_before = false;
    let address = match args.address.clone() {
        Some(a) => a,
        None => {
            let candidates: Vec<Found> = scan(args.scan_time)
                .await?
                .into_iter()
                .filter(Found::in_setup_mode)
                .collect();
            let chosen = match candidates.len() {
                0 => bail!(
                    "no display in Bluetooth pairing mode was found.\n   {}",
                    crate::settings::PAIRING_MODE_HELP
                ),
                1 => candidates[0].clone(),
                n if interactive => {
                    println!("Found {n} displays in setup mode:");
                    for (i, f) in candidates.iter().enumerate() {
                        println!("  {}. {}", i + 1, describe(f));
                    }
                    let answer = con.ask(&format!("Select the one to set up [1-{n}]: "))?;
                    let i = config::parse_choice(&answer, n).context("invalid choice")?;
                    candidates[i].clone()
                }
                _ => bail!("several displays are in setup mode; pass --address"),
            };
            println!("Selected {}", describe(&chosen));
            set_up_before = chosen.advert.is_some_and(|a| a.configured);
            display_name = display_name.or(chosen.name.clone());
            chosen.address
        }
    };

    // ── PIN ─────────────────────────────────────────────────────────────────
    let pin = match args.pairing_pin.clone() {
        Some(p) => p,
        None if interactive => loop {
            let question = if set_up_before {
                "Pairing PIN (this display was set up before and shows no PIN: use its display PIN)"
            } else {
                "Pairing PIN (the 6 digits under the QR code on the display)"
            };
            let p = ask_required(&mut con, question, None)?;
            if valid_pin(&p) {
                break p;
            }
            con.say("The PIN is 6 digits.");
        },
        None => bail!(
            "pass --pairing-pin (the pairing PIN: the 6 digits under the QR code on the display)"
        ),
    };
    if !valid_pin(&pin) {
        bail!("the PIN must be 6 digits");
    }
    if let Some(p) = &args.display_pin {
        if !valid_pin(p) {
            bail!("--display-pin must be 6 digits");
        }
    }

    if args.probe {
        return probe(&address, &pin).await;
    }

    // ── Network ─────────────────────────────────────────────────────────────
    let network = if let Some(ssid) = &args.wifi {
        let password = match &args.wifi_password {
            Some(p) => p.clone(),
            None if interactive => {
                con.ask_secret("Wi-Fi password, hidden while you type [none: open network]: ")?
            }
            None => String::new(),
        };
        NetworkPlan::Wifi {
            ssid: ssid.clone(),
            password,
        }
    } else if interactive {
        let ssid = ask_required(&mut con, "Wi-Fi network name (2.4 GHz)", None)?;
        let password =
            con.ask_secret("Wi-Fi password, hidden while you type [none: open network]: ")?;
        NetworkPlan::Wifi { ssid, password }
    } else {
        bail!("pass the Wi-Fi network: --wifi <name> [--wifi-password …]");
    };
    let NetworkPlan::Wifi { ssid, password } = &network;
    proto::wifi_data(ssid, password)?; // validate lengths before connecting

    // ── Name to save under ──────────────────────────────────────────────────
    let path = config::default_path()?;
    let mut saved = config::load(&path)?;
    let save_as = if args.no_save {
        None
    } else {
        let suggestion = config::unique_name(
            &config::suggest_name(display_name.as_deref().unwrap_or(config::LEGACY_NAME)),
            &saved,
        );
        Some(match args.name.clone() {
            Some(n) => {
                config::validate_name(&n)?;
                n
            }
            None if interactive => ask_name(&mut con, &saved, Some(&suggestion), true)?,
            None => suggestion,
        })
    };

    // ── Settings (the app's last setup pages) ───────────────────────────────
    let choices = setup_choices(&args, interactive, &mut con)?;

    let plan = Plan {
        pin,
        network,
        // One name: in emdx and on the display
        device_name: args.device_name.clone().or_else(|| save_as.clone()),
        new_pin: choices.new_pin.clone(),
        settings: choices.settings,
    };
    let timing = Timing::default();
    let mut progress = |m: &str| println!("{m}");

    // ── Bluetooth phase ─────────────────────────────────────────────────────
    println!("🔗 Connecting to {address} over Bluetooth...");
    let link = BluezLink::connect(&address, plan.pin.parse().ok(), Duration::from_secs(30)).await?;
    let mut ble = BleSession::new(link);
    let phase = provision::network_phase(&mut ble, &plan, timing, &mut progress).await;
    ble.into_link().close().await;
    let net = phase?;
    println!("🌐 The display is on the network at {}", net.ip);

    // ── MDC phase ───────────────────────────────────────────────────────────
    let (accept_flag, decline_flag) = (args.accept_terms, args.decline_terms);
    let mut accept = |url: &str| -> Result<bool> {
        println!("📄 Terms & conditions: {url}");
        if accept_flag {
            return Ok(true);
        }
        if decline_flag {
            println!("   Declined (--decline-terms).");
            return Ok(false);
        }
        if interactive {
            return confirm(
                &mut Terminal,
                "Do you accept the display's terms & conditions?",
                false,
            );
        }
        println!("   Not accepted (pass --accept-terms to accept them).");
        Ok(false)
    };
    let host = net.ip.to_string();
    let done = provision::finish_phase(
        &host,
        mdc::DEFAULT_PORT,
        &plan,
        timing,
        &mut progress,
        &mut accept,
    )
    .await?;

    // ── Save ────────────────────────────────────────────────────────────────
    let mac = match done.wifi_mac.clone() {
        Some(m) => Some(m),
        None => discovery::lookup_mac(&host).await,
    };
    if let Some(name) = &save_as {
        let first = saved.displays.is_empty();
        saved.upsert(
            name,
            DisplayEntry {
                host: host.clone(),
                pin: done.pin.clone(),
                mac: mac.clone(),
            },
            first,
        )?;
        config::save(&path, &saved)?;
        println!("💾 Saved as \"{name}\" — try `emdx -d {name} status`");
    }
    println!("🎉 Setup complete: {host}");
    if done.terms_accepted == Some(false) {
        println!(
            "ℹ️  The terms & conditions were not accepted; Samsung's online services stay off."
        );
    }

    // ── Identification image ────────────────────────────────────────────────
    if !args.no_placeholder {
        let target = Target {
            name: save_as.clone(),
            host: host.clone(),
            pin: done.pin.clone(),
            mac,
        };
        let NetworkPlan::Wifi { ssid, .. } = &plan.network;
        let ssid = Some(ssid.as_str());
        let title = save_as
            .or_else(|| display_name.clone())
            .unwrap_or_else(|| host.clone());
        println!("🖼  Showing the identification image...");
        let shown = async {
            let jpeg = crate::identification_image(&target, &title, ssid).await?;
            crate::push_image(&target, jpeg, "jpg", None).await
        }
        .await;
        if let Err(e) = shown {
            println!("⚠️  Could not show the identification image ({e:#}). Setup itself is done.");
        }
    }
    Ok(())
}

/// Connect, authorize and read — no setting is changed. The bond is removed on exit.
async fn probe(address: &str, pin: &str) -> Result<()> {
    println!("🔗 Connecting to {address} over Bluetooth...");
    let link = BluezLink::connect(address, pin.parse().ok(), Duration::from_secs(30)).await?;
    println!("   MTU {}", emdx_controller::ble::session::Link::mtu(&link));
    let mut ble = BleSession::new(link);
    let result: Result<()> = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let auth = ble
            .authorize(pin)
            .await
            .context("Bluetooth authorization")?;
        println!("🔐 Authorization: {auth}");
        if !auth.is_ok() {
            bail!("authorization failed: {auth}");
        }
        let show = |label: &str, v: Result<String>| match v {
            Ok(v) => println!("   {label:<14} {v}"),
            Err(e) => println!("   {label:<14} error: {e:#}"),
        };
        show(
            "Status:",
            ble.device_status().await.map(|s| format!("{s:?}")),
        );
        show(
            "Address:",
            ble.network_address().await.map(|a| {
                format!(
                    "{}/{} gw {} dns {} (DHCP {})",
                    a.ip, a.prefix, a.gateway, a.dns, a.dhcp
                )
            }),
        );
        show("Wi-Fi name:", ble.wifi_ssid().await);
        show("PoE+:", ble.poe_available().await.map(|v| v.to_string()));
        show(
            "Enterprise:",
            ble.eap_supported().await.map(|v| v.to_string()),
        );
        Ok(())
    }
    .await;
    println!("🔌 Disconnecting (and removing the pairing)...");
    ble.into_link().close().await;
    result
}
