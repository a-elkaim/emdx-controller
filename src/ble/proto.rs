//! The EMDX Bluetooth LE setup protocol (reverse engineered from the Samsung
//! ePaper Android app 1.3.3).
//!
//! A brand-new display (or one put in BLE mode over MDC) advertises Samsung
//! manufacturer data and exposes one GATT service. The phone writes framed
//! commands to a characteristic and reads replies from its notifications:
//!
//! ```text
//! request   [h0 h1] cmd data…          h0 = id:2 frag:1 more:1 op:3 len_hi:1
//! reply     [h0 h1] status cmd data…   status & 0x3F, 0 = success
//! auth reply[h0 h1] cmd data…          (no status byte, commands 1–4)
//! ```
//!
//! Payloads larger than the MTU are split into chunks prefixed with a 4-byte
//! big-endian total size (first chunk) or offset (later chunks).

use std::{fmt, net::Ipv4Addr};

use uuid::Uuid;

/// "SVCDeviceSetting"
pub const SERVICE_UUID: Uuid = Uuid::from_u128(0x53564344_6576_6963_6553_657474696E67);
/// "CMDDeviceSetting": write requests, receive notifications.
pub const COMMAND_UUID: Uuid = Uuid::from_u128(0x434D4444_6576_6963_6553_657474696E67);
/// "CMDDevicePairing": read; first byte 0 = the display has no phone info yet.
pub const PAIRING_UUID: Uuid = Uuid::from_u128(0x434D4444_6576_6963_6550_616972696E67);

/// Bluetooth SIG company id of Samsung Electronics.
pub const SAMSUNG_COMPANY_ID: u16 = 0x0075;
/// Manufacturer-data service byte of an e-paper display in setup mode.
pub const EPD_SERVICE_ID: u8 = 34;
/// Generic Samsung (SmartThings) beacon, also sent by TVs.
pub const ST_SERVICE_ID: u8 = 4;

/// Largest MTU the app uses.
pub const MAX_MTU: usize = 515;

pub mod cmd {
    pub const START_AUTH: u8 = 1;
    pub const CONFIRM_AUTH: u8 = 2;
    pub const QUERY_AUTH: u8 = 3;
    pub const ANSWER_AUTH: u8 = 4;
    pub const DEVICE_STATUS: u8 = 16;
    pub const NETWORK_TYPE: u8 = 17;
    pub const NETWORK_ADDRESS: u8 = 18;
    pub const NETWORK_ADDRESS_V6: u8 = 19;
    pub const WIFI: u8 = 20;
    pub const SCAN_ACCESS_POINTS: u8 = 21;
    pub const CLEAR_SAVED_ACCESS_POINTS: u8 = 22;
    pub const POE_ACTIVATION: u8 = 24;
    pub const WIRED_AVAILABLE: u8 = 25;
    pub const WRITE_CA_CERT: u8 = 32;
    pub const WRITE_CLIENT_CERT: u8 = 33;
    pub const WRITE_EAP_CONFIG: u8 = 34;
    pub const GET_CA_CERT_LIST: u8 = 35;

    pub fn is_auth(command: u8) -> bool {
        (START_AUTH..=ANSWER_AUTH).contains(&command)
    }
}

/// Operation types (3 bits of the header).
pub mod op {
    pub const REQUEST_WITH_RESPONSE: u8 = 0;
    pub const AUTHORIZATION: u8 = 7;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    TooLong(String),
    Malformed(String),
    Fragmented,
    Status { command: u8, status: u8 },
    Invalid(String),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::TooLong(m) | ProtoError::Malformed(m) | ProtoError::Invalid(m) => {
                f.write_str(m)
            }
            ProtoError::Fragmented => f.write_str("fragmented BLE responses are not supported"),
            ProtoError::Status { command, status } => {
                write!(
                    f,
                    "display rejected BLE command {command}: {}",
                    status_name(*status)
                )
            }
        }
    }
}

impl std::error::Error for ProtoError {}

pub type Result<T> = std::result::Result<T, ProtoError>;

