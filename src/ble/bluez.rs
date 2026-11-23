//! BlueZ transport (Linux, via D-Bus) for the BLE setup protocol.

use std::{pin::Pin, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use bluer::{
    agent::{Agent, AgentHandle},
    gatt::remote::{Characteristic, CharacteristicWriteRequest},
    gatt::WriteOp,
    Adapter, AdapterEvent, Address, AddressType, Device, DiscoveryFilter, DiscoveryTransport,
    Session,
};
use tokio_stream::{Stream, StreamExt};

use super::{
    proto::{self, Advert},
    session::Link,
};

/// How long to wait for the OS to bond after the first protected write.
const BOND_WAIT: Duration = Duration::from_secs(10);
/// How long to wait for the display to start pairing (it does so ~110 ms after
/// connecting; the app allows 20 s after service discovery).
const PASSIVE_BOND_WAIT: Duration = Duration::from_secs(20);

/// A display seen while scanning.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Found {
    pub address: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
    /// `Some` when advertising in setup mode (connectable for setup).
    pub advert: Option<Advert>,
}

impl Found {
    pub fn in_setup_mode(&self) -> bool {
        self.advert.is_some_and(|a| a.setup_mode)
    }
}

async fn adapter(session: &Session) -> Result<Adapter> {
    let adapter = session.default_adapter().await.context(
        "no Bluetooth adapter found (is Bluetooth enabled? try `rfkill unblock bluetooth`)",
    )?;
    adapter
        .set_powered(true)
        .await
        .context("powering on the Bluetooth adapter")?;
    Ok(adapter)
}

fn looks_like_emdx(name: Option<&str>) -> bool {
    name.is_some_and(|n| {
        n.contains("EMDX") || n.contains("ePaper") || n.contains("EM32DX") || n.contains("EM13DX")
    })
}

/// How long to look for a display's advert right before connecting (the app: 10 s)
const ADVERT_SCAN: Duration = Duration::from_secs(10);

/// The setup advert of `addr`: from BlueZ's cache, else a short LE scan for it
async fn advert_of(adapter: &Adapter, addr: Address, wait: Duration) -> Option<Advert> {
    let read = || async {
        adapter
            .device(addr)
            .ok()?
            .manufacturer_data()
            .await
            .ok()
            .flatten()?
            .get(&proto::SAMSUNG_COMPANY_ID)
            .and_then(|data| proto::parse_advert(data))
    };
    if let Some(a) = read().await {
        return Some(a);
    }
    let events = adapter.discover_devices().await.ok()?;
    let deadline = tokio::time::Instant::now() + wait;
    let mut found = None;
    while tokio::time::Instant::now() < deadline {
        if let Some(a) = read().await {
            found = Some(a);
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    drop(events); // stops discovery
    found
}

/// Scan for EMDX displays. Displays in setup mode are identified by their
/// advertisement; already-set-up ones only by name (they send a generic beacon).
pub async fn scan(duration: Duration) -> Result<Vec<Found>> {
    let session = Session::new()
        .await
        .context("connecting to BlueZ over D-Bus")?;
    let adapter = adapter(&session).await?;
    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            ..Default::default()
        })
        .await
        .ok();
    let mut events = adapter.discover_devices().await?;
    let mut seen = Vec::new();
    let deadline = tokio::time::sleep(duration);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ev = events.next() => match ev {
                Some(AdapterEvent::DeviceAdded(addr)) if !seen.contains(&addr) => seen.push(addr),
                Some(_) => {}
                None => break,
            }
        }
    }
    drop(events); // stops discovery

    let mut found = Vec::new();
    for addr in seen {
        let Ok(device) = adapter.device(addr) else {
            continue;
        };
        let name = device.name().await.ok().flatten();
        let advert = device
            .manufacturer_data()
            .await
            .ok()
            .flatten()
            .and_then(|m| m.get(&proto::SAMSUNG_COMPANY_ID).cloned())
            .and_then(|data| proto::parse_advert(&data));
        if advert.is_none() && !looks_like_emdx(name.as_deref()) {
            continue;
        }
        found.push(Found {
            address: addr.to_string(),
            name,
            rssi: device.rssi().await.ok().flatten(),
            advert,
        });
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.rssi.unwrap_or(i16::MIN)));
    Ok(found)
}

