//! First-time setup ("OOBE") of a display, following the Samsung ePaper app:
//!
//! 1. Bluetooth: wait 2 s, authorize with the PIN, probe PoE/EAP support,
//!    send the network settings, wait for an address.
//! 2. MDC over the network: set the clock, offer the terms, change the PIN
//!    (optional), select the mobile content player (MobileCMS, needed to show
//!    images sent over MDC), mark setup as complete, then name the display.

use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::{
    ble::{
        proto::{AuthResult, NetworkAddress},
        session::{configure_network, wait_for_address, BleSession, Link, NetworkPlan},
    },
    mdc::{self, Device},
};

/// What to apply to the display.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The pairing PIN: the 6 digits shown under the QR code on the setup screen.
    pub pin: String,
    pub network: NetworkPlan,
    pub device_name: Option<String>,
    pub new_pin: Option<String>,
    /// Settings asked at the end of the app's setup; `None` keeps the display's own.
    pub settings: SetupSettings,
}

/// The settings pages at the end of the ePaper app's setup, applied before
/// setup is marked complete (in the app's order).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetupSettings {
    /// `true` for portrait, `false` for landscape
    pub portrait: Option<bool>,
    pub sleep_time: Option<mdc::SleepTime>,
    pub network_standby: Option<bool>,
    /// Daily refresh at `(hour, minute)`
    pub daily_refresh: Option<(u8, u8)>,
}

/// Timings (shortened in tests).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Pause before authorizing (the app waits 2 s)
    pub auth_pause: Duration,
    /// Pause after setting PoE (the app waits 3 s)
    pub poe_pause: Duration,
    pub address_timeout: Duration,
    pub address_poll: Duration,
    /// How long to keep retrying the first MDC connection
    pub mdc_connect_window: Duration,
    pub mdc_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            auth_pause: Duration::from_secs(2),
            poe_pause: Duration::from_secs(3),
            address_timeout: Duration::from_secs(90),
            address_poll: Duration::from_secs(3),
            mdc_connect_window: Duration::from_secs(60),
            // The app retries the MDC connection every 2 s
            mdc_retry: Duration::from_secs(2),
        }
    }
}

/// Bluetooth phase: authorize, configure the network, wait for an address.
pub async fn network_phase<L: Link>(
    ble: &mut BleSession<L>,
    plan: &Plan,
    timing: Timing,
    progress: &mut dyn FnMut(&str),
) -> Result<NetworkAddress> {
    progress("🔐 Authorizing with the display's PIN...");
    tokio::time::sleep(timing.auth_pause).await;
    match ble
        .authorize(&plan.pin)
        .await
        .context("Bluetooth authorization")?
    {
        r if r.is_ok() => {}
        AuthResult::WrongPin => {
            bail!("wrong PIN — use the pairing PIN: the 6 digits under the QR code on the display")
        }
        other => bail!("Bluetooth authorization failed: {other}"),
    }
    // The app's capability probes, in its order
    let _poe = ble.poe_available().await.unwrap_or(false);
    let _eap = ble.eap_supported().await.unwrap_or(false);
    progress("📶 Sending Wi-Fi settings...");
    configure_network(ble, &plan.network, timing.poe_pause).await?;
    progress("⏳ Waiting for the display to join the network...");
    wait_for_address(ble, timing.address_timeout, timing.address_poll).await
}

/// Outcome of the MDC phase.
#[derive(Debug, Clone, Default)]
pub struct Finished {
    pub terms_url: Option<String>,
    /// `Some(false)` when the display has terms that were not accepted.
    pub terms_accepted: Option<bool>,
    pub wifi_mac: Option<String>,
    /// PIN to use from now on.
    pub pin: String,
}