pub fn status_name(status: u8) -> &'static str {
    match status {
        0 => "success",
        33 => "error",
        36 => "already exists",
        37 => "not supported",
        38 => "invalid data",
        39 => "API error",
        41 => "fragment error",
        63 => "command-defined error",
        _ => "unknown status",
    }
}

// ─── Framing ─────────────────────────────────────────────────────────────────

/// 2-byte PDU header.
pub fn header(id: u8, fragment: bool, more: bool, op: u8, len: usize) -> [u8; 2] {
    [
        (id & 3) << 6
            | u8::from(fragment) << 5
            | u8::from(more) << 4
            | (op & 7) << 1
            | ((len >> 8) & 1) as u8,
        (len & 0xFF) as u8,
    ]
}

/// Frame a command for writing; returns one write per element.
pub fn encode_request(command: u8, data: &[u8], op: u8, mtu: usize) -> Result<Vec<Vec<u8>>> {
    let mtu = mtu.min(MAX_MTU);
    if mtu < 12 {
        return Err(ProtoError::Invalid(format!("MTU {mtu} too small")));
    }
    if data.len() + 5 <= mtu - 5 {
        let mut frame = header(0, false, false, op, 1 + data.len()).to_vec();
        frame.push(command);
        frame.extend_from_slice(data);
        return Ok(vec![frame]);
    }
    // Chunked: payload = command + data, each chunk prefixed by size/offset
    let mut payload = vec![command];
    payload.extend_from_slice(data);
    let chunk = mtu - 3 - 2 - 4;
    let count = payload.len().div_ceil(chunk);
    let mut frames = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * chunk;
        let end = if i == count - 1 {
            payload.len()
        } else {
            start + chunk
        };
        let prefix = if i == 0 { payload.len() } else { start } as u32;
        let body_len = 4 + (end - start);
        if body_len > 0x1FF {
            return Err(ProtoError::TooLong(format!(
                "chunk of {body_len} bytes exceeds 511"
            )));
        }
        let mut frame = header(0, i > 0, i < count - 1, op, body_len).to_vec();
        frame.extend_from_slice(&prefix.to_be_bytes());
        frame.extend_from_slice(&payload[start..end]);
        frames.push(frame);
    }
    Ok(frames)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub command: u8,
    pub status: u8,
    pub data: Vec<u8>,
}

/// Decode one notification. Authorization replies (commands 1–4) carry no status byte.
pub fn decode_reply(frame: &[u8]) -> Result<Reply> {
    if frame.len() < 3 {
        return Err(ProtoError::Malformed(format!(
            "BLE reply too short ({} bytes)",
            frame.len()
        )));
    }
    let more = frame[0] >> 4 & 1 == 1;
    let fragment = frame[0] >> 5 & 1 == 1;
    if more || fragment {
        return Err(ProtoError::Fragmented);
    }
    let len = (usize::from(frame[0] & 1) << 8) | usize::from(frame[1]);
    if cmd::is_auth(frame[2]) {
        let end = (2 + len).min(frame.len());
        return Ok(Reply {
            command: frame[2],
            status: 0,
            data: frame[3..end.max(3)].to_vec(),
        });
    }
    if frame.len() < 4 {
        return Err(ProtoError::Malformed(
            "BLE reply missing command byte".into(),
        ));
    }
    Ok(Reply {
        command: frame[3],
        status: frame[2] & 0x3F,
        data: frame[4..].to_vec(),
    })
}

/// Whether `reply` answers `request`. The display answers START_AUTH with
/// QUERY_AUTH (the challenge) or CONFIRM_AUTH (a bare result), and
/// ANSWER_AUTH with CONFIRM_AUTH (the app's old BLE stack accepts exactly these).
pub fn replies_to(request: u8, reply: u8) -> bool {
    match request {
        cmd::START_AUTH => matches!(reply, cmd::START_AUTH | cmd::CONFIRM_AUTH | cmd::QUERY_AUTH),
        cmd::ANSWER_AUTH => matches!(reply, cmd::ANSWER_AUTH | cmd::CONFIRM_AUTH),
        _ => request == reply,
    }
}