/// Pairing agent. A display in setup mode starts pairing itself and shows a
/// random 6-digit passkey (a new one on every attempt); on a phone, Android's
/// own pairing dialog asks for it. Answer with `passkey` when given, and
/// accept "Just Works" confirmations.
fn pairing_agent(passkey: Option<u32>) -> Agent {
    Agent {
        request_default: true,
        request_passkey: Some(Box::new(move |_req| {
            Box::pin(async move { passkey.ok_or(bluer::agent::ReqError::Rejected) })
        })),
        request_pin_code: Some(Box::new(move |_req| {
            Box::pin(async move {
                passkey
                    .map(|p| format!("{p:06}"))
                    .ok_or(bluer::agent::ReqError::Rejected)
            })
        })),
        // Numeric comparison: show the code so it can be checked on the display
        request_confirmation: Some(Box::new(|req| {
            Box::pin(async move {
                crate::say!(
                    "🔑 Bluetooth pairing code {:06} (should match the display)",
                    req.passkey
                );
                Ok(())
            })
        })),
        request_authorization: Some(Box::new(|_req| Box::pin(async { Ok(()) }))),
        authorize_service: Some(Box::new(|_req| Box::pin(async { Ok(()) }))),
        ..Default::default()
    }
}

/// Connected GATT link to a display's setup characteristic.
pub struct BluezLink {
    adapter: Adapter,
    device: Device,
    characteristic: Characteristic,
    notifications: Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>,
    mtu: usize,
    _agent: AgentHandle,
    _session: Session,
}

