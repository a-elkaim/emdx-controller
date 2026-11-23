mod config;
mod fanout;
#[cfg(feature = "ble")]
mod provision;
mod saved_slideshow;
mod serve;
mod settings;
mod setup;
mod thumbnail;
mod wifi;

use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use emdx_controller::{
    cms, content, discovery, local_ip, mdc,
    mdc::{Device, DEFAULT_CONNECT_TIMEOUT},
    placeholder, schedule, slideshow, wol,
};

use config::{CliConfig, Target};

#[derive(Parser)]
#[command(
    name = "emdx",
    version,
    about = "Control Samsung EMDX e-paper displays from the command line"
)]
struct Cli {
    /// Saved display to use (default: the default display, see `emdx displays`)
    #[arg(short, long, global = true, env = "EMDX_DISPLAY")]
    display: Option<String>,
    /// More diagnostic detail: -v info, -vv debug, -vvv trace (or set EMDX_LOG,
    /// e.g. EMDX_LOG=debug or EMDX_LOG=emdx_controller::mdc=trace)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Hide progress lines and all but error messages
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Also append everything (progress + diagnostics) to this file, for bug reports
    #[arg(long, global = true, env = "EMDX_LOG_FILE", value_name = "PATH")]
    log_file: Option<std::path::PathBuf>,
    /// Send images over plain HTTP (port 6868) instead of HTTPS with a certificate
    /// handed to the display (port 6869), like the app with "Secure Mobile CMS" off
    #[arg(long, global = true, env = "EMDX_CONTENT_HTTP")]
    content_http: bool,
    #[command(subcommand)]
    command: Command,
}

/// Connection flags; anything omitted comes from the saved display.
#[derive(Args, Debug)]
struct DeviceArgs {
    /// Display IP address (overrides the saved display)
    #[arg(long)]
    host: Option<String>,
    /// Display PIN, e.g. 000000 (overrides the saved display)
    #[arg(long)]
    display_pin: Option<String>,
    /// MAC address for Wake-on-LAN (overrides the saved display)
    #[arg(long)]
    mac: Option<String>,
}