/// Rebuilds fragmented replies. Each fragment is `[h0 h1][4-byte BE size or
/// offset][…]`, like request chunks; the first fragment continues with
/// `[status][cmd][data…]` and later ones with more data.
#[derive(Debug, Default)]
pub struct Reassembler {
    partial: Option<Reply>,
}

impl Reassembler {
    /// Feed one notification; returns a reply once it is complete.
    pub fn feed(&mut self, frame: &[u8]) -> Result<Option<Reply>> {
        if frame.len() < 2 {
            return Err(ProtoError::Malformed("BLE notification too short".into()));
        }
        let more = frame[0] >> 4 & 1 == 1;
        let fragment = frame[0] >> 5 & 1 == 1;
        if !more && !fragment {
            self.partial = None;
            return decode_reply(frame).map(Some);
        }
        if !fragment {
            if frame.len() < 8 {
                return Err(ProtoError::Malformed(
                    "first reply fragment too short".into(),
                ));
            }
            self.partial = Some(Reply {
                status: frame[6] & 0x3F,
                command: frame[7],
                data: frame[8..].to_vec(),
            });
            return Ok(None);
        }
        let Some(partial) = self.partial.as_mut() else {
            return Err(ProtoError::Malformed(
                "reply fragment without its first part".into(),
            ));
        };
        partial
            .data
            .extend_from_slice(frame.get(6..).unwrap_or_default());
        if more {
            Ok(None)
        } else {
            Ok(self.partial.take())
        }
    }
}

// ─── Authorization (bcrypt challenge) ────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthResult {
    Success,
    AlreadyAuthorized,
    Denied,
    InvalidSequence,
    WrongPin,
    Other(u8),
}

impl AuthResult {
    pub fn from_byte(b: u8) -> AuthResult {
        match b {
            0 => AuthResult::Success,
            1 => AuthResult::AlreadyAuthorized,
            128 => AuthResult::Denied,
            129 => AuthResult::InvalidSequence,
            130 => AuthResult::WrongPin,
            other => AuthResult::Other(other),
        }
    }

    pub fn is_ok(self) -> bool {
        matches!(self, AuthResult::Success | AuthResult::AlreadyAuthorized)
    }
}

impl fmt::Display for AuthResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthResult::Success => f.write_str("authorized"),
            AuthResult::AlreadyAuthorized => f.write_str("already authorized"),
            AuthResult::Denied => f.write_str("authorization denied by the display"),
            AuthResult::InvalidSequence => f.write_str("invalid authorization sequence"),
            AuthResult::WrongPin => f.write_str("wrong PIN"),
            AuthResult::Other(b) => write!(f, "unexpected authorization result {b}"),
        }
    }
}

/// The display's challenge (reply to START_AUTH).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    /// A bare result (e.g. already authorized).
    Result(AuthResult),
    /// bcrypt parameters to hash the PIN with.
    Bcrypt {
        version: u8,
        cost: u8,
        salt: Vec<u8>,
    },
}

pub fn parse_challenge(data: &[u8]) -> Result<Challenge> {
    match data {
        [] => Err(ProtoError::Malformed(
            "empty authorization challenge".into(),
        )),
        [b] => Ok(Challenge::Result(AuthResult::from_byte(*b))),
        [b, salt @ ..] => Ok(Challenge::Bcrypt {
            version: b >> 5 & 7,
            cost: b & 31,
            salt: salt.to_vec(),
        }),
    }
}

const BCRYPT_ALPHABET: &[u8; 64] =
    b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// bcrypt's base64 variant (the app's custom decoder).
pub fn bcrypt_b64_decode(s: &str) -> Result<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for ch in s.bytes() {
        if ch == b'=' {
            break;
        }
        let v = BCRYPT_ALPHABET
            .iter()
            .position(|&c| c == ch)
            .ok_or_else(|| {
                ProtoError::Invalid(format!("invalid bcrypt base64 character {:?}", ch as char))
            })?;
        acc = (acc << 6 | v as u32) & 0xFFFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out.truncate(s.len() * 3 / 4);
    Ok(out)
}