impl BluezLink {
    /// Connect, pair if needed, and subscribe to the command characteristic.
    /// `passkey` answers the display's pairing request (the PIN on its screen).
    pub async fn connect(
        address: &str,
        passkey: Option<u32>,
        timeout: Duration,
    ) -> Result<BluezLink> {
        let addr: Address = address
            .parse()
            .map_err(|_| anyhow!("invalid Bluetooth address {address:?}"))?;
        let session = Session::new()
            .await
            .context("connecting to BlueZ over D-Bus")?;
        let agent = session.register_agent(pairing_agent(passkey)).await?;
        let adapter = adapter(&session).await?;

        // The display is reached over LE only (it has no Classic profiles)
        adapter
            .set_discovery_filter(DiscoveryFilter {
                transport: DiscoveryTransport::Le,
                ..Default::default()
            })
            .await
            .ok();
        // Which pairing flow, from the display's advert (the app's `isOldBLE`). Like
        // the app, scan for it right before connecting: BlueZ forgets a scanned
        // device ~30 s after its scan, and connect_le removes its entry anyway.
        let new_stack = advert_of(&adapter, addr, ADVERT_SCAN)
            .await
            .is_some_and(|a| !a.old_ble);
        // The display is dual-mode, and BlueZ's generic Device1.Connect() picks
        // Bluetooth Classic for it, even when it was only seen over LE (observed
        // with btmon), where the setup service doesn't exist. Only
        // Adapter1.ConnectDevice can force LE, so there is no fallback.
        let device = connect_le(&adapter, addr, timeout)
            .await
            .with_context(|| format!("connecting to {address} over Bluetooth LE"))?;
        // From here on, any failure must not leave a connection or a pairing behind
        let ready = async {
            // New stack (advert flags bit 1 set, e.g. software S-RSEDWWC-1130.7): the
            // display doesn't start pairing (no SMP Security Request in 20 s, seen with
            // btmon) and hides its setup service until the link is paired, so this
            // computer starts it (LE Secure Connections, Just Works).
            if new_stack && !device.is_paired().await.unwrap_or(false) {
                crate::say!("🤝 Pairing with the display (newer Bluetooth stack)...");
                // BlueZ's Pair() may never return although the pairing completed
                // (keys stored within a second, seen with btmon): watch `Paired`
                // instead. Pair() runs in its own task and is never dropped while
                // pending: bluer sends CancelPairing for a dropped Pair(), which
                // made BlueZ undo the finished pairing and disconnect (btmon).
                let (pair_tx, mut pair_rx) = tokio::sync::oneshot::channel();
                let pairing = device.clone();
                tokio::spawn(async move {
                    let _ = pair_tx.send(pairing.pair().await);
                });
                let deadline = tokio::time::sleep(PASSIVE_BOND_WAIT);
                tokio::pin!(deadline);
                let mut pair_done = false;
                loop {
                    if device.is_paired().await.unwrap_or(false) {
                        break;
                    }
                    tokio::select! {
                        r = &mut pair_rx, if !pair_done => {
                            pair_done = true;
                            if let Ok(Err(e)) = r {
                                if !device.is_paired().await.unwrap_or(false) {
                                    bail!("the display refused Bluetooth pairing ({e})");
                                }
                            }
                        }
                        _ = &mut deadline => bail!(
                            "Bluetooth pairing didn't finish within {} s",
                            PASSIVE_BOND_WAIT.as_secs()
                        ),
                        _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                    }
                }
                crate::say!("🤝 Paired");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            // Like the app: wait for the display to pair first (it starts the
            // pairing itself), then look for the setup characteristic. That
            // characteristic is protected, and BlueZ only lists the setup service
            // once it could read it, i.e. on an encrypted link (seen with btmon on
            // a display that had been set up before).
            if !device.is_paired().await.unwrap_or(false) {
                crate::say!("⏳ Waiting for the display to start Bluetooth pairing...");
                let deadline = tokio::time::Instant::now() + PASSIVE_BOND_WAIT;
                while tokio::time::Instant::now() < deadline
                    && !device.is_paired().await.unwrap_or(false)
                    && device.is_connected().await.unwrap_or(false)
                {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                if device.is_paired().await.unwrap_or(false) {
                    crate::say!("🤝 Paired (started by the display)");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                } else if !device.is_connected().await.unwrap_or(false) {
                    bail!("the display disconnected while waiting for it to start pairing");
                } else {
                    // The display sends an SMP Security Request ~110 ms after connecting
                    // (seen with the real app). Starting pairing ourselves after a refusal
                    // only deepens its "repeated attempts" lockout, so stop here.
                    let _ = device.disconnect().await;
                    bail!(
                        "the display didn't start Bluetooth pairing within {} s. It may be refusing \
                         this computer after earlier failed attempts (a factory reset clears that)",
                        PASSIVE_BOND_WAIT.as_secs()
                    );
                }
            }
            let characteristic = find_characteristic(&device).await?;
            let mtu = characteristic.mtu().await.unwrap_or(23);
            let notifications = Box::pin(
                characteristic
                    .notify()
                    .await
                    .context("subscribing to display notifications")?,
            );
            Ok::<_, anyhow::Error>((characteristic, notifications, mtu))
        }
        .await;
        let (characteristic, notifications, mtu) = match ready {
            Ok(parts) => parts,
            Err(e) => {
                let _ = device.disconnect().await;
                let _ = adapter.remove_device(device.address()).await;
                return Err(e);
            }
        };
        let mut link = BluezLink {
            adapter,
            device,
            characteristic,
            notifications,
            mtu,
            _agent: agent,
            _session: session,
        };
        link.bond().await;
        Ok(link)
    }

    /// The app never pairs explicitly: its first START_AUTH write makes the
    /// OS bond (the setting characteristic is protected), then it waits 1 s.
    /// Do the same, fall back to an explicit pairing, and drop the stale reply.
    async fn bond(&mut self) {
        if self.device.is_paired().await.unwrap_or(false) {
            return;
        }
        if let Ok(frames) = proto::encode_request(
            proto::cmd::START_AUTH,
            &[],
            proto::op::AUTHORIZATION,
            self.mtu,
        ) {
            for frame in &frames {
                let _ = self.write(frame).await;
            }
        }
        let deadline = tokio::time::Instant::now() + BOND_WAIT;
        while tokio::time::Instant::now() < deadline {
            if self.device.is_paired().await.unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if !self.device.is_paired().await.unwrap_or(false) {
            if let Err(e) = self.device.pair().await {
                crate::say_err!(
                    "⚠️  Bluetooth pairing did not complete ({e}); continuing unpaired"
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        // Discard the reply to the bonding START_AUTH
        while tokio::time::timeout(Duration::from_millis(300), self.notifications.next())
            .await
            .is_ok_and(|n| n.is_some())
        {}
    }

    /// Disconnect and forget the bond (the app removes it too).
    pub async fn close(self) {
        let _ = self.device.disconnect().await;
        let _ = self.adapter.remove_device(self.device.address()).await;
    }
}

/// What to do when bluetoothd has no `ConnectDevice` (experimental API off).
pub const EXPERIMENTAL_HELP: &str = "BlueZ's LE connect (Adapter1.ConnectDevice) is unavailable: \
    bluetoothd runs without its experimental API. Without it, BlueZ connects to this display over \
    Bluetooth Classic, where setup is impossible. Fix: set `Experimental = true` in \
    /etc/bluetooth/main.conf, then `sudo systemctl restart bluetooth`";

/// Connect over LE with `Adapter1.ConnectDevice` (never the generic connect).
/// It refuses addresses BlueZ already knows, so a stale entry is removed first.
async fn connect_le(adapter: &Adapter, addr: Address, timeout: Duration) -> Result<Device> {
    let mut last = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let known = adapter.device_addresses().await?.contains(&addr);
        if known {
            let device = adapter.device(addr)?;
            if device.is_connected().await.unwrap_or(false) {
                return Ok(device);
            }
            let _ = adapter.remove_device(addr).await;
        }
        match tokio::time::timeout(timeout, adapter.connect_device(addr, AddressType::LePublic))
            .await
        {
            Ok(Ok(device)) => return Ok(device),
            Ok(Err(e))
                if e.message.contains("doesn't exist") || e.message.contains("UnknownMethod") =>
            {
                bail!(EXPERIMENTAL_HELP)
            }
            Ok(Err(e)) => last = Some(anyhow!(e)),
            Err(_) => {
                last = Some(anyhow!(
                    "timed out; is the display in setup (pairing) mode and in range?"
                ))
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("could not connect")))
}

/// Find the setup characteristic. BlueZ may report services as resolved from
/// a partial cache, so look again for a while before giving up.
async fn find_characteristic(device: &Device) -> Result<Characteristic> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let mut seen = Vec::new();
        if let Ok(services) = device.services().await {
            for service in services {
                let Ok(uuid) = service.uuid().await else {
                    continue;
                };
                seen.push(uuid.to_string());
                if uuid != proto::SERVICE_UUID {
                    continue;
                }
                for ch in service.characteristics().await.unwrap_or_default() {
                    if ch.uuid().await.ok() == Some(proto::COMMAND_UUID) {
                        return Ok(ch);
                    }
                }
            }
        }
        let connected = device.is_connected().await.unwrap_or(false);
        if !connected || tokio::time::Instant::now() >= deadline {
            bail!(
                "the display's setup characteristic was not found ({}; services seen: {})",
                if connected {
                    "still connected"
                } else {
                    "the display disconnected"
                },
                if seen.is_empty() {
                    "none".to_string()
                } else {
                    seen.join(", ")
                }
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

impl Link for BluezLink {
    fn mtu(&self) -> usize {
        self.mtu
    }

    async fn write(&mut self, frame: &[u8]) -> Result<()> {
        let req = CharacteristicWriteRequest {
            op_type: WriteOp::Request,
            ..Default::default()
        };
        self.characteristic
            .write_ext(frame, &req)
            .await
            .context("writing to the display over Bluetooth")
    }

    async fn recv(&mut self) -> Result<Vec<u8>> {
        self.notifications
            .next()
            .await
            .context("Bluetooth notifications stopped (display disconnected?)")
    }
}