impl DeviceArgs {
    fn target(self, display: Option<&str>, saved: &CliConfig) -> Result<Target> {
        config::resolve(display, self.host, self.display_pin, self.mac, saved)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Add a display that is already on the network (scan, or enter IP and PIN)
    Add {
        /// Name for the display (manual mode), e.g. "lobby"
        name: Option<String>,
        /// Scan the network without asking first
        #[arg(long, conflicts_with = "manual")]
        scan: bool,
        /// Enter the name, IP, MAC and display PIN by hand (implied by --host/--display-pin/--mac)
        #[arg(long)]
        manual: bool,
        /// Display IP address (manual mode)
        #[arg(long)]
        host: Option<String>,
        /// Display PIN (manual mode): the one chosen during its setup
        #[arg(long)]
        display_pin: Option<String>,
        /// MAC address for Wake-on-LAN (manual mode)
        #[arg(long)]
        mac: Option<String>,
        /// Make this the default display
        #[arg(long)]
        default: bool,
        /// Save without first checking that the display accepts the PIN
        #[arg(long)]
        no_verify: bool,
        /// Print the saved displays and exit (same as `emdx displays`)
        #[arg(long)]
        show: bool,
    },
    /// List, rename, remove saved displays or change the default
    Displays {
        #[command(subcommand)]
        action: Option<DisplaysAction>,
    },
    /// Show an image on a display
    ShowImage {
        #[command(flatten)]
        device: DeviceArgs,
        /// Path to the image file
        #[arg(long)]
        image: PathBuf,
        /// Local IP the display should download from (default: auto)
        #[arg(long)]
        local_ip: Option<String>,
        /// Minutes of inactivity before powering off (0 = never)
        #[arg(long, default_value_t = 0.0)]
        sleep_after: f64,
    },
    /// Show images in turn, changing every `--every` (a slideshow the display
    /// runs on its own, like the ePaper app's playlists)
    Slideshow {
        #[command(flatten)]
        device: DeviceArgs,
        /// An image, in the order shown; repeat for each slide
        #[arg(long = "image", required = true, num_args = 1)]
        images: Vec<PathBuf>,
        /// Time each image stays on screen: minutes (30), hours (2h) or 1:30, from
        /// 5 minutes to 24 hours
        #[arg(long)]
        every: String,
        /// Name shown for it on the display's side
        #[arg(long, default_value = "emdx slideshow")]
        name: String,
        /// Local IP the display should download from (default: auto)
        #[arg(long)]
        local_ip: Option<String>,
    },
    /// Show a different image at set times of day, every day (a schedule the display
    /// runs on its own, like the ePaper app's schedules)
    ///
    /// Each `--at` is a start time and the image shown from then until the next one.
    Schedule {
        #[command(flatten)]
        device: DeviceArgs,
        /// A start time (HH:MM) and the image shown from then on; repeat per entry
        #[arg(long = "at", required = true, num_args = 2, value_names = ["TIME", "IMAGE"])]
        at: Vec<String>,
        /// Name shown for it on the display's side
        #[arg(long, default_value = "emdx schedule")]
        name: String,
        /// Local IP the display should download from (default: auto)
        #[arg(long)]
        local_ip: Option<String>,
    },
    /// Ask a display to upload a thumbnail of what it is showing
    ///
    /// The ePaper app never pulls a thumbnail: it sends the display a callback URL (MDC
    /// 0xC7 sub 0x54) and the display POSTs a multipart upload back to it. This command
    /// runs that receiver, sends the request, and saves the image.
    Thumbnail(thumbnail::ThumbnailArgs),
    /// Set up a new or factory-reset display over Bluetooth (network, settings, PIN)
    ///
    /// The display must be in pairing mode: press the Bluetooth button on the back of the
    /// screen, the LED blinks blue. If nothing happens, the display is asleep: press its
    /// power button first (the LED turns green while it wakes up), then the Bluetooth
    /// button; the LED should blink blue.
    ///
    /// The pairing PIN is shown under the QR code on a new or factory-reset display. A
    /// display that was set up before shows no PIN: its pairing PIN is its display PIN.
    #[cfg(feature = "ble")]
    Setup(provision::ProvisionArgs),
    /// Change the display PIN on the display itself, then save it
    ChangePin {
        #[command(flatten)]
        device: DeviceArgs,
        /// The new PIN (6 digits); asked twice, hidden, when omitted
        #[arg(long)]
        new_pin: Option<String>,
    },
    /// Put a configured display in Bluetooth pairing mode (to set it up again)
    #[command(alias = "enable-ble")]
    PairingMode {
        #[command(flatten)]
        device: DeviceArgs,
        /// Value sent with the request (the ePaper app's default; meaning unconfirmed)
        #[arg(long, default_value_t = 2000)]
        value: u16,
    },
    /// Forget a display: back to pairing mode (or erase its content), then remove it from emdx
    Forget {
        #[command(flatten)]
        device: DeviceArgs,
        /// Don't ask for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Reset the display to default settings and delete its content (`defaults` or
    /// `all`, the app's "Reset All"), or only its settings (`settings`)
    Reset {
        #[command(flatten)]
        device: DeviceArgs,
        /// `defaults`/`all`: settings back to default and stored content deleted; `settings`: settings only
        #[arg(value_enum)]
        what: ResetArg,
        /// Don't ask for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Search the local network for EMDX displays; saved displays found at a new
    /// address (matched by MAC) get their address updated
    Discover {
        /// Search duration in seconds
        #[arg(long, default_value_t = 4)]
        timeout: u64,
    },
    /// Wake the display via Wake-on-LAN
    Wake {
        /// MAC address (overrides the saved display)
        #[arg(long)]
        mac: Option<String>,
        /// Wake every saved display
        #[arg(long, conflicts_with = "mac")]
        all: bool,
    },
    /// Deep sleep (network standby disabled): only the power button wakes it again
    Sleep {
        #[command(flatten)]
        device: DeviceArgs,
        /// Power off every saved display
        #[arg(long, conflicts_with_all = ["host", "display_pin", "mac"])]
        all: bool,
    },
    /// Show or set what the display plays (images from emdx need `mobile`)
    ContentPlayer {
        #[command(flatten)]
        device: DeviceArgs,
        /// New source; omit to show the current one
        #[arg(value_enum)]
        source: Option<PlayerSource>,
    },
    /// Show or set how long the display stays awake after new content
    SleepTime {
        #[command(flatten)]
        device: DeviceArgs,
        /// `off`, minutes (45) or hours and minutes (1:30), from 5 minutes to 23:59;
        /// omit to show the current one
        value: Option<String>,
    },
    /// Show or set whether the display sleeps as soon as new content is shown
    SleepAfterUpdate {
        #[command(flatten)]
        device: DeviceArgs,
        /// `on` or `off`; omit to show the current state
        value: Option<String>,
    },
    /// Show or set the screen protection time (HH:MM), or refresh now (`now`): the
    /// screen is refreshed at that time to prevent ghost images (every 3 days according
    /// to the ePaper app)
    DailyRefresh {
        #[command(flatten)]
        device: DeviceArgs,
        /// `HH:MM` or `now`; omit to show the next scheduled refresh
        value: Option<String>,
    },
    /// Show or set the sleep mode, as in the ePaper app: `sleep` (Wake-on-LAN wakes it),
    /// `deep-sleep` (only its power button wakes it) or `off` (never sleeps)
    SleepMode {
        #[command(flatten)]
        device: DeviceArgs,
        /// `sleep`, `deep-sleep` or `off`; omit to show the current one
        value: Option<String>,
    },
    /// Show or set network standby (whether the display stays reachable asleep)
    NetworkStandby {
        #[command(flatten)]
        device: DeviceArgs,
        /// `on` or `off`; omit to show the current state
        value: Option<String>,
    },
    /// Erase content (or certificates) stored on the display
    EmptyStorage {
        #[command(flatten)]
        device: DeviceArgs,
        /// What to erase
        #[arg(long, value_enum, default_value = "files")]
        what: StorageArg,
        /// Don't ask for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Show the display's name and details on it, to tell displays apart
    Identify {
        #[command(flatten)]
        device: DeviceArgs,
        /// Save the image to this file instead of sending it
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Show or set the screen orientation
    Orientation {
        #[command(flatten)]
        device: DeviceArgs,
        /// New orientation; omit to show the current one
        #[arg(value_enum)]
        orientation: Option<OrientationArg>,
    },
    /// Show or set the background colour (the six the ePaper app offers)
    BackgroundColor {
        #[command(flatten)]
        device: DeviceArgs,
        /// New colour; omit to show the current one
        #[arg(value_enum)]
        colour: Option<BackgroundColorArg>,
    },
    /// Run the web UI (same displays and features as the CLI)
    Serve(serve::ServeArgs),
    /// Get display status (power, battery, info)
    Status {
        #[command(flatten)]
        device: DeviceArgs,
        /// Show the status of every saved display
        #[arg(long, conflicts_with_all = ["host", "display_pin", "mac"])]
        all: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum PlayerSource {
    /// Images sent by emdx or the ePaper app (MobileCMS)
    Mobile,
    Usb,
    Vxt,
    CustomApp,
}

impl PlayerSource {
    fn value(self) -> u8 {
        use emdx_controller::mdc::content_player as cp;
        match self {
            Self::Mobile => cp::MOBILE_CMS,
            Self::Usb => cp::USB,
            Self::Vxt => cp::VXT,
            Self::CustomApp => cp::CUSTOM_APP,
        }
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ResetArg {
    /// Only the app's "Reset Settings" (on an EM32DX: just the sleep time); content is kept
    Settings,
    /// Settings back to default and the stored content deleted (the app's "Reset All";
    /// also `defaults`)
    #[value(alias = "defaults")]
    All,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum StorageArg {
    /// Downloaded images and content
    Files,
    NetworkCerts,
    AppCerts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum OrientationArg {
    Landscape,
    Portrait,
}

/// The background colours the ePaper app can set (`0x21/0x90`); the display has no
/// other colours, so emdx sends no other values.
#[derive(Clone, Copy, clap::ValueEnum)]
enum BackgroundColorArg {
    White,
    /// The app's near-black (`#121212` swatch)
    Black,
    Red,
    Green,
    Blue,
    /// The app's yellow (`#ffd700` swatch)
    Yellow,
}

impl BackgroundColorArg {
    fn value(self) -> u8 {
        use emdx_controller::mdc::bg_color as c;
        match self {
            Self::White => c::WHITE,
            Self::Black => c::BLACK,
            Self::Red => c::RED,
            Self::Green => c::GREEN,
            Self::Blue => c::BLUE,
            Self::Yellow => c::YELLOW,
        }
    }
}

impl OrientationArg {
    pub(crate) fn is_portrait(self) -> bool {
        self == Self::Portrait
    }
}

#[derive(Subcommand)]
enum DisplaysAction {
    /// List saved displays (the default is marked with *)
    List,
    /// Make a saved display the default
    Default { name: String },
    /// Forget a saved display
    Remove { name: String },
    /// Rename a saved display
    Rename { from: String, to: String },
    /// Change a saved display's address, saved PIN or MAC (locally; does not change
    /// anything on the display — see `change-pin` to change the PIN on the display)
    Edit {
        name: String,
        /// New IP address or hostname
        #[arg(long)]
        host: Option<String>,
        /// New saved PIN (6 digits; only the value emdx stores, not the display's)
        #[arg(long)]
        pin: Option<String>,
        /// New MAC address, or "" to clear it
        #[arg(long)]
        mac: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    // Set logging up first, so every later line (and any error) is captured.
    if let Err(e) = emdx_controller::log::init(emdx_controller::log::Options {
        verbose: cli.verbose,
        quiet: cli.quiet,
        file: cli.log_file.clone(),
        console_banner: matches!(cli.command, Command::Serve(_)),
    }) {
        eprintln!("❌ {e:#}");
        std::process::exit(1);
    }
    if let Err(e) = run(cli).await {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

/// Saved settings. An unreadable file is reported but doesn't block
/// commands that pass explicit flags.
fn saved_config() -> CliConfig {
    let Ok(path) = config::default_path() else {
        return CliConfig::default();
    };
    config::load(&path).unwrap_or_else(|e| {
        eprintln!("⚠️  Ignoring saved settings: {e:#}");
        CliConfig::default()
    })
}

pub(crate) fn label(t: &Target) -> String {
    match &t.name {
        Some(name) => format!("\"{name}\" ({})", t.host),
        None => t.host.clone(),
    }
}

async fn run(cli: Cli) -> Result<()> {
    emdx_controller::cms::set_plain_http(cli.content_http);
    let display = cli.display.as_deref();
    match cli.command {
        Command::Add {
            name,
            scan,
            manual,
            host,
            display_pin,
            mac,
            default,
            no_verify,
            show,
        } => {
            if show {
                return list_displays();
            }
            let opts = setup::Options {
                name: name.or(cli.display.clone()),
                host,
                display_pin,
                mac,
                mode: match (scan, manual) {
                    (true, _) => Some(setup::Mode::Scan),
                    (_, true) => Some(setup::Mode::Manual),
                    _ => None,
                },
                make_default: default,
                verify: !no_verify,
                interactive: std::io::stdin().is_terminal(),
            };
            run_setup(&opts).await.map(drop)
        }
        Command::Displays { action } => manage_displays(action.unwrap_or(DisplaysAction::List)),
        Command::Discover { timeout } => discover(timeout).await,
        Command::Serve(args) => serve::run(args).await,
        #[cfg(feature = "ble")]
        Command::Setup(args) => provision::run(args).await,
        command => {
            let saved = ensure_display(&command, display).await?;
            match command {
                Command::ShowImage {
                    device,
                    image,
                    local_ip,
                    sleep_after,
                } => {
                    show_image(
                        device.target(display, &saved)?,
                        &image,
                        local_ip,
                        sleep_after,
                    )
                    .await
                }
                Command::Slideshow {
                    device,
                    images,
                    every,
                    name,
                    local_ip,
                } => {
                    let device = device.target(display, &saved)?;
                    slideshow(&device, &images, &every, &name, local_ip).await
                }
                Command::Schedule {
                    device,
                    at,
                    name,
                    local_ip,
                } => {
                    let device = device.target(display, &saved)?;
                    schedule_cmd(&device, &at, &name, local_ip).await
                }
                Command::Thumbnail(args) => thumbnail::run(args, display, &saved).await,
                Command::Wake { all: true, .. } => {
                    on_all(display, &saved, "Waking", |t| async move { wake(&t).await }).await
                }
                Command::Wake { mac, all: false } => {
                    let mac = config::resolve_mac(display, mac, &saved)?;
                    emdx_controller::say!("🔄 Sending Wake-on-LAN to {mac}...");
                    wol::wake(&mac).await?;
                    emdx_controller::say!("✅ Magic packet sent");
                    Ok(())
                }
                Command::Sleep { all: true, .. } => {
                    on_all(display, &saved, "Powering off", |t| async move {
                        sleep(&t).await
                    })
                    .await
                }
                Command::Sleep { device, all: false } => {
                    let device = device.target(display, &saved)?;
                    emdx_controller::say!("🔄 Connecting to {}...", label(&device));
                    for line in sleep(&device).await? {
                        println!("{line}");
                    }
                    Ok(())
                }
                Command::Status { all: true, .. } => {
                    on_all(
                        display,
                        &saved,
                        "Checking",
                        |t| async move { status(&t).await },
                    )
                    .await
                }
                Command::Status { device, all: false } => {
                    let device = device.target(display, &saved)?;
                    emdx_controller::say!("🔄 Connecting to {}...", label(&device));
                    for line in status(&device).await? {
                        println!("{line}");
                    }
                    Ok(())
                }
                Command::SleepTime { device, value } => {
                    let device = device.target(display, &saved)?;
                    settings::sleep_time(&device, value.as_deref()).await
                }
                Command::DailyRefresh { device, value } => {
                    let device = device.target(display, &saved)?;
                    settings::daily_refresh(&device, value.as_deref()).await
                }
                Command::NetworkStandby { device, value } => {
                    let device = device.target(display, &saved)?;
                    settings::network_standby(&device, value.as_deref()).await
                }
                Command::SleepAfterUpdate { device, value } => {
                    let device = device.target(display, &saved)?;
                    settings::sleep_after_update(&device, value.as_deref()).await
                }
                Command::SleepMode { device, value } => {
                    let device = device.target(display, &saved)?;
                    settings::sleep_mode(&device, value.as_deref()).await
                }
                Command::EmptyStorage { device, what, yes } => {
                    let device = device.target(display, &saved)?;
                    let kind = match what {
                        StorageArg::Files => mdc::StorageKind::Files,
                        StorageArg::NetworkCerts => mdc::StorageKind::NetworkCertificates,
                        StorageArg::AppCerts => mdc::StorageKind::AppCertificates,
                    };
                    settings::empty_storage(&device, kind, yes).await
                }
                Command::Identify { device, output } => {
                    let device = device.target(display, &saved)?;
                    emdx_controller::say!("🔄 Connecting to {}...", label(&device));
                    let title = device.name.clone().unwrap_or_else(|| device.host.clone());
                    let jpeg = identification_image(&device, &title, None).await?;
                    match output {
                        Some(path) => {
                            std::fs::write(&path, &jpeg)
                                .with_context(|| format!("writing {}", path.display()))?;
                            println!("💾 Saved {}", path.display());
                            Ok(())
                        }
                        None => push_image(&device, jpeg, "jpg", None).await,
                    }
                }
                Command::Orientation {
                    device,
                    orientation,
                } => {
                    let device = device.target(display, &saved)?;
                    let mut s = settings::connect(&device).await?;
                    if let Some(o) = orientation {
                        s.set_portrait(o.is_portrait()).await?;
                    }
                    let portrait = s.portrait().await?;
                    s.close().await;
                    println!(
                        "Orientation: {}",
                        if portrait { "portrait" } else { "landscape" }
                    );
                    Ok(())
                }
                Command::BackgroundColor { device, colour } => {
                    let device = device.target(display, &saved)?;
                    let mut s = settings::connect(&device).await?;
                    if let Some(c) = colour {
                        s.set_background_color(c.value()).await?;
                    }
                    let colour = s.background_color().await;
                    s.close().await;
                    let colour = colour?;
                    println!(
                        "Background colour: {}",
                        mdc::bg_color::name(colour).unwrap_or(&format!("unknown ({colour})"))
                    );
                    Ok(())
                }
                Command::ContentPlayer { device, source } => {
                    let device = device.target(display, &saved)?;
                    content_player(&device, source).await
                }
                Command::PairingMode { device, value } => {
                    let device = device.target(display, &saved)?;
                    settings::pairing_mode(&device, value).await
                }
                Command::ChangePin { device, new_pin } => {
                    let device = device.target(display, &saved)?;
                    change_pin(&device, new_pin).await
                }
                Command::Forget { device, yes } => {
                    let device = device.target(display, &saved)?;
                    let name = device.name.clone();
                    settings::forget(&device, yes).await?;
                    // Drop its saved slideshow too — but only if forget actually
                    // removed it (it returns Ok and keeps it when the user declines).
                    if let (Some(name), Ok(path)) = (name, config::default_path()) {
                        let gone = config::load(&path)
                            .map(|c| !c.displays.contains_key(&name))
                            .unwrap_or(false);
                        if gone {
                            let _ = saved_slideshow::remove(&saved_slideshow::root(&path), &name);
                        }
                    }
                    Ok(())
                }
                Command::Reset { device, what, yes } => {
                    let device = device.target(display, &saved)?;
                    let kind = match what {
                        ResetArg::Settings => mdc::ResetKind::Settings,
                        ResetArg::All => mdc::ResetKind::All,
                    };
                    settings::reset(&device, kind, yes).await
                }
                #[cfg(feature = "ble")]
                Command::Setup(_) => unreachable!("handled above"),
                Command::Add { .. }
                | Command::Displays { .. }
                | Command::Discover { .. }
                | Command::Serve(_) => {
                    unreachable!("handled above")
                }
            }
        }
    }
}

fn list_displays() -> Result<()> {
    let path = config::default_path()?;
    let saved = config::load(&path)?;
    if saved.displays.is_empty() {
        println!(
            "No displays saved yet ({}). Add one with `emdx add`, or set up a new one with `emdx setup`.",
            path.display()
        );
        return Ok(());
    }
    let default = saved.default_name();
    let width = saved.names().iter().map(|n| n.len()).max().unwrap_or(0);
    let pin_width = saved
        .displays
        .values()
        .map(|d| d.pin.len())
        .max()
        .unwrap_or(0);
    println!("Saved displays ({}):", path.display());
    for (name, d) in &saved.displays {
        println!(
            "  {} {name:<width$}  {:<15}  PIN {:<pin_width$}  MAC {}",
            if Some(name.as_str()) == default {
                "*"
            } else {
                " "
            },
            d.host,
            config::mask_pin(&d.pin),
            d.mac.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

fn manage_displays(action: DisplaysAction) -> Result<()> {
    if let DisplaysAction::List = action {
        return list_displays();
    }
    let path = config::default_path()?;
    let mut saved = config::load(&path)?;
    match action {
        DisplaysAction::List => unreachable!("handled above"),
        DisplaysAction::Default { name } => {
            saved.set_default(&name)?;
            println!("⭐ \"{name}\" is now the default display");
        }
        DisplaysAction::Remove { name } => {
            saved.remove(&name)?;
            let _ = saved_slideshow::remove(&saved_slideshow::root(&path), &name);
            println!("🗑  Removed \"{name}\"");
            if let Some(d) = saved.default_name() {
                println!("   Default display: \"{d}\"");
            }
        }
        DisplaysAction::Rename { from, to } => {
            saved.rename(&from, &to)?;
            let _ = saved_slideshow::rename(&saved_slideshow::root(&path), &from, &to);
            println!("✏️  Renamed \"{from}\" → \"{to}\"");
        }
        DisplaysAction::Edit {
            name,
            host,
            pin,
            mac,
        } => {
            let entry = saved
                .displays
                .get_mut(&name)
                .with_context(|| format!("unknown display {name:?}"))?;
            let mut changed = Vec::new();
            if let Some(h) = host.map(|h| h.trim().to_string()).filter(|h| !h.is_empty()) {
                entry.host = h;
                changed.push("address");
            }
            if let Some(p) = pin.filter(|p| !p.is_empty()) {
                entry.pin = p;
                changed.push("saved PIN");
            }
            if let Some(m) = mac {
                entry.mac = Some(m.trim().to_string()).filter(|m| !m.is_empty());
                changed.push("MAC address");
            }
            if changed.is_empty() {
                bail!("nothing to change: pass --host, --pin and/or --mac");
            }
            println!("✏️  Updated the {} for \"{name}\"", changed.join(", "));
        }
    }
    config::save(&path, &saved)
}

/// Run the setup dialog against the real terminal and network, saving to
/// the settings file after every added display. Returns the updated settings.
async fn run_setup(opts: &setup::Options) -> Result<CliConfig> {
    let path = config::default_path()?;
    let mut saved = config::load(&path)?;
    let mut save = |c: &CliConfig| config::save(&path, c);
    let added = setup::run(
        &mut setup::Terminal,
        &mut setup::Network,
        &mut saved,
        &mut save,
        opts,
    )
    .await?;
    if !added.is_empty() {
        println!("Settings: {}", path.display());
    }
    Ok(saved)
}

/// Settings for a display command. With nothing saved and no connection
/// flags, start setup first (when there is a terminal to ask questions).
async fn ensure_display(command: &Command, display: Option<&str>) -> Result<CliConfig> {
    let saved = saved_config();
    let has_flags = match command {
        Command::ShowImage { device, .. }
        | Command::Slideshow { device, .. }
        | Command::Schedule { device, .. }
        | Command::Sleep { device, .. }
        | Command::ContentPlayer { device, .. }
        | Command::Orientation { device, .. }
        | Command::BackgroundColor { device, .. }
        | Command::Identify { device, .. }
        | Command::PairingMode { device, .. }
        | Command::ChangePin { device, .. }
        | Command::Reset { device, .. }
        | Command::Forget { device, .. }
        | Command::SleepTime { device, .. }
        | Command::DailyRefresh { device, .. }
        | Command::NetworkStandby { device, .. }
        | Command::SleepMode { device, .. }
        | Command::SleepAfterUpdate { device, .. }
        | Command::EmptyStorage { device, .. }
        | Command::Status { device, .. } => device.host.is_some(),
        Command::Thumbnail(args) => args.device.host.is_some(),
        Command::Wake { mac, .. } => mac.is_some(),
        _ => true,
    };
    if !saved.displays.is_empty()
        || has_flags
        || display.is_some()
        || !std::io::stdin().is_terminal()
    {
        return Ok(saved);
    }
    println!("No display is set up yet — let's add one.\n");
    let opts = setup::Options {
        verify: true,
        interactive: true,
        ..setup::Options::default()
    };
    let saved = run_setup(&opts).await?;
    if saved.displays.is_empty() {
        bail!("no display was added");
    }
    println!();
    Ok(saved)
}

/// Run `action` on every saved display (`--all`) and print a grouped report.
async fn on_all<F, Fut>(
    display: Option<&str>,
    saved: &CliConfig,
    verb: &str,
    action: F,
) -> Result<()>
where
    F: Fn(Target) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>>> + Send + 'static,
{
    if display.is_some() {
        bail!("--all can't be combined with --display");
    }
    let targets = saved.targets();
    if targets.is_empty() {
        bail!("no displays saved: run `emdx add`");
    }
    println!(
        "🔄 {verb} {} display{}...\n",
        targets.len(),
        if targets.len() == 1 { "" } else { "s" }
    );
    let outcomes = fanout::run_all(targets, action).await;
    let (text, failed) = fanout::report(&outcomes);
    println!("{text}");
    if failed > 0 {
        bail!("{failed} of {} displays failed", outcomes.len());
    }
    Ok(())
}

async fn wake(device: &Target) -> Result<Vec<String>> {
    let Some(mac) = device.mac.as_deref() else {
        let name = device.name.as_deref().unwrap_or("<name>");
        bail!("no MAC address saved (add one with `emdx add {name} --manual`)");
    };
    wol::wake(mac).await?;
    Ok(vec![format!("✅ Wake-on-LAN sent to {mac}")])
}

/// `emdx change-pin`: on the display, then in the saved settings
async fn change_pin(device: &Target, new_pin: Option<String>) -> Result<()> {
    let new_pin = match new_pin {
        Some(p) => p,
        None => {
            let first = rpassword::prompt_password("New display PIN (6 digits, hidden): ")
                .context("reading the PIN")?;
            settings::check_new_pin(first.trim())?;
            let again =
                rpassword::prompt_password("Enter the PIN again: ").context("reading the PIN")?;
            if first.trim() != again.trim() {
                bail!("the PINs you entered do not match");
            }
            first.trim().to_string()
        }
    };
    settings::change_pin_on_display(device, &new_pin).await?;
    match &device.name {
        Some(name) => {
            let path = config::default_path()?;
            let mut cfg = config::load(&path)?;
            if let Some(e) = cfg.displays.get_mut(name) {
                e.pin = new_pin.clone();
            }
            config::save(&path, &cfg)?;
            println!("✅ The display PIN is changed, and saved for \"{name}\".");
        }
        None => println!("✅ The display PIN is changed: use --display-pin {new_pin} from now on."),
    }
    println!("   Remember it: if you forget it, the display has to be reset.");
    Ok(())
}

async fn sleep(device: &Target) -> Result<Vec<String>> {
    Device::new(&device.host, &device.pin)
        .power_off(false)
        .await?;
    Ok(vec![
        "✅ In deep sleep: press its power button to wake it (the network can't)".into(),
    ])
}

async fn content_player(device: &Target, source: Option<PlayerSource>) -> Result<()> {
    use emdx_controller::mdc::content_player::name;
    println!("🔄 Connecting to {}...", label(device));
    let mut s = Device::new(&device.host, &device.pin)
        .connect(DEFAULT_CONNECT_TIMEOUT)
        .await?;
    if let Some(source) = source {
        s.set_content_player(source.value()).await?;
    }
    let current = s.content_player().await?;
    let shown = name(current).map_or_else(|| format!("unknown ({current})"), str::to_string);
    println!("Content player: {shown}");
    Ok(())
}

async fn discover(timeout: u64) -> Result<()> {
    println!("🔍 Searching for EMDX displays...");
    let displays =
        discovery::discover(Duration::from_secs(timeout), |msg| println!("   {msg}")).await?;
    if displays.is_empty() {
        println!("❌ No displays found — sleeping displays do not respond, wake them first");
        return Ok(());
    }
    println!(
        "\n✅ Found {} display{}:",
        displays.len(),
        if displays.len() == 1 { "" } else { "s" }
    );
    // Saved displays the router gave a new address to: follow them by MAC
    let path = config::default_path()?;
    let mut cfg = config::load(&path)?;
    let moved = cfg.relocate_by_mac(
        displays
            .iter()
            .filter_map(|d| Some((d.mac.as_deref()?, d.host.as_str()))),
    );
    if !moved.is_empty() {
        config::save(&path, &cfg)?;
        println!();
        for m in &moved {
            println!(
                "📍 \"{}\" is now at {} (was {}): saved address updated",
                m.name, m.to, m.from
            );
        }
    }
    for d in displays {
        println!("\n  {} ({})", d.name, d.model);
        println!("    Host:   {}", d.host);
        println!("    MAC:    {}", d.mac.as_deref().unwrap_or("unknown"));
        println!("    Serial: {}", d.serial.as_deref().unwrap_or("unknown"));
    }
    Ok(())
}

async fn status(device: &Target) -> Result<Vec<String>> {
    let mut s = Device::new(&device.host, &device.pin)
        .connect(DEFAULT_CONNECT_TIMEOUT)
        .await?;
    let mut lines = vec![format!(
        "Power:    {}",
        s.power_state().await.unwrap_or("Unknown")
    )];
    lines.push(match s.battery().await {
        Ok(b) => format!(
            "Battery:  {}%{}{}",
            b.level,
            if b.charging { " (charging)" } else { "" },
            if b.healthy { "" } else { " (unhealthy)" }
        ),
        Err(_) => "Battery:  N/A".into(),
    });
    if let Ok(name) = s.device_name().await {
        lines.push(format!("Name:     {name}"));
    }
    if let Ok(serial) = s.serial_number().await {
        lines.push(format!("Serial:   {serial}"));
    }
    if let Ok(version) = s.software_version().await {
        lines.push(format!("Software: {version}"));
    }
    if let Ok(mac) = s.wifi_mac().await {
        lines.push(format!("Wi-Fi MAC: {mac}"));
    }
    if let Some(c) = s.clock().await {
        lines.push(format!(
            "Clock:    {}-{:02}-{:02} {:02}:{:02}",
            c.year, c.month, c.day, c.hour, c.minute
        ));
    }
    if let Ok(done) = s.setup_done().await {
        lines.push(format!(
            "Setup:    {}",
            if done { "completed" } else { "not completed" }
        ));
    }
    if let Ok(t) = s.terms().await {
        let agreed = match t.agreed {
            Some(true) => "accepted",
            Some(false) => "not accepted",
            None => "unknown",
        };
        lines.push(format!("Terms:    {agreed}"));
    }
    if let Ok(v) = s.content_player().await {
        let name =
            mdc::content_player::name(v).map_or_else(|| format!("unknown ({v})"), str::to_string);
        lines.push(format!("Player:   {name}"));
    }
    if let Ok(portrait) = s.portrait().await {
        lines.push(format!(
            "Orientation: {}",
            if portrait { "portrait" } else { "landscape" }
        ));
    }
    if let Ok(colour) = s.background_color().await {
        lines.push(format!(
            "Background colour: {}",
            mdc::bg_color::name(colour).unwrap_or(&format!("unknown ({colour})"))
        ));
    }
    if let Ok(t) = s.sleep_time().await {
        lines.push(format!("Sleep time: {t}"));
    }
    if let Ok(r) = s.daily_refresh().await {
        lines.push(format!(
            "Screen protection time: {}",
            settings::describe_refresh(&r)
        ));
    }
    if let Some(on) = s.network_standby().await {
        lines.push(format!(
            "Network standby: {}",
            if on { "on" } else { "off" }
        ));
    }
    if let Ok(f) = s.features().await {
        lines.push(format!(
            "Features: low-power Wi-Fi {}, refresh {}",
            if f.low_power_wifi { "yes" } else { "no" },
            if f.refresh_by_refresh {
                "by refresh"
            } else {
                "on new content only"
            }
        ));
    }
    if let Ok(info) = s.contact_info().await {
        for (code, value) in info {
            // Already shown above
            if matches!(code, 0x8001 | 0x8002 | 0x8006) || value.is_empty() {
                continue;
            }
            let label = mdc::contact_info_label(code)
                .map_or_else(|| format!("0x{code:04X}"), str::to_string);
            lines.push(format!("{label}: {value}"));
        }
    }
    s.close().await;
    Ok(lines)
}

async fn show_image(
    device: Target,
    image: &Path,
    local_ip: Option<String>,
    sleep_after: f64,
) -> Result<()> {
    let bytes = std::fs::read(image)
        .with_context(|| format!("Image file not found: {}", image.display()))?;
    let ext = image
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg")
        .to_string();
    println!("Image: {} ({} bytes)", image.display(), bytes.len());
    println!("Display: {}", label(&device));
    if sleep_after > 0.0 {
        println!("Sleep after: {sleep_after} minutes");
    }
    push_image(&device, bytes, &ext, local_ip).await?;

    if sleep_after > 0.0 {
        println!("⏰ Display will sleep in {sleep_after} minutes...");
        tokio::time::sleep(Duration::from_secs_f64(sleep_after * 60.0)).await;
        println!("\n💤 Sleep timer fired — powering off display...");
        for line in sleep(&device).await? {
            println!("{line}");
        }
    }
    Ok(())
}

/// `emdx slideshow`: read and convert the images, then send them
async fn slideshow(
    device: &Target,
    images: &[PathBuf],
    every: &str,
    name: &str,
    local_ip: Option<String>,
) -> Result<()> {
    let interval = content::SlideInterval::parse(every).map_err(anyhow::Error::msg)?;
    let mut jpegs = Vec::new();
    for path in images {
        let jpeg = read_jpeg(path).await?;
        jpegs.push(jpeg);
    }
    println!(
        "Slideshow: {} images, each shown for {interval}, on {}",
        jpegs.len(),
        label(device)
    );
    push_slideshow(device, jpegs, interval, name, local_ip).await
}

/// `emdx schedule`: check the entries, read and convert the images, then send them
async fn schedule_cmd(
    device: &Target,
    at: &[String],
    name: &str,
    local_ip: Option<String>,
) -> Result<()> {
    // clap gives the values of every `--at` in one list: time, image, time, image…
    let mut planned: Vec<schedule::Entry<PathBuf>> = at
        .chunks_exact(2)
        .map(|pair| {
            Ok(schedule::Entry {
                start: content::StartTime::parse(&pair[0]).map_err(anyhow::Error::msg)?,
                image: PathBuf::from(&pair[1]),
            })
        })
        .collect::<Result<_>>()?;
    schedule::check(&planned).map_err(anyhow::Error::msg)?;
    planned.sort_by_key(|e| e.start);
    println!("Schedule on {}, every day:", label(device));
    for e in &planned {
        println!("  {}  {}", e.start, e.image.display());
    }
    let mut entries = Vec::new();
    for e in planned {
        entries.push(schedule::Entry {
            start: e.start,
            image: read_jpeg(&e.image).await?,
        });
    }
    push_schedule(device, entries, name, local_ip).await
}

/// Read an image file and convert it to the JPEG the display takes
async fn read_jpeg(path: &std::path::Path) -> Result<Vec<u8>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("Image file not found: {}", path.display()))?;
    tokio::task::spawn_blocking(move || {
        emdx_controller::imaging::normalize_jpeg(&bytes, emdx_controller::imaging::QUALITY_DEFAULT)
    })
    .await?
    .with_context(|| format!("reading the image {}", path.display()))
}

/// Wake the display if it can be, then send the schedule (see [`schedule::send`])
pub(crate) async fn push_schedule(
    device: &Target,
    entries: Vec<schedule::Entry>,
    name: &str,
    local_ip: Option<String>,
) -> Result<()> {
    let local_ip = local_ip.unwrap_or_else(|| local_ip::get_local_ip(Some(&device.host)));
    if local_ip.is_empty() {
        bail!("Could not determine a local IP address; pass --local-ip");
    }
    if let Some(mac) = &device.mac {
        wol::wake(mac).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    emdx_controller::say!("🔄 Sending the schedule to {}...", label(device));
    let switched = schedule::send(
        &Device::new(&device.host, &device.pin),
        &local_ip,
        entries,
        name,
    )
    .await?;
    if switched {
        emdx_controller::say!(
            "🖼  Switched the content player to Mobile (needed to show images from emdx)"
        );
    }
    Ok(())
}

/// Wake the display if it can be, then send the slideshow (see [`slideshow::send`])
pub(crate) async fn push_slideshow(
    device: &Target,
    jpegs: Vec<Vec<u8>>,
    interval: content::SlideInterval,
    name: &str,
    local_ip: Option<String>,
) -> Result<()> {
    let local_ip = local_ip.unwrap_or_else(|| local_ip::get_local_ip(Some(&device.host)));
    if local_ip.is_empty() {
        bail!("Could not determine a local IP address; pass --local-ip");
    }
    if let Some(mac) = &device.mac {
        wol::wake(mac).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    emdx_controller::say!("🔄 Sending the slideshow to {}...", label(device));
    let switched = slideshow::send(
        &Device::new(&device.host, &device.pin),
        &local_ip,
        jpegs,
        interval,
        name,
    )
    .await?;
    if switched {
        emdx_controller::say!(
            "🖼  Switched the content player to Mobile (needed to show images from emdx)"
        );
    }
    Ok(())
}

/// The identification image: `title` in large type, then the display's
/// address, Wi-Fi network, MAC, serial and software version, sized to its
/// current orientation.
pub(crate) async fn identification_image(
    device: &Target,
    title: &str,
    ssid: Option<&str>,
) -> Result<Vec<u8>> {
    let mut s = Device::new(&device.host, &device.pin)
        .connect(DEFAULT_CONNECT_TIMEOUT)
        .await?;
    let portrait = s.portrait().await.unwrap_or(true);
    let device_name = s.device_name().await.ok();
    let serial = s.serial_number().await.ok();
    let firmware = s.software_version().await.ok();
    let mac = match &device.mac {
        Some(m) => Some(m.clone()),
        None => s.wifi_mac().await.ok(),
    };
    drop(s);

    let mut rows = vec![("IP address".to_string(), device.host.clone())];
    if let Some(ssid) = ssid {
        rows.push(("Wi-Fi network".into(), ssid.to_string()));
    }
    for (label, value) in [
        ("MAC address", mac),
        ("Serial number", serial),
        ("Software", firmware),
    ] {
        rows.push((label.to_string(), value.unwrap_or_default()));
    }
    let info = placeholder::Info {
        title: title.to_string(),
        subtitle: device_name.filter(|n| n != title),
        rows,
        footer: None,
    };
    let frame = if portrait { (1440, 2560) } else { (2560, 1440) };
    placeholder::render_jpeg(&info, frame)
}

/// Wake the display if it can be, then show `bytes` on it (see [`cms::send_image`])
pub(crate) async fn push_image(
    device: &Target,
    bytes: Vec<u8>,
    ext: &str,
    local_ip: Option<String>,
) -> Result<()> {
    let local_ip = local_ip.unwrap_or_else(|| local_ip::get_local_ip(Some(&device.host)));
    if local_ip.is_empty() {
        bail!("Could not determine a local IP address; pass --local-ip");
    }
    if let Some(mac) = &device.mac {
        println!("🔄 Waking up device...");
        wol::wake(mac).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!("✅ Device woken up\n");
    }
    emdx_controller::say!("🔄 Sending the image to {}...", label(device));
    let image = cms::Image {
        bytes,
        ext: ext.to_string(),
    };
    let switched = cms::send_image(
        &Device::new(&device.host, &device.pin),
        &local_ip,
        image,
        "emdx image",
    )
    .await?;
    if switched {
        println!("🖼  Switched the content player to Mobile (needed to show images from emdx)");
    }
    println!("✅ Image sent\n");
    Ok(())
}