/// Answer to a bcrypt challenge: the 23-byte hash part of bcrypt(PIN).
pub fn auth_digest(pin: &str, cost: u8, salt: &[u8]) -> Result<Vec<u8>> {
    let salt: [u8; 16] = salt.try_into().map_err(|_| {
        ProtoError::Malformed(format!("bcrypt salt must be 16 bytes, got {}", salt.len()))
    })?;
    let parts = bcrypt::hash_with_salt(pin.as_bytes(), u32::from(cost), salt)
        .map_err(|e| ProtoError::Invalid(format!("bcrypt: {e}")))?;
    // The version prefix ($2a/$2b/$2y) doesn't change the hash for ASCII PINs;
    // the display only checks the last 31 characters.
    let full = parts.format_for_version(bcrypt::Version::TwoA);
    bcrypt_b64_decode(&full[full.len() - 31..])
}

// ─── Command payloads ────────────────────────────────────────────────────────

/// DEVICE_STATUS reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct DeviceStatus {
    pub network_type_bit: bool,
    /// Bit 6. The app names it `disconnected` but never reads it; its runtime
    /// meaning is unconfirmed, so don't base decisions on it.
    pub disconnected: bool,
    pub gateway: bool,
    pub internet: bool,
    pub wifi_active: bool,
    pub setup_done: bool,
}

pub fn parse_device_status(data: &[u8]) -> Result<DeviceStatus> {
    let [b] = data else {
        return Err(ProtoError::Malformed(format!(
            "device status must be 1 byte, got {}",
            data.len()
        )));
    };
    let bit = |n: u8| b >> n & 1 == 1;
    Ok(DeviceStatus {
        network_type_bit: bit(7),
        disconnected: bit(6),
        gateway: bit(5),
        internet: bit(4),
        wifi_active: bit(3),
        setup_done: bit(0),
    })
}

/// NETWORK_TYPE set data for Wi-Fi (the only network setup emdx supports).
pub const NETWORK_TYPE_WIFI: [u8; 1] = [1];

/// WIFI set: `[has_pw<<6 | ssid_len] ssid [pw_len pw]`.
pub fn wifi_data(ssid: &str, password: &str) -> Result<Vec<u8>> {
    let (s, p) = (ssid.as_bytes(), password.as_bytes());
    if s.is_empty() || s.len() > 32 {
        return Err(ProtoError::Invalid(format!(
            "SSID must be 1-32 bytes, got {}",
            s.len()
        )));
    }
    if p.len() > 64 {
        return Err(ProtoError::Invalid(format!(
            "Wi-Fi password must be at most 64 bytes, got {}",
            p.len()
        )));
    }
    let mut out = vec![if p.is_empty() { 0 } else { 0x40 } | s.len() as u8];
    out.extend_from_slice(s);
    if !p.is_empty() {
        out.push(p.len() as u8);
        out.extend_from_slice(p);
    }
    Ok(out)
}

/// WIFI get: the SSID the display is configured for.
pub fn parse_wifi_ssid(data: &[u8]) -> Result<String> {
    let Some(&first) = data.first() else {
        return Ok(String::new());
    };
    let len = usize::from(first & 63).min(data.len() - 1);
    Ok(String::from_utf8_lossy(&data[1..1 + len]).into_owned())
}

/// NETWORK_ADDRESS set data for an automatic address (DHCP): `C0` + 12 zeros.
pub fn dhcp_config_data() -> Vec<u8> {
    let mut v = vec![0xC0];
    v.extend_from_slice(&[0; 12]);
    v
}

/// NETWORK_ADDRESS get reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct NetworkAddress {
    pub dhcp: bool,
    pub auto_dns: bool,
    pub prefix: u8,
    pub ip: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub dns: Ipv4Addr,
}

pub fn parse_network_address(data: &[u8]) -> Result<NetworkAddress> {
    if data.len() < 13 {
        return Err(ProtoError::Malformed(format!(
            "network address reply too short ({} bytes)",
            data.len()
        )));
    }
    let ip = |i: usize| Ipv4Addr::new(data[i], data[i + 1], data[i + 2], data[i + 3]);
    Ok(NetworkAddress {
        dhcp: data[0] & 0x80 != 0,
        auto_dns: data[0] & 0x40 != 0,
        prefix: data[0] & 63,
        ip: ip(1),
        gateway: ip(5),
        dns: ip(9),
    })
}

