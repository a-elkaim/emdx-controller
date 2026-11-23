//! `emdx thumbnail`: ask a display for a thumbnail of whatever it is showing.
//!
//! The ePaper app never pulls a thumbnail: it sends the display a callback URL
//! (MDC `0xC7` sub `0x54`) and the display `POST`s a `multipart/form-data` upload
//! back to it. This command runs that receiver, sends the request, and saves the
//! image.

use std::{path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use clap::Args;

use emdx_controller::{
    local_ip,
    mdc::{content_player, Device, DEFAULT_CONNECT_TIMEOUT},
    thumbnail::{self, Upload},
    wol,
};

use crate::{
    config::{self, CliConfig},
    label, DeviceArgs,
};

#[derive(Args, Debug)]
pub(crate) struct ThumbnailArgs {
    #[command(flatten)]
    pub device: DeviceArgs,
    /// Folder to save thumbnails in (default: <settings folder>/thumbnails)
    #[arg(short, long)]
    pub out: Option<PathBuf>,
    /// Local IP the display should upload to (default: auto)
    #[arg(long)]
    pub local_ip: Option<String>,
    /// Port for the upload receiver (default: the ePaper app's own, 6868, or 6869 with --secure)
    #[arg(long)]
    pub port: Option<u16>,
    /// Do what the ePaper app does by default: push our certificate to the display
    /// (MDC 0xD2/0x20) and receive the upload over HTTPS
    #[arg(long)]
    pub secure: bool,
    /// Close the MDC session as soon as the display acknowledges, instead of keeping
    /// it open until the upload arrives (the app keeps its socket open)
    #[arg(long)]
    pub no_hold: bool,
    /// Seconds to wait for the display to upload
    #[arg(long, default_value_t = 45)]
    pub timeout: u64,
    /// Keep listening for later uploads: a panel uploads again whenever its content changes
    #[arg(long)]
    pub watch: bool,
    /// Only listen for uploads; don't connect to the display or send the request
    #[arg(long)]
    pub listen_only: bool,
    /// Don't send a Wake-on-LAN packet first
    #[arg(long)]
    pub no_wake: bool,
    /// Rotate the saved image. `auto` turns it upright from the display's
    /// orientation (the panel sends portrait screens sideways)
    #[arg(long, value_enum, default_value = "auto")]
    pub rotate: RotateArg,
    /// Print the multipart fields the panel sent, as JSON
    #[arg(long)]
    pub json: bool,
    /// Open the saved thumbnail with the desktop's viewer
    #[arg(long)]
    pub open: bool,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum RotateArg {
    /// Upright, from the display's orientation
    Auto,
    /// Save it exactly as the display sent it
    None,
    /// 90° clockwise, like the ePaper app does for portrait panels
    #[value(name = "90")]
    Clockwise90,
    #[value(name = "180")]
    Half,
    /// 90° counter-clockwise
    #[value(name = "270")]
    CounterClockwise90,
}

impl RotateArg {
    /// Fixed rotation, `None` for `auto`
    fn degrees(self) -> Option<u16> {
        Some(match self {
            Self::Auto => return None,
            Self::None => 0,
            Self::Clockwise90 => 90,
            Self::Half => 180,
            Self::CounterClockwise90 => 270,
        })
    }
}

pub(crate) async fn run(
    args: ThumbnailArgs,
    display: Option<&str>,
    saved: &CliConfig,
) -> Result<()> {
    let out_dir = match &args.out {
        Some(p) => p.clone(),
        None => thumbnail::default_out_dir(&config::default_path()?),
    };
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating the thumbnail folder {}", out_dir.display()))?;

    // --listen-only never opens the MDC socket, so it needs no display at all
    let device = match args.listen_only {
        true => None,
        false => Some(args.device.target(display, saved)?),
    };

    let local_ip = match &args.local_ip {
        Some(ip) => ip.clone(),
        None => local_ip::get_local_ip(device.as_ref().map(|d| d.host.as_str())),
    };
    if local_ip.is_empty() {
        bail!("Could not determine a local IP address; pass --local-ip");
    }

    let mut endpoint = match args.secure {
        true => {
            let port = args.port.unwrap_or(thumbnail::SECURE_PORT);
            thumbnail::Endpoint::start_secure(&local_ip, port, &out_dir).await?
        }
        false => {
            let port = args.port.unwrap_or(thumbnail::NON_SECURE_PORT);
            thumbnail::Endpoint::start(&local_ip, port, &out_dir).await?
        }
    };
    emdx_controller::say!("📥 Waiting for an upload at {}", endpoint.url);

    let timeout = Duration::from_secs(args.timeout.max(1));
    // Uploaded while the MDC session was still open
    let mut early = None;
    // `--rotate auto`: 90° clockwise for portrait, as the ePaper app does
    let mut degrees = args.rotate.degrees().unwrap_or(0);
    let auto = args.rotate.degrees().is_none();
    if let Some(device) = &device {
        if !args.no_wake {
            if let Some(mac) = &device.mac {
                emdx_controller::say!("🔄 Waking up device...");
                wol::wake(mac).await?;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        emdx_controller::say!("🔄 Connecting to {}...", label(device));
        let url = endpoint.url.clone();
        let cert_pem = endpoint.cert_pem.clone();
        let hold = !args.no_hold;
        let endpoint = &mut endpoint;
        let degrees = &mut degrees;
        early = Device::new(&device.host, &device.pin)
            .with_session(DEFAULT_CONNECT_TIMEOUT, async |s| {
                // Only a hint: the upload reports the player too
                if let Ok(p) = s.content_player().await {
                    let name = content_player::name(p).unwrap_or("unknown");
                    emdx_controller::say!("   Content player: {name} ({p})");
                }
                if auto {
                    let portrait = s.portrait().await.unwrap_or(false);
                    *degrees = if portrait { 90 } else { 0 };
                }
                if let Some(pem) = &cert_pem {
                    s.push_cms_cert(pem).await?;
                    emdx_controller::say!("🔐 The display accepted our certificate");
                }
                match s.request_thumbnail(&url).await? {
                    Some(echo) if echo == url => {
                        emdx_controller::say!("✅ The display stored the callback URL")
                    }
                    Some(text) => emdx_controller::say!("✅ The display answered: {text}"),
                    None => emdx_controller::say!("✅ The display acknowledged the request"),
                }
                if !hold {
                    return Ok(None);
                }
                emdx_controller::say!(
                    "⏳ Keeping the MDC session open until the upload arrives..."
                );
                Ok(Some(endpoint.wait(timeout).await))
            })
            .await
            .with_context(|| {
                format!(
                    "asking {} for a thumbnail (is it awake? try `emdx wake`)",
                    label(device)
                )
            })?;
    }

    let mut first = true;
    loop {
        let upload = if let Some(result) = early.take() {
            result?
        } else if first {
            endpoint.wait(timeout).await?
        } else {
            endpoint
                .next()
                .await
                .context("the upload receiver stopped")?
        };
        first = false;
        report(upload, degrees, args.json, args.open).await?;
        if !args.watch {
            break;
        }
        emdx_controller::say!("\n📥 Waiting for the next upload (Ctrl-C to stop)...");
    }
    endpoint.shutdown();
    Ok(())
}

async fn report(upload: Upload, degrees: u16, as_json: bool, open: bool) -> Result<()> {
    println!();
    for line in upload.summary() {
        emdx_controller::say!("   {line}");
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&upload.metadata_json())?);
    }
    let path = upload.saved.clone().context("the upload was not saved")?;
    emdx_controller::say!("💾 Saved {}", path.display());
    let mut upload = upload;
    let saved_at = upload.rotate_saved(degrees).await?;
    if degrees != 0 {
        emdx_controller::say!("🔃 Rotated {degrees}° → {}", saved_at.display());
    }

    if open {
        if let Err(e) = open_in_viewer(&saved_at) {
            emdx_controller::say_err!("   (couldn't open it: {e:#})");
        }
    }
    Ok(())
}

/// Hand the file to the desktop's viewer (`xdg-open`, or whatever `EMDX_OPENER` says).
fn open_in_viewer(path: &std::path::Path) -> Result<()> {
    let command_line = std::env::var("EMDX_OPENER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "xdg-open".to_string());
    let mut words = command_line.split_whitespace();
    let program = words.next().context("EMDX_OPENER is empty")?;
    let mut child = std::process::Command::new(program)
        .args(words)
        .arg(path)
        .spawn()
        .context("starting the image viewer")?;
    let status = child.wait().context("waiting for the image viewer")?;
    if !status.success() {
        bail!("the image viewer exited with {status}");
    }
    Ok(())
}
