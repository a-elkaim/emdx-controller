//! Command/response session over any BLE link (real BlueZ or a test double).

use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::proto::{self, cmd, op, AuthResult, Challenge, DeviceStatus, NetworkAddress};

/// A connected GATT link to the display's command characteristic.
#[allow(async_fn_in_trait)]
pub trait Link {
    /// Negotiated ATT MTU.
    fn mtu(&self) -> usize;
    /// Write one frame to the command characteristic (with response).
    async fn write(&mut self, frame: &[u8]) -> Result<()>;
    /// Next notification from the command characteristic.
    async fn recv(&mut self) -> Result<Vec<u8>>;
}

/// Delay between chunks of a fragmented write (the app uses 200 ms).
pub const CHUNK_DELAY: Duration = Duration::from_millis(200);

pub struct BleSession<L: Link> {
    link: L,
    reassembler: proto::Reassembler,
    pub reply_timeout: Duration,
}

impl<L: Link> BleSession<L> {
    pub fn new(link: L) -> Self {
        BleSession {
            link,
            reassembler: proto::Reassembler::default(),
            // The app waits 20 s for every reply
            reply_timeout: Duration::from_secs(20),
        }
    }

    pub fn into_link(self) -> L {
        self.link
    }

    /// Send a command and return the data of its successful reply.
    pub async fn request(&mut self, command: u8, data: &[u8], op: u8) -> Result<Vec<u8>> {
        let frames = proto::encode_request(command, data, op, self.link.mtu())?;
        let chunked = frames.len() > 1;
        for frame in &frames {
            if chunked {
                tokio::time::sleep(CHUNK_DELAY).await;
            }
            self.link.write(frame).await?;
        }
        let wait = async {
            loop {
                let frame = self.link.recv().await?;
                let Some(reply) = self.reassembler.feed(&frame)? else {
                    continue; // more fragments to come
                };
                if !proto::replies_to(command, reply.command) {
                    continue; // late reply to an earlier command
                }
                if reply.status != 0 {
                    return Err(proto::ProtoError::Status {
                        command,
                        status: reply.status,
                    }
                    .into());
                }
                return Ok(reply.data);
            }
        };
        tokio::time::timeout(self.reply_timeout, wait)
            .await
            .with_context(|| format!("no reply from the display to BLE command {command}"))?
    }

    async fn get(&mut self, command: u8) -> Result<Vec<u8>> {
        self.request(command, &[], op::REQUEST_WITH_RESPONSE).await
    }

    async fn set(&mut self, command: u8, data: &[u8]) -> Result<Vec<u8>> {
        self.request(command, data, op::REQUEST_WITH_RESPONSE).await
    }

    /// bcrypt challenge/response with the display's 6-digit PIN.
    pub async fn authorize(&mut self, pin: &str) -> Result<AuthResult> {
        let challenge = proto::parse_challenge(
            &self
                .request(cmd::START_AUTH, &[], op::AUTHORIZATION)
                .await?,
        )?;
        let (cost, salt) = match challenge {
            Challenge::Result(result) => return Ok(result),
            Challenge::Bcrypt { cost, salt, .. } => (cost, salt),
        };
        let digest = proto::auth_digest(pin, cost, &salt)?;
        let reply = self
            .request(cmd::ANSWER_AUTH, &digest, op::AUTHORIZATION)
            .await?;
        let byte = *reply.first().context("empty authorization answer")?;
        Ok(AuthResult::from_byte(byte))
    }

    pub async fn device_status(&mut self) -> Result<DeviceStatus> {
        Ok(proto::parse_device_status(
            &self.get(cmd::DEVICE_STATUS).await?,
        )?)
    }

    pub async fn network_address(&mut self) -> Result<NetworkAddress> {
        Ok(proto::parse_network_address(
            &self.get(cmd::NETWORK_ADDRESS).await?,
        )?)
    }

    pub async fn wifi_ssid(&mut self) -> Result<String> {
        Ok(proto::parse_wifi_ssid(&self.get(cmd::WIFI).await?)?)
    }

    pub async fn set_network_type_wifi(&mut self) -> Result<()> {
        self.set(cmd::NETWORK_TYPE, &proto::NETWORK_TYPE_WIFI)
            .await
            .map(drop)
    }

    pub async fn set_wifi(&mut self, ssid: &str, password: &str) -> Result<()> {
        self.set(cmd::WIFI, &proto::wifi_data(ssid, password)?)
            .await
            .map(drop)
    }

    /// Get the address automatically (DHCP).
    pub async fn set_dhcp(&mut self) -> Result<()> {
        self.set(cmd::NETWORK_ADDRESS, &proto::dhcp_config_data())
            .await
            .map(drop)
    }