// ─── Discovery data ──────────────────────────────────────────────────────────

/// What a display's advertisement says (manufacturer data, company 0x0075).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Advert {
    /// Advertising in the e-paper setup format (connectable for setup).
    pub setup_mode: bool,
    /// Set up before (Samsung's "isOOBEBefore").
    pub configured: bool,
    /// Older BLE firmware variant.
    pub old_ble: bool,
}

/// Parse Samsung manufacturer data (without the company id). `None` when
/// it isn't an e-paper display advert.
pub fn parse_advert(data: &[u8]) -> Option<Advert> {
    if data.len() <= 5 {
        return None;
    }
    match data[1] {
        EPD_SERVICE_ID => Some(Advert {
            setup_mode: true,
            configured: data[2] & 1 == 0,
            old_ble: data[2] >> 1 & 1 == 0,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_spell_their_names() {
        assert_eq!(
            SERVICE_UUID.to_string(),
            "53564344-6576-6963-6553-657474696e67"
        );
        assert_eq!(&SERVICE_UUID.as_bytes()[..], b"SVCDeviceSetting");
        assert_eq!(&COMMAND_UUID.as_bytes()[..], b"CMDDeviceSetting");
        assert_eq!(&PAIRING_UUID.as_bytes()[..], b"CMDDevicePairing");
    }

    #[test]
    fn header_bits() {
        assert_eq!(header(0, false, false, 0, 5), [0x00, 0x05]);
        assert_eq!(header(0, false, false, op::AUTHORIZATION, 1), [0x0E, 0x01]);
        assert_eq!(header(1, true, true, 0, 0x1FF), [0x71, 0xFF]);
    }

    #[test]
    fn single_frame_request() {
        let frames =
            encode_request(cmd::DEVICE_STATUS, &[], op::REQUEST_WITH_RESPONSE, 185).unwrap();
        assert_eq!(frames, vec![vec![0x00, 0x01, 16]]);
        let frames = encode_request(cmd::START_AUTH, &[], op::AUTHORIZATION, 185).unwrap();
        assert_eq!(frames, vec![vec![0x0E, 0x01, 1]]);
    }

    #[test]
    fn chunked_request_matches_app_layout() {
        // MTU 23 (no MTU exchange): chunks of 23-9 = 14 bytes of payload
        let data: Vec<u8> = (0..30).collect();
        let frames = encode_request(cmd::WIFI, &data, 0, 23).unwrap();
        assert_eq!(frames.len(), 3); // 31 bytes of payload
        assert_eq!(&frames[0][..2], &header(0, false, true, 0, 18));
        assert_eq!(
            &frames[0][2..6],
            &31u32.to_be_bytes(),
            "first chunk carries the total size"
        );
        assert_eq!(frames[0][6], cmd::WIFI);
        assert_eq!(&frames[1][..2], &header(0, true, true, 0, 18));
        assert_eq!(
            &frames[1][2..6],
            &14u32.to_be_bytes(),
            "later chunks carry their offset"
        );
        assert_eq!(
            &frames[2][..2],
            &header(0, true, false, 0, 7),
            "last chunk: more=0"
        );
        let rebuilt: Vec<u8> = frames.iter().flat_map(|f| f[6..].to_vec()).collect();
        assert_eq!(rebuilt[0], cmd::WIFI);
        assert_eq!(&rebuilt[1..], &data[..]);
        assert!(frames.iter().all(|f| f.len() <= 23 - 3));
    }

    #[test]
    fn decodes_replies() {
        let r = decode_reply(&[0x00, 0x03, 0x00, 16, 0b0110_1001]).unwrap();
        assert_eq!(
            (r.command, r.status, r.data.clone()),
            (16, 0, vec![0b0110_1001])
        );
        let r = decode_reply(&[0x00, 0x02, 0x25, 24]).unwrap();
        assert_eq!(r.status, 37);
        // Authorization replies have no status byte
        let r = decode_reply(&[0x0E, 0x02, cmd::ANSWER_AUTH, 0x00]).unwrap();
        assert_eq!((r.command, r.data), (cmd::ANSWER_AUTH, vec![0]));
        assert_eq!(
            decode_reply(&[0x10, 0x05, 0, 0, 0]),
            Err(ProtoError::Fragmented)
        );
        assert!(decode_reply(&[0x00]).is_err());
    }

    #[test]
    fn auth_reply_matching() {
        assert!(replies_to(cmd::START_AUTH, cmd::QUERY_AUTH));
        assert!(replies_to(cmd::START_AUTH, cmd::CONFIRM_AUTH));
        assert!(replies_to(cmd::ANSWER_AUTH, cmd::CONFIRM_AUTH));
        assert!(!replies_to(cmd::ANSWER_AUTH, cmd::QUERY_AUTH));
        assert!(replies_to(cmd::WIFI, cmd::WIFI));
        assert!(!replies_to(cmd::WIFI, cmd::NETWORK_ADDRESS));
    }

    #[test]
    fn reassembles_fragmented_replies() {
        let mut r = Reassembler::default();
        // first: [h0 h1][total 4][status][cmd][data…]
        let mut first = header(0, false, true, 1, 9).to_vec();
        first.extend_from_slice(&[0, 0, 0, 7, 0x00, cmd::GET_CA_CERT_LIST, b'a']);
        assert_eq!(r.feed(&first).unwrap(), None);
        let mut middle = header(0, true, true, 1, 6).to_vec();
        middle.extend_from_slice(&[0, 0, 0, 3, b'b', b'c']);
        assert_eq!(r.feed(&middle).unwrap(), None);
        let mut last = header(0, true, false, 1, 5).to_vec();
        last.extend_from_slice(&[0, 0, 0, 5, b'd']);
        let reply = r.feed(&last).unwrap().unwrap();
        assert_eq!(
            (reply.command, reply.status, reply.data),
            (cmd::GET_CA_CERT_LIST, 0, b"abcd".to_vec())
        );
        // Plain replies pass straight through
        assert_eq!(
            r.feed(&[0x00, 0x03, 0x00, 16, 1]).unwrap().unwrap().data,
            vec![1]
        );
        // A continuation without its first part is an error
        assert!(Reassembler::default().feed(&last).is_err());
    }

    #[test]
    fn parses_challenges() {
        assert_eq!(
            parse_challenge(&[1]).unwrap(),
            Challenge::Result(AuthResult::AlreadyAuthorized)
        );
        let mut data = vec![0b000_01010];
        data.extend_from_slice(&[7; 16]);
        assert_eq!(
            parse_challenge(&data).unwrap(),
            Challenge::Bcrypt {
                version: 0,
                cost: 10,
                salt: vec![7; 16]
            }
        );
        assert!(parse_challenge(&[]).is_err());
        assert_eq!(AuthResult::from_byte(130), AuthResult::WrongPin);
        assert!(AuthResult::from_byte(1).is_ok());
        assert!(!AuthResult::Denied.is_ok());
    }

    #[test]
    fn bcrypt_base64() {
        // 22 salt characters → 16 bytes (cross-checked with Python's bcrypt)
        assert_eq!(
            bcrypt_b64_decode("zYVLbkY4qqsxLpFVEkHH4e").unwrap(),
            hex("d5a5cd7666bab2cbb336b1d71a6249ea")
        );
        assert!(bcrypt_b64_decode("bad!").is_err());
    }

    #[test]
    fn digest_matches_python_bcrypt() {
        // python: bcrypt.hashpw(b"123456", b"$2a$04$zYVLbkY4qqsxLpFVEkHH4e")
        //   = $2a$04$zYVLbkY4qqsxLpFVEkHH4e.qndmwy6v2gxI5zZZOrPggxSnK/bC9.
        let salt = hex("d5a5cd7666bab2cbb336b1d71a6249ea");
        let digest = auth_digest("123456", 4, &salt).unwrap();
        assert_eq!(
            digest,
            hex("02ca5fa32d3cc788b32bbd5b6d0b518a2cd4a4c05d13f0")
        );
        assert_eq!(digest.len(), 23);
        assert!(auth_digest("123456", 4, &[0; 15]).is_err());
    }

    #[test]
    fn digest_accepted_by_a_real_display() {
        // Logged by the ePaper app 1.3.3 during a real setup (EM32DX, 2026-11-10):
        // START_AUTH reply version 3, cost 4; the display answered ANSWER_AUTH with success
        let salt = hex("1559479fcf7336f4df9da5af1d408af3");
        assert_eq!(
            auth_digest("852140", 4, &salt).unwrap(),
            hex("132ead207916f9dab82fe2ae8ad3f94035df84949b6f54")
        );
        // The app sent this digest as ANSWER_AUTH in two chunks at the default MTU (23)
        let digest = hex("132ead207916f9dab82fe2ae8ad3f94035df84949b6f54");
        let frames = encode_request(cmd::ANSWER_AUTH, &digest, op::AUTHORIZATION, 23).unwrap();
        assert_eq!(
            frames,
            vec![
                // header, total size 24, ANSWER_AUTH (04), first 13 digest bytes
                hex("1e120000001804132ead207916f9dab82fe2ae8a"),
                hex("2e0e0000000ed3f94035df84949b6f54"),
            ]
        );
        assert_eq!(
            encode_request(cmd::START_AUTH, &[], op::AUTHORIZATION, 23).unwrap(),
            vec![hex("0e0101")]
        );
        // Same session, first attempt (cost 5)
        let salt = hex("183d9905748031bc95dfb7d530aeb69c");
        assert_eq!(
            auth_digest("852140", 5, &salt).unwrap()[..7],
            hex("955720f060241e")[..]
        );
    }

    #[test]
    fn device_status_bits() {
        let s = parse_device_status(&[0b0111_1001]).unwrap();
        assert!(s.disconnected && s.gateway && s.internet && s.wifi_active && s.setup_done);
        assert!(!s.network_type_bit);
        assert!(parse_device_status(&[]).is_err());
    }

    #[test]
    fn wifi_payload() {
        assert_eq!(
            wifi_data("home", "secret").unwrap(),
            [&[0x44][..], b"home", &[6], b"secret"].concat()
        );
        assert_eq!(
            wifi_data("open", "").unwrap(),
            [&[0x04][..], b"open"].concat()
        );
        assert!(wifi_data("", "x").is_err());
        assert!(wifi_data(&"s".repeat(33), "").is_err());
        assert!(wifi_data("s", &"p".repeat(65)).is_err());
        assert_eq!(
            parse_wifi_ssid(&[0x44, b'h', b'o', b'm', b'e']).unwrap(),
            "home"
        );
        assert_eq!(parse_wifi_ssid(&[]).unwrap(), "");
    }

    #[test]
    fn ip_payloads() {
        assert_eq!(dhcp_config_data(), [&[0xC0][..], &[0; 12]].concat());
        let a = parse_network_address(&[0xC0 | 24, 192, 168, 1, 34, 192, 168, 1, 1, 8, 8, 8, 8])
            .unwrap();
        assert!(a.dhcp && a.auto_dns);
        assert_eq!((a.prefix, a.ip.to_string()), (24, "192.168.1.34".into()));
        assert!(parse_network_address(&[0; 5]).is_err());
    }

    #[test]
    fn adverts() {
        // Setup-mode display, never set up, new BLE stack
        let a = parse_advert(&[0x42, 34, 0b11, 0, 0, 0]).unwrap();
        assert!(a.setup_mode && !a.configured && !a.old_ble);
        // The real display already set up advertises the generic 0x04 beacon
        let live = hex("420401016 1b0f2f6550d0cb2f2f6550d0b09000000000000"
            .replace(' ', "")
            .as_str());
        assert_eq!(parse_advert(&live), None);
        assert_eq!(parse_advert(&[0x42, 34]), None);
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