/// MDC phase on the freshly connected display.
///
/// `accept_terms` is called with the terms page URL when the display has
/// terms that weren't accepted yet; it must only return `true` when the user
/// agreed. Declining sends nothing and setup continues, as in the app.
pub async fn finish_phase(
    host: &str,
    port: u16,
    plan: &Plan,
    timing: Timing,
    progress: &mut dyn FnMut(&str),
    accept_terms: &mut dyn FnMut(&str) -> Result<bool>,
) -> Result<Finished> {
    progress(&format!("🔗 Connecting to {host} over the network..."));
    let device = Device::new(host, &plan.pin).with_port(port);
    let deadline = tokio::time::Instant::now() + timing.mdc_connect_window;
    let mut s = loop {
        match device.connect(mdc::DEFAULT_CONNECT_TIMEOUT).await {
            Ok(s) => break s,
            Err(mdc::MdcError::AuthFailed(why)) => bail!("MDC login failed: {why}"),
            Err(e) if tokio::time::Instant::now() >= deadline => {
                return Err(anyhow::Error::new(e).context(format!(
                    "the display joined the network but MDC on {host}:{port} did not answer"
                )));
            }
            Err(_) => tokio::time::sleep(timing.mdc_retry).await,
        }
    };

    let result: Result<Finished> = async {
        progress("🕒 Setting the clock...");
        s.set_clock(&chrono::Local::now().naive_local())
            .await
            .context("setting the clock")?;

        let terms = s.terms().await.unwrap_or_default();
        let mut terms_accepted = terms.agreed;
        if let (false, Some(url)) = (terms.agreed == Some(true), terms.url.as_deref()) {
            if accept_terms(url)? {
                progress("📄 Accepting the terms & conditions...");
                s.accept_terms().await.context("accepting the terms")?;
                terms_accepted = Some(true);
            } else {
                progress("⚠️  Terms & conditions not accepted (nothing was sent)");
                terms_accepted = Some(false);
            }
        }

        // The app's settings pages: orientation, sleep time and network standby,
        // daily refresh, then the PIN
        let set = plan.settings;
        if let Some(portrait) = set.portrait {
            progress(if portrait {
                "🔄 Orientation: portrait..."
            } else {
                "🔄 Orientation: landscape..."
            });
            s.set_portrait(portrait)
                .await
                .context("setting the orientation")?;
        }
        if let Some(t) = set.sleep_time {
            // Never sleeping needs mains power (the app's rule); on battery the
            // display keeps its own sleep time rather than failing the setup here
            let on_battery = t == mdc::SleepTime::Off
                && s.battery().await.ok().and_then(|b| b.plugged_in()) == Some(false);
            if on_battery {
                progress(
                    "⚠️  Sleep after: kept the display's own. To turn off sleep mode, connect \
                     the device to a power cord.",
                );
            } else {
                progress(&format!("💤 Sleep after: {t}..."));
                s.set_sleep_time(t)
                    .await
                    .context("setting the sleep time")?;
            }
        }
        if let Some(on) = set.network_standby {
            progress(if on {
                "📶 Network standby: on..."
            } else {
                "📶 Network standby: off..."
            });
            s.set_network_standby(on)
                .await
                .context("setting network standby")?;
        }
        if let Some((h, m)) = set.daily_refresh {
            progress(&format!("🔁 Screen protection time: {h:02}:{m:02}..."));
            s.set_daily_refresh(Some((h, m)))
                .await
                .context("setting the screen protection time")?;
        }
        let mut pin = plan.pin.clone();
        if let Some(new_pin) = &plan.new_pin {
            progress("🔑 Changing the PIN...");
            s.set_pin(new_pin).await.context("changing the PIN")?;
            pin = new_pin.clone();
        }
        // The app's last setting before completing setup. Without it the display
        // downloads images sent over MDC but doesn't show them.
        progress("🖼  Selecting the mobile content player...");
        s.set_content_player(mdc::content_player::MOBILE_CMS)
            .await
            .context("selecting the content player")?;
        progress("✅ Marking setup as complete...");
        s.set_setup_done(true).await.context("completing setup")?;
        // Device name is the app's last step
        if let Some(name) = &plan.device_name {
            progress(&format!("🏷  Naming the display \"{name}\"..."));
            s.set_device_name(name)
                .await
                .context("setting the device name")?;
        }
        let wifi_mac = s.wifi_mac().await.ok();
        Ok(Finished {
            terms_url: terms.url,
            terms_accepted,
            wifi_mac,
            pin,
        })
    }
    .await;
    s.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ble::session::fake::FakeDisplay;

    fn fast() -> Timing {
        Timing {
            auth_pause: Duration::ZERO,
            poe_pause: Duration::ZERO,
            address_timeout: Duration::from_millis(200),
            address_poll: Duration::from_millis(1),
            mdc_connect_window: Duration::from_millis(200),
            mdc_retry: Duration::from_millis(20),
        }
    }

    fn plan(pin: &str) -> Plan {
        Plan {
            pin: pin.into(),
            network: NetworkPlan::Wifi {
                ssid: "Home".into(),
                password: "secret".into(),
            },
            device_name: None,
            new_pin: None,
            settings: Default::default(),
        }
    }

    #[tokio::test]
    async fn network_phase_returns_the_address() {
        let mut ble = BleSession::new(FakeDisplay::new("123456"));
        let mut log = Vec::new();
        let addr = network_phase(&mut ble, &plan("123456"), fast(), &mut |m| {
            log.push(m.to_string())
        })
        .await
        .unwrap();
        assert_eq!(addr.ip.to_string(), "192.168.1.77");
        assert!(log[0].contains("Authorizing") && log.iter().any(|l| l.contains("Wi-Fi")));
        assert_eq!(ble.into_link().wifi, Some(("Home".into(), "secret".into())));
    }

    #[tokio::test]
    async fn wrong_pin_stops_before_any_change() {
        let mut ble = BleSession::new(FakeDisplay::new("123456"));
        let err = network_phase(&mut ble, &plan("000000"), fast(), &mut |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("wrong PIN"), "{err}");
        let d = ble.into_link();
        assert!(d.wifi.is_none() && d.ip_config.is_none());
    }
}