    /// PoE+ power. Displays without PoE answer "not supported", which is fine.
    /// PoE+ power. Like the app, any status the display returns is ignored;
    /// only transport failures (no reply, disconnect) are errors.
    pub async fn set_poe(&mut self, enabled: bool) -> Result<()> {
        match self.set(cmd::POE_ACTIVATION, &[u8::from(enabled)]).await {
            Ok(_) => Ok(()),
            Err(e) if e.downcast_ref::<proto::ProtoError>().is_some() => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn poe_available(&mut self) -> Result<bool> {
        match self.get(cmd::POE_ACTIVATION).await {
            Ok(d) => Ok(d.first().is_some_and(|b| *b != 0)),
            Err(e) if is_not_supported(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Enterprise Wi-Fi (EAP) support, probed like the app does.
    pub async fn eap_supported(&mut self) -> Result<bool> {
        match self.get(cmd::GET_CA_CERT_LIST).await {
            Ok(_) => Ok(true),
            Err(e) if is_not_supported(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

fn is_not_supported(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<proto::ProtoError>(),
        Some(proto::ProtoError::Status { status: 37, .. })
    )
}

/// What to configure on the display's network side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkPlan {
    /// Wi-Fi with an automatic address (DHCP)
    Wifi { ssid: String, password: String },
}

/// The app's network sequence: PoE off (it sends it for Wi-Fi too), pause,
/// network type, Wi-Fi credentials, automatic address.
pub async fn configure_network<L: Link>(
    s: &mut BleSession<L>,
    plan: &NetworkPlan,
    pause: Duration,
) -> Result<()> {
    s.set_poe(false).await.context("setting PoE mode")?;
    tokio::time::sleep(pause).await;
    let NetworkPlan::Wifi { ssid, password } = plan;
    s.set_network_type_wifi().await.context("selecting Wi-Fi")?;
    s.set_wifi(ssid, password)
        .await
        .context("sending Wi-Fi credentials")?;
    s.set_dhcp().await.context("sending IP settings")
}

/// Poll until the display reports a usable address (DHCP can take a while).
pub async fn wait_for_address<L: Link>(
    s: &mut BleSession<L>,
    timeout: Duration,
    poll: Duration,
) -> Result<NetworkAddress> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // The app reads the address once and lets MDC retries cover the
        // delay; polling until it is non-zero is more reliable.
        let addr = s.network_address().await?;
        if !addr.ip.is_unspecified() {
            return Ok(addr);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the display did not get a network address within {}s (check the Wi-Fi name and password)",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A simulated display speaking the BLE setup protocol.

    use std::collections::VecDeque;

    use super::*;
    use crate::ble::proto::header;

    pub struct FakeDisplay {
        pub mtu: usize,
        pub pin: String,
        pub salt: [u8; 16],
        pub authorized: bool,
        pub poe_supported: bool,
        pub eap_supported: bool,
        pub dhcp_ip: [u8; 4],
        /// Polls answered with 0.0.0.0 before the address appears
        pub dhcp_delay_polls: usize,
        pub received: Vec<(u8, Vec<u8>)>,
        pub wifi: Option<(String, String)>,
        pub ip_config: Option<Vec<u8>>,
        pub network_type: Option<u8>,
        pub silent: bool,
        /// Answer GET_CA_CERT_LIST with a fragmented reply
        pub fragment_cert_list: bool,
        assembling: Vec<u8>,
        outbox: VecDeque<Vec<u8>>,
    }

    impl FakeDisplay {
        pub fn new(pin: &str) -> FakeDisplay {
            FakeDisplay {
                mtu: 185,
                pin: pin.into(),
                salt: [9; 16],
                authorized: false,
                poe_supported: false,
                eap_supported: true,
                dhcp_ip: [192, 168, 1, 77],
                dhcp_delay_polls: 0,
                received: Vec::new(),
                wifi: None,
                ip_config: None,
                network_type: None,
                silent: false,
                fragment_cert_list: false,
                assembling: Vec::new(),
                outbox: VecDeque::new(),
            }
        }

        pub fn commands(&self) -> Vec<u8> {
            self.received.iter().map(|(c, _)| *c).collect()
        }

        fn reply(&mut self, command: u8, status: u8, data: &[u8]) {
            let mut f = header(0, false, false, 1, 2 + data.len()).to_vec();
            f.push(status);
            f.push(command);
            f.extend_from_slice(data);
            self.outbox.push_back(f);
        }

        fn auth_reply(&mut self, command: u8, data: &[u8]) {
            let mut f = header(0, false, false, 7, 1 + data.len()).to_vec();
            f.push(command);
            f.extend_from_slice(data);
            self.outbox.push_back(f);
        }

        fn handle(&mut self, command: u8, data: Vec<u8>) {
            self.received.push((command, data.clone()));
            if self.silent {
                return;
            }
            let needs_auth = !proto::cmd::is_auth(command);
            if needs_auth && !self.authorized {
                return self.reply(command, 33, &[]);
            }
            match command {
                // Like the real display: the challenge comes back as QUERY_AUTH
                // and results as CONFIRM_AUTH
                cmd::START_AUTH if self.authorized => self.auth_reply(cmd::CONFIRM_AUTH, &[1]),
                cmd::START_AUTH => {
                    let mut d = vec![0b000_00100]; // version 2a, cost 4
                    d.extend_from_slice(&self.salt);
                    self.auth_reply(cmd::QUERY_AUTH, &d)
                }
                cmd::ANSWER_AUTH => {
                    let expected = proto::auth_digest(&self.pin, 4, &self.salt).unwrap();
                    self.authorized = data == expected;
                    let r = if self.authorized { 0 } else { 130 };
                    self.auth_reply(cmd::CONFIRM_AUTH, &[r])
                }
                cmd::DEVICE_STATUS => self.reply(command, 0, &[0b0011_0001]),
                cmd::GET_CA_CERT_LIST if self.eap_supported && self.fragment_cert_list => {
                    // A long reply split in two fragments
                    let mut first = header(0, false, true, 1, 7).to_vec();
                    first.extend_from_slice(&[0, 0, 0, 6, 0, command, b'c', b'a']);
                    self.outbox.push_back(first);
                    let mut last = header(0, true, false, 1, 6).to_vec();
                    last.extend_from_slice(&[0, 0, 0, 4, b'.', b'c']);
                    self.outbox.push_back(last);
                }
                cmd::POE_ACTIVATION => {
                    if self.poe_supported {
                        self.reply(command, 0, &[data.first().copied().unwrap_or(1)])
                    } else {
                        self.reply(command, 37, &[])
                    }
                }
                cmd::GET_CA_CERT_LIST => {
                    if self.eap_supported {
                        self.reply(command, 0, &[0])
                    } else {
                        self.reply(command, 37, &[])
                    }
                }
                cmd::NETWORK_TYPE => {
                    self.network_type = data.first().copied();
                    self.reply(command, 0, &[0])
                }
                cmd::WIFI if data.is_empty() => {
                    let ssid = self.wifi.as_ref().map(|w| w.0.clone()).unwrap_or_default();
                    let mut d = vec![ssid.len() as u8];
                    d.extend_from_slice(ssid.as_bytes());
                    self.reply(command, 0, &d)
                }
                cmd::WIFI => {
                    let len = usize::from(data[0] & 63);
                    let ssid = String::from_utf8_lossy(&data[1..1 + len]).into_owned();
                    let pw = if data[0] & 0x40 != 0 {
                        let pl = usize::from(data[1 + len]);
                        String::from_utf8_lossy(&data[2 + len..2 + len + pl]).into_owned()
                    } else {
                        String::new()
                    };
                    self.wifi = Some((ssid, pw));
                    self.reply(command, 0, &[])
                }
                cmd::NETWORK_ADDRESS if data.is_empty() => {
                    let ip = if self.ip_config.is_none() {
                        [0; 4]
                    } else if self.dhcp_delay_polls > 0 {
                        self.dhcp_delay_polls -= 1;
                        [0; 4]
                    } else {
                        self.dhcp_ip
                    };
                    let mut d = vec![0xC0 | 24];
                    d.extend_from_slice(&ip);
                    d.extend_from_slice(&[192, 168, 1, 1, 192, 168, 1, 1]);
                    self.reply(command, 0, &d)
                }
                cmd::NETWORK_ADDRESS => {
                    self.ip_config = Some(data);
                    self.reply(command, 0, &[])
                }
                _ => self.reply(command, 37, &[]),
            }
        }
    }

    impl Link for FakeDisplay {
        fn mtu(&self) -> usize {
            self.mtu
        }

        async fn write(&mut self, frame: &[u8]) -> Result<()> {
            let fragment = frame[0] >> 5 & 1 == 1;
            let more = frame[0] >> 4 & 1 == 1;
            if !fragment && !more {
                self.handle(frame[2], frame[3..].to_vec());
                return Ok(());
            }
            // Chunk: 4-byte total/offset, then payload
            if !fragment {
                self.assembling.clear();
            }
            self.assembling.extend_from_slice(&frame[6..]);
            if !more {
                let payload = std::mem::take(&mut self.assembling);
                self.handle(payload[0], payload[1..].to_vec());
            }
            Ok(())
        }

        async fn recv(&mut self) -> Result<Vec<u8>> {
            match self.outbox.pop_front() {
                Some(f) => Ok(f),
                None => std::future::pending().await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeDisplay;
    use super::*;

    fn session(d: FakeDisplay) -> BleSession<FakeDisplay> {
        let mut s = BleSession::new(d);
        s.reply_timeout = Duration::from_millis(300);
        s
    }

    #[tokio::test]
    async fn authorizes_with_the_right_pin() {
        let mut s = session(FakeDisplay::new("123456"));
        assert_eq!(s.authorize("123456").await.unwrap(), AuthResult::Success);
        assert!(s.device_status().await.is_ok());
        assert_eq!(
            s.into_link().commands(),
            [cmd::START_AUTH, cmd::ANSWER_AUTH, cmd::DEVICE_STATUS]
        );
    }

    #[tokio::test]
    async fn wrong_pin_is_reported_and_commands_are_refused() {
        let mut s = session(FakeDisplay::new("123456"));
        assert_eq!(s.authorize("000000").await.unwrap(), AuthResult::WrongPin);
        let err = s.device_status().await.unwrap_err().to_string();
        assert!(err.contains("rejected BLE command 16: error"), "{err}");
    }

    #[tokio::test]
    async fn silent_display_times_out() {
        let mut d = FakeDisplay::new("1");
        d.silent = true;
        let err = session(d).device_status().await.unwrap_err().to_string();
        assert!(err.contains("no reply from the display"), "{err}");
    }

    #[tokio::test]
    async fn wifi_setup_sequence_matches_the_app() {
        let mut s = session(FakeDisplay::new("123456"));
        s.authorize("123456").await.unwrap();
        let plan = NetworkPlan::Wifi {
            ssid: "Home Net".into(),
            password: "pa ss".into(),
        };
        configure_network(&mut s, &plan, Duration::ZERO)
            .await
            .unwrap();
        let addr = wait_for_address(&mut s, Duration::from_secs(2), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(addr.ip.to_string(), "192.168.1.77");
        let d = s.into_link();
        let cmds = d.commands();
        assert_eq!(
            &cmds[2..6],
            [
                cmd::POE_ACTIVATION,
                cmd::NETWORK_TYPE,
                cmd::WIFI,
                cmd::NETWORK_ADDRESS
            ],
            "PoE off, network type, credentials, IP — the app's order"
        );
        assert_eq!(d.received[2].1, vec![0], "PoE off for Wi-Fi");
        assert_eq!(d.network_type, Some(1));
        assert_eq!(d.wifi, Some(("Home Net".into(), "pa ss".into())));
        assert_eq!(d.ip_config.as_deref().map(|c| c[0]), Some(0xC0), "DHCP");
    }

    #[tokio::test]
    async fn long_credentials_are_chunked_at_small_mtu() {
        let mut d = FakeDisplay::new("123456");
        d.mtu = 23; // no MTU exchange
        let mut s = session(d);
        s.authorize("123456").await.unwrap(); // 23-byte digest → chunked too
        let pw = "p".repeat(64);
        s.set_wifi(&"s".repeat(32), &pw).await.unwrap();
        assert_eq!(s.into_link().wifi.map(|w| w.1), Some(pw));
    }

    #[tokio::test]
    async fn second_authorization_is_already_authorized() {
        let mut s = session(FakeDisplay::new("123456"));
        assert_eq!(s.authorize("123456").await.unwrap(), AuthResult::Success);
        assert_eq!(
            s.authorize("123456").await.unwrap(),
            AuthResult::AlreadyAuthorized
        );
    }

    #[tokio::test]
    async fn fragmented_replies_are_reassembled() {
        let mut d = FakeDisplay::new("123456");
        d.fragment_cert_list = true;
        let mut s = session(d);
        s.authorize("123456").await.unwrap();
        assert!(s.eap_supported().await.unwrap());
        assert!(
            s.device_status().await.unwrap().setup_done,
            "next reply still parses"
        );
    }

    #[tokio::test]
    async fn unsupported_features_are_not_errors() {
        let mut d = FakeDisplay::new("123456");
        d.eap_supported = false;
        let mut s = session(d);
        s.authorize("123456").await.unwrap();
        assert!(!s.poe_available().await.unwrap());
        assert!(!s.eap_supported().await.unwrap());
        s.set_poe(false).await.unwrap();
    }

    #[tokio::test]
    async fn waits_for_dhcp_then_gives_up() {
        let mut d = FakeDisplay::new("123456");
        d.dhcp_delay_polls = 2;
        let mut s = session(d);
        s.authorize("123456").await.unwrap();
        s.set_dhcp().await.unwrap();
        let addr = wait_for_address(&mut s, Duration::from_secs(5), Duration::from_millis(1))
            .await
            .unwrap();
        assert!(!addr.ip.is_unspecified());

        let mut s = session(FakeDisplay::new("123456"));
        s.authorize("123456").await.unwrap();
        let err = wait_for_address(&mut s, Duration::from_millis(20), Duration::from_millis(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not get a network address"));
    }
}
