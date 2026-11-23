//! Samsung MDC (Multiple Display Control) protocol for EMDX e-paper displays.
//!
//! Session flow:
//! 1. TCP connect to port 1515; the display greets with `MDCSTART<<TLS>>`
//! 2. TLS handshake on the same socket (self-signed certificate)
//! 3. Send the PIN; the display answers `MDCAUTH<<PASS>>` or `MDCAUTH<<FAIL:0x0N>>`
//! 4. Exchange framed commands:
//!    request  `AA cmd id len data… checksum`
//!    response `AA FF id len ack cmd payload… checksum` (`len` counts ack+cmd+payload)
//!
//! The checksum is the byte sum (mod 256) of everything after the `AA` header.

use std::{fmt, sync::Arc, sync::OnceLock, time::Duration};

use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider},
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, SignatureScheme,
};
use serde::Serialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{client::TlsStream, TlsConnector};

pub const HEADER: u8 = 0xAA;
pub const RESPONSE: u8 = 0xFF;
pub const ACK: u8 = 0x41; // 'A'
pub const NAK: u8 = 0x4E; // 'N'
pub const DEFAULT_PORT: u16 = 1515;
pub const BANNER: &[u8] = b"MDCSTART<<TLS>>";

pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(8);

/// MDC command ids used by EMDX displays.
pub mod cmd {
    pub const SERIAL_NUMBER: u8 = 0x0B;
    pub const SOFTWARE_VERSION: u8 = 0x0E;
    pub const MODEL_INFO: u8 = 0x10;
    pub const POWER: u8 = 0x11;
    pub const BATTERY: u8 = 0x1B;
    pub const ON_TIMER: u8 = 0x25;
    pub const DEVICE_NAME: u8 = 0x67;
    pub const CLOCK: u8 = 0xA7;
    pub const NETWORK_STANDBY: u8 = 0xB5;
    /// Auto-sleep delay (sub-command [`sub::SLEEP_TIME`]).
    pub const SLEEP_TIME: u8 = 0xC6;
    pub const CONTENT_DOWNLOAD: u8 = 0xC7;
    /// Capability flags (no sub-command).
    pub const SUPPORT_FEATURE: u8 = 0xCB;
    /// Reset (no sub-command): `[02]` settings, `[03]` everything (factory reset).
    pub const RESET_CONTROL: u8 = 0x9F;
    /// Screen orientation (sub-command [`sub::ORIENTATION`]).
    pub const ORIENTATION: u8 = 0xC8;
    /// Bluetooth control (sub-command [`sub::TURN_ON_BLE`]).
    pub const BLUETOOTH: u8 = 0x22;
    /// Screen settings, by sub-command: background colour ([`sub::BG_COLOR`]) and
    /// brightness (`0x4F`, not used by emdx).
    pub const SCREEN: u8 = 0x21;
    /// Terms & conditions (sub-command [`sub::TERMS`], large frames).
    pub const TERMS: u8 = 0xD2;
}

/// Sub-commands (first data byte), from the ePaper app's MDCConstant table.
pub mod sub {
    /// under `cmd::BATTERY` (0x1B)
    pub const BATTERY: u8 = 0x73;
    pub const WIFI_MAC: u8 = 0x81;
    pub const PIN: u8 = 0x87;
    pub const SETUP_STATUS: u8 = 0xB1;
    pub const DAILY_REFRESH: u8 = 0xB0;
    pub const EMPTY_STORAGE: u8 = 0x74;
    pub const RESOLUTION: u8 = 0x13;
    /// under `cmd::SLEEP_TIME` (0xC6)
    pub const SLEEP_TIME: u8 = 0x81;
    /// under `cmd::TERMS` (0xD2), large frames: identity TLV ("contact Samsung")
    pub const CONTACT_INFO: u8 = 0x00;
    /// under `cmd::BLUETOOTH` (0x22)
    pub const TURN_ON_BLE: u8 = 0x10;
    /// under `cmd::TERMS` (0xD2)
    pub const TERMS: u8 = 0x70;
    /// under `cmd::CONTENT_DOWNLOAD` (0xC7): `53 80 len url…`, the display then
    /// GETs that manifest URL
    pub const CONTENT_DOWNLOAD: u8 = 0x53;
    /// under `cmd::CONTENT_DOWNLOAD` (0xC7): the payload is a callback URL and the
    /// display later `POST`s a multipart thumbnail to it
    pub const GET_THUMBNAIL: u8 = 0x54;
    /// under `cmd::TERMS` (0xD2): push the Mobile CMS certificate to the display
    pub const DOWNLOAD_FILE_CERT: u8 = 0x20;
    /// under `cmd::CONTENT_DOWNLOAD` (0xC7)
    pub const CONTENT_PLAYER: u8 = 0x81;
    /// under `cmd::CONTENT_DOWNLOAD` (0xC7): sleep as soon as new content is shown
    /// (`MDCContentDownloadCommand.Set/GetImmediateSleepMode`). Set `[01, on]`,
    /// the reply is `[01, on]`.
    pub const IMMEDIATE_SLEEP: u8 = 0x53;
    /// under `cmd::ORIENTATION` (0xC8): `[0]` landscape, `[1]` portrait
    pub const ORIENTATION: u8 = 0x81;
    /// under `cmd::SCREEN` (0x21): `[00, colour]`, see [`bg_color`]
    pub const BG_COLOR: u8 = 0x90;
}

/// Values of the content player setting (`0xC7/0x81`), from the ePaper app.
pub mod content_player {
    pub const CUSTOM_APP: u8 = 1;
    pub const USB: u8 = 3;
    /// Plays what the app (or `emdx show-image`) sends. The app's default.
    pub const MOBILE_CMS: u8 = 4;
    pub const VXT: u8 = 5;

    pub fn name(value: u8) -> Option<&'static str> {
        Some(match value {
            CUSTOM_APP => "custom app",
            USB => "USB",
            MOBILE_CMS => "mobile (MobileCMS)",
            VXT => "VXT",
            _ => return None,
        })
    }
}

/// The background colours the ePaper app can set (`0x21/0x90`), in the app's order.
///
/// The byte sent is the app's enum value. The colours the app shows for them on screen
/// (white, `#121212`, `#eb3333`, `#2e9d58`, `#1d8cc8`, `#ffd700`) are only its own
/// swatches: what the panel draws for each index is its own business.
pub mod bg_color {
    pub const WHITE: u8 = 0;
    pub const BLACK: u8 = 1;
    pub const RED: u8 = 2;
    pub const GREEN: u8 = 3;
    pub const BLUE: u8 = 4;
    pub const YELLOW: u8 = 5;

    /// Every colour the app sets, as `(byte, name)`.
    pub const ALL: &[(u8, &str)] = &[
        (WHITE, "white"),
        (BLACK, "black"),
        (RED, "red"),
        (GREEN, "green"),
        (BLUE, "blue"),
        (YELLOW, "yellow"),
    ];

    pub fn name(value: u8) -> Option<&'static str> {
        ALL.iter().find(|(v, _)| *v == value).map(|(_, n)| *n)
    }

    /// A colour name (`white`, `black`, `red`, `green`, `blue`, `yellow`) → the byte to
    /// send. Unknown names, and the values the app itself has no entry for, are refused.
    pub fn parse(value: &str) -> Option<u8> {
        ALL.iter()
            .find(|(_, n)| *n == value.trim().to_ascii_lowercase())
            .map(|(v, _)| *v)
    }
}

/// On Timer repeat codes (MDC 0x25).
pub mod repeat {
    pub const ONCE: u8 = 0x01;
    pub const DAILY: u8 = 0x02;
    pub const MON_FRI: u8 = 0x03;
    pub const MON_SAT: u8 = 0x04;
    pub const SAT_SUN: u8 = 0x05;
}

#[derive(Debug)]
pub enum MdcError {
    Io(std::io::Error),
    Tls(String),
    Timeout(String),
    Closed,
    AuthFailed(&'static str),
    Protocol(String),
    ChecksumMismatch,
    Nak { command: u8, payload: Vec<u8> },
    InvalidArgument(String),
}

impl fmt::Display for MdcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MdcError::Io(e) => write!(f, "{e}"),
            MdcError::Tls(e) => write!(f, "TLS error: {e}"),
            MdcError::Timeout(msg) => write!(f, "{msg}"),
            MdcError::Closed => write!(f, "Closed"),
            MdcError::AuthFailed(why) => write!(f, "Authentication Failed: {why}"),
            MdcError::Protocol(msg) => write!(f, "Protocol error: {msg}"),
            MdcError::ChecksumMismatch => write!(f, "Checksum Mismatch"),
            MdcError::Nak { command, .. } => write!(f, "NAK for command 0x{command:02X}"),
            MdcError::InvalidArgument(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for MdcError {}

impl From<std::io::Error> for MdcError {
    fn from(e: std::io::Error) -> Self {
        MdcError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, MdcError>;

// ─── Framing (pure, unit-tested) ─────────────────────────────────────────────

/// Byte sum modulo 256.
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b))
}

/// Encode a request frame: `AA cmd id len data… checksum`.
pub fn encode_command(command: u8, display_id: u8, data: &[u8]) -> Result<Vec<u8>> {
    let len = u8::try_from(data.len()).map_err(|_| {
        MdcError::InvalidArgument(format!(
            "MDC payload too long ({} bytes, max 255)",
            data.len()
        ))
    })?;
    let mut frame = Vec::with_capacity(data.len() + 5);
    frame.extend_from_slice(&[HEADER, command, display_id, len]);
    frame.extend_from_slice(data);
    frame.push(checksum(&frame[1..]));
    Ok(frame)
}

/// Encode a "large" request frame with a 2-byte length:
/// `AA cmd id len_hi len_lo data… checksum`.
pub fn encode_command_large(command: u8, display_id: u8, data: &[u8]) -> Result<Vec<u8>> {
    let len = u16::try_from(data.len()).map_err(|_| {
        MdcError::InvalidArgument(format!("MDC payload too long ({} bytes)", data.len()))
    })?;
    let mut frame = Vec::with_capacity(data.len() + 6);
    frame.extend_from_slice(&[HEADER, command, display_id]);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(data);
    frame.push(checksum(&frame[1..]));
    Ok(frame)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub display_id: u8,
    pub ack: bool,
    pub command: u8,
    pub payload: Vec<u8>,
}

/// Encode a response frame: `AA FF id len ack cmd payload… checksum`
/// (what a display sends back; used by simulators and tests).
pub fn encode_response(resp: &Response) -> Result<Vec<u8>> {
    let len = u8::try_from(resp.payload.len() + 2).map_err(|_| {
        MdcError::InvalidArgument(format!(
            "MDC response payload too long ({} bytes, max 253)",
            resp.payload.len()
        ))
    })?;
    let ack = if resp.ack { ACK } else { NAK };
    let mut frame = Vec::with_capacity(resp.payload.len() + 7);
    frame.extend_from_slice(&[HEADER, RESPONSE, resp.display_id, len, ack, resp.command]);
    frame.extend_from_slice(&resp.payload);
    frame.push(checksum(&frame[1..]));
    Ok(frame)
}

/// Result of trying to decode a response frame from the front of a buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    /// More bytes are needed.
    Incomplete,
    /// The first `n` bytes are not a response frame; drop them.
    Skip(usize),
    /// A complete frame of `n` bytes whose checksum does not match.
    BadChecksum(usize),
    /// A complete, valid frame of `n` bytes.
    Frame(Response, usize),
}

/// Decode a response frame (`AA FF id len ack cmd payload… checksum`).
pub fn decode_response(buf: &[u8]) -> Decoded {
    decode_response_with(buf, false)
}

/// Decode a response; `large` frames carry a 2-byte length
/// (`AA FF id len_hi len_lo ack cmd payload… checksum`).
pub fn decode_response_with(buf: &[u8], large: bool) -> Decoded {
    let Some(&first) = buf.first() else {
        return Decoded::Incomplete;
    };
    if first != HEADER {
        let skip = buf.iter().position(|&b| b == HEADER).unwrap_or(buf.len());
        return Decoded::Skip(skip);
    }
    let head = if large { 5 } else { 4 };
    if buf.len() < head {
        return Decoded::Incomplete;
    }
    if buf[1] != RESPONSE {
        return Decoded::Skip(1);
    }
    let len = if large {
        usize::from(buf[3]) << 8 | usize::from(buf[4])
    } else {
        usize::from(buf[3])
    };
    if len < 2 {
        // A response must at least carry ack + command
        return Decoded::Skip(1);
    }
    let total = head + len + 1;
    if buf.len() < total {
        return Decoded::Incomplete;
    }
    if checksum(&buf[1..total - 1]) != buf[total - 1] {
        return Decoded::BadChecksum(total);
    }
    let ack = match buf[head] {
        ACK => true,
        NAK => false,
        _ => return Decoded::Skip(total),
    };
    Decoded::Frame(
        Response {
            display_id: buf[2],
            ack,
            command: buf[head + 1],
            payload: buf[head + 2..head + len].to_vec(),
        },
        total,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResult {
    Pass,
    WrongPin,
    Blocked,
    Other(String),
}

/// Find an `MDCAUTH<<…>>` reply in `buf`; returns it and the bytes consumed.
pub fn parse_auth(buf: &[u8]) -> Option<(AuthResult, usize)> {
    const PREFIX: &[u8] = b"MDCAUTH<<";
    let start = find(buf, PREFIX)?;
    let body_start = start + PREFIX.len();
    let end_rel = find(&buf[body_start..], b">>")?;
    let body = &buf[body_start..body_start + end_rel];
    let consumed = body_start + end_rel + 2;
    let result = match body {
        b"PASS" => AuthResult::Pass,
        b"FAIL:0x01" => AuthResult::WrongPin,
        b"FAIL:0x02" => AuthResult::Blocked,
        other => AuthResult::Other(String::from_utf8_lossy(other).into_owned()),
    };
    Some((result, consumed))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Payload for MDC 0xC7 (content download): `53 80 len url…`.
pub fn content_download_data(url: &str) -> Result<Vec<u8>> {
    if url.is_empty() {
        return Err(MdcError::InvalidArgument("Missing URL".into()));
    }
    // 3 header bytes + url must fit the 1-byte frame length
    if url.len() > 252 {
        return Err(MdcError::InvalidArgument(format!(
            "URL too long ({} chars), there is a maximum of 252 characters",
            url.len()
        )));
    }
    let mut data = vec![0x53, 0x80, url.len() as u8];
    data.extend_from_slice(url.as_bytes());
    Ok(data)
}

/// Payload for MDC 0xC7/0x54 (`MDC_COMMAND_GET_THUMBNAIL`): the callback URL,
/// sent as the frame's data with `54` as its sub-command byte.
///
/// Unlike `53`, this sub-command has no inner `80 len` framing — the app builds
/// it with `createPacket buildSubCmd` (`MDCThumbnailImageCommand$Set`).
pub fn thumbnail_data(url: &str) -> Result<Vec<u8>> {
    if url.is_empty() {
        return Err(MdcError::InvalidArgument("Missing URL".into()));
    }
    // 1 sub-command byte + url must fit the 1-byte frame length
    if url.len() > 254 {
        return Err(MdcError::InvalidArgument(format!(
            "URL too long ({} chars), there is a maximum of 254 characters",
            url.len()
        )));
    }
    let mut data = vec![sub::GET_THUMBNAIL];
    data.extend_from_slice(url.as_bytes());
    Ok(data)
}

/// `CertificateType.CMS_CERT_SERVER` in the ePaper app: the phone's own HTTPS
/// server certificate, which the display trusts when it uploads a thumbnail.
pub const CERT_TYPE_CMS_SERVER: u16 = 6;
/// The file name the app gives that certificate.
pub const CMS_CERT_FILE_NAME: &str = "CMSServer.crt";

/// Payload for MDC 0xD2/0x20 (`MDC_COMMAND_DOWNLOAD_FILE_CERT`), sent in a large frame:
/// `20 <download mode 00> <type u16 BE> <name len> name <data len u16 BE> pem`.
///
/// Byte for byte what `MDCDownloadFileCert$Set.l()` builds for
/// `MDCDownloadFileCert.Companion.a()` (`setCMSServerCert`).
pub fn cms_cert_data(pem: &str) -> Result<Vec<u8>> {
    let name = CMS_CERT_FILE_NAME.as_bytes();
    let len = u16::try_from(pem.len())
        .map_err(|_| MdcError::InvalidArgument("certificate too large".into()))?;
    let mut data = vec![sub::DOWNLOAD_FILE_CERT, 0x00];
    data.extend_from_slice(&CERT_TYPE_CMS_SERVER.to_be_bytes());
    data.push(name.len() as u8);
    data.extend_from_slice(name);
    data.extend_from_slice(&len.to_be_bytes());
    data.extend_from_slice(pem.as_bytes());
    Ok(data)
}

/// The URL in a `0xC7/0x54` reply `54 <len> <url>` (seen on an EM32DX), or the
/// whole payload when it does not have that shape.
pub fn thumbnail_echo(payload: &[u8]) -> &[u8] {
    match payload {
        [sub::GET_THUMBNAIL, len, rest @ ..] if usize::from(*len) == rest.len() => rest,
        _ => payload,
    }
}

/// Payload for MDC 0x25 (On Timer).
pub fn on_timer_data(enabled: bool, repeat: u8, hour: u8, minute: u8) -> [u8; 5] {
    [u8::from(enabled), repeat, hour, minute, 0x01]
}

pub fn parse_power(payload: &[u8]) -> Option<&'static str> {
    match payload.first()? {
        0x00 => Some("Off"),
        0x01 => Some("On"),
        0x02 => Some("Reboot"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatteryState {
    pub charging: bool,
    pub present: bool,
    pub healthy: bool,
    pub level: u8,
    /// What powers it, when the display reports it
    #[serde(rename = "powerSource", skip_serializing_if = "Option::is_none")]
    pub power_source: Option<PowerSource>,
    pub raw: Vec<u8>,
}

/// The app's `PowerSource` (`MDCBatteryCommand.GetBatteryLevel`): the byte two
/// after the level in the battery reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PowerSource {
    /// On battery only
    None,
    AcPower,
    Usb,
    Wireless,
}

impl PowerSource {
    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::None),
            1 => Some(Self::AcPower),
            2 => Some(Self::Usb),
            3 => Some(Self::Wireless),
            _ => None,
        }
    }
}

impl BatteryState {
    /// Plugged in (any power source but the battery), when the display says
    pub fn plugged_in(&self) -> Option<bool> {
        self.power_source.map(|p| p != PowerSource::None)
    }
}

/// Battery response: `73 CC PP HH LL ?? SS …` (echo, charging, present, health,
/// level %, then the power source two bytes after the level, read like the app).
pub fn parse_battery(payload: &[u8]) -> Result<BatteryState> {
    if payload.len() < 5 {
        return Err(MdcError::Protocol(format!(
            "battery response too short ({} bytes)",
            payload.len()
        )));
    }
    Ok(BatteryState {
        charging: payload[1] == 0x01,
        present: payload[2] == 0x01,
        healthy: payload[3] == 0x01,
        level: payload[4],
        power_source: payload.get(6).copied().and_then(PowerSource::from_code),
        raw: payload.to_vec(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Clock {
    pub day: u8,
    /// 0–23
    pub hour: u8,
    pub minute: u8,
    pub month: u8,
    pub year: u16,
    pub raw: Vec<u8>,
}

/// Date/time register (0xA7), as the ePaper app reads and writes it:
/// `[day, hour 1–12, minute, month, year_hi, year_lo, am]` (am: 1 = AM, 0 = PM).
pub fn parse_clock(payload: &[u8]) -> Option<Clock> {
    if payload.len() < 7 {
        return None;
    }
    let (h12, am) = (payload[1], payload[6] == 1);
    let hour = match (am, h12) {
        (true, 12) => 0,
        (true, h) => h,
        (false, 12) => 12,
        (false, h) => h + 12,
    };
    Some(Clock {
        day: payload[0],
        hour,
        minute: payload[2],
        month: payload[3],
        year: u16::from(payload[4]) << 8 | u16::from(payload[5]),
        raw: payload.to_vec(),
    })
}

/// Payload for setting the date/time register (see [`parse_clock`]).
pub fn clock_data(t: &chrono::NaiveDateTime) -> [u8; 7] {
    use chrono::{Datelike, Timelike};
    let hour = t.hour() as u8;
    let (h12, am) = match hour {
        0 => (12, 1),
        1..=11 => (hour, 1),
        12 => (12, 0),
        _ => (hour - 12, 0),
    };
    let year = t.year().clamp(0, u16::MAX as i32) as u16;
    [
        t.day() as u8,
        h12,
        t.minute() as u8,
        t.month() as u8,
        (year >> 8) as u8,
        year as u8,
        am,
    ]
}

/// Wi-Fi MAC reply: the address as ASCII hex (`b0f2f6550d0b`), as the
/// ePaper app reads it (sub-command echo removed).
pub fn parse_wifi_mac(data: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(data).ok()?.trim_end_matches('\0');
    let hex: String = text.chars().filter(|c| *c != ':' && *c != '-').collect();
    if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let pairs: Vec<String> = (0..6)
        .map(|i| hex[i * 2..i * 2 + 2].to_ascii_uppercase())
        .collect();
    Some(pairs.join(":"))
}

/// Payload asking the display to advertise for Bluetooth setup.
pub fn turn_on_ble_data(duration: u16) -> [u8; 6] {
    let [hi, lo] = duration.to_be_bytes();
    [sub::TURN_ON_BLE, 0x00, 3, hi, lo, 0xD2]
}

/// Terms & conditions state, from the TLV records of the terms query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Terms {
    /// `Some(true)` when the display's terms were accepted.
    pub agreed: Option<bool>,
    /// Page to read before accepting.
    pub url: Option<String>,
}

#[derive(serde::Deserialize)]
struct TermsJson {
    #[serde(rename = "URL", default)]
    url: String,
    #[serde(rename = "OPENAPI", default)]
    open_api: String,
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "PARAM", default)]
    param: Vec<KeyValue>,
    #[serde(rename = "SUPPORT", default)]
    support: Vec<KeyValue>,
}

#[derive(serde::Deserialize)]
struct KeyValue {
    key: String,
    value: String,
}

/// Auto-sleep delay (`0xC6/0x81`): how long the display stays awake after
/// showing new content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepTime {
    Off,
    /// One of the app's presets: 5, 10, 20, 30 or 60 minutes.
    Minutes(u8),
    /// Any other number of minutes (code `0xF0`, then the minutes as a big-endian
    /// u16: the app's `AutoSleepTime.CustomSleepTime(customMinutes)`).
    Custom(u16),
}

impl SleepTime {
    const PRESETS: [(u8, u8); 5] = [(5, 6), (10, 7), (20, 8), (30, 9), (60, 10)];
    /// What the app lets you pick for a custom time: 5 minutes to 23 h 59 min
    pub const MIN_MINUTES: u16 = 5;
    pub const MAX_MINUTES: u16 = 23 * 60 + 59;

    /// `minutes` as a preset when it is one, else a custom time
    pub fn from_minutes(minutes: u16) -> Result<SleepTime> {
        if !(Self::MIN_MINUTES..=Self::MAX_MINUTES).contains(&minutes) {
            return Err(MdcError::InvalidArgument(format!(
                "sleep time must be between 5 minutes and 23 h 59 min (got {minutes} minutes)"
            )));
        }
        Ok(Self::PRESETS
            .iter()
            .find(|(min, _)| u16::from(*min) == minutes)
            .map(|(min, _)| SleepTime::Minutes(*min))
            .unwrap_or(SleepTime::Custom(minutes)))
    }

    /// Minutes before sleeping, `None` when it never sleeps
    pub fn minutes(self) -> Option<u16> {
        match self {
            SleepTime::Off => None,
            SleepTime::Minutes(m) => Some(m.into()),
            SleepTime::Custom(m) => Some(m),
        }
    }

    /// Set data (after the sub-command).
    pub fn to_data(self) -> Result<Vec<u8>> {
        match self {
            SleepTime::Off => Ok(vec![0]),
            SleepTime::Minutes(m) => Self::PRESETS
                .iter()
                .find(|(min, _)| *min == m)
                .map(|(_, code)| vec![*code])
                .ok_or_else(|| {
                    MdcError::InvalidArgument(format!(
                        "sleep time must be off, 5, 10, 20, 30 or 60 minutes (got {m})"
                    ))
                }),
            SleepTime::Custom(v) => Ok(vec![0xF0, (v >> 8) as u8, v as u8]),
        }
    }

    /// Parse the reply data (after the sub-command).
    pub fn parse(data: &[u8]) -> Option<SleepTime> {
        match *data.first()? {
            0 => Some(SleepTime::Off),
            0xF0 => {
                let v = data.get(1..3)?;
                Some(SleepTime::Custom(u16::from_be_bytes([v[0], v[1]])))
            }
            code => Self::PRESETS
                .iter()
                .find(|(_, c)| *c == code)
                .map(|(min, _)| SleepTime::Minutes(*min)),
        }
    }
}

impl std::fmt::Display for SleepTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SleepTime::Off => f.write_str("off"),
            SleepTime::Minutes(m) => write!(f, "{m} minutes"),
            SleepTime::Custom(m) if m % 60 == 0 => write!(f, "{} h", m / 60),
            SleepTime::Custom(m) if *m > 60 => write!(f, "{} h {} min", m / 60, m % 60),
            SleepTime::Custom(m) => write!(f, "{m} minutes"),
        }
    }
}

/// Daily refresh setting (`0x1B/0xB0` reply).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DailyRefresh {
    pub hour: u8,
    pub minute: u8,
    /// `(day, month)` of the next refresh, when the display reports it.
    pub next: Option<(u8, u8)>,
}

/// Parse the daily refresh reply (after the sub-command): `tag len value`
/// records. Tag `0x80` is `[hour, minute]` (what an EM32DX sends, and the Set
/// format); tag `0x81` is `[hour, minute, day, month]` (what the app's Get reads).
pub fn parse_daily_refresh(data: &[u8]) -> Option<DailyRefresh> {
    let mut out: Option<DailyRefresh> = None;
    let mut i = 0;
    while i + 2 <= data.len() {
        let (tag, len) = (data[i], data[i + 1] as usize);
        let v = data.get(i + 2..i + 2 + len)?;
        match tag {
            0x80 if len >= 2 => {
                let next = out.and_then(|r| r.next);
                out = Some(DailyRefresh {
                    hour: v[0],
                    minute: v[1],
                    next,
                });
            }
            0x81 if len >= 4 => {
                out = Some(DailyRefresh {
                    hour: v[0],
                    minute: v[1],
                    next: Some((v[2], v[3])),
                });
            }
            _ => {}
        }
        i += 2 + len;
    }
    out
}

/// Set data for a daily refresh at `hour:minute` (`None`: refresh now).
pub fn daily_refresh_data(at: Option<(u8, u8)>) -> Result<Vec<u8>> {
    match at {
        None => Ok(vec![0, 0]),
        Some((h, m)) if h < 24 && m < 60 => Ok(vec![0x80, 2, h, m]),
        Some((h, m)) => Err(MdcError::InvalidArgument(format!(
            "invalid time {h:02}:{m:02}"
        ))),
    }
}

/// What `0x9F` resets (the ePaper app's "Reset Settings" and "Reset All").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetKind {
    /// "Reset all device settings to default."
    Settings,
    /// "Restores a device to its factory settings": settings, network and stored content.
    All,
}

impl ResetKind {
    pub fn code(self) -> u8 {
        match self {
            ResetKind::Settings => 0x02,
            ResetKind::All => 0x03,
        }
    }
}

/// What `0x1B/0x74` erases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    /// Downloaded content (images).
    Files,
    NetworkCertificates,
    AppCertificates,
}

impl StorageKind {
    pub fn code(self) -> u8 {
        match self {
            StorageKind::Files => 0x00,
            StorageKind::NetworkCertificates => 0x80,
            StorageKind::AppCertificates => 0x81,
        }
    }
}

/// Identity block from "contact Samsung" (`0xD2/0x00`): TLV records of
/// `code:2` (0x8000–0xBFFF) `len:1` and UTF-8 text. Returns `(code, text)`.
pub fn parse_contact_info(data: &[u8]) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let code = u16::from_be_bytes([data[i], data[i + 1]]);
        let len = data[i + 2] as usize;
        if !(0x8000..0xC000).contains(&code) {
            break;
        }
        let Some(v) = data.get(i + 3..i + 3 + len) else {
            break;
        };
        out.push((
            code,
            String::from_utf8_lossy(v)
                .trim_end_matches('\0')
                .to_string(),
        ));
        i += 3 + len;
    }
    out
}

/// Label of a [`parse_contact_info`] code, as named in the ePaper app.
pub fn contact_info_label(code: u16) -> Option<&'static str> {
    Some(match code {
        0x8000 => "Model",
        0x8001 => "Serial number",
        0x8002 => "Software",
        0x8003 => "Status",
        0x8004 => "Sub-micom",
        0x8005 => "Wired MAC",
        0x8006 => "Wi-Fi MAC",
        0x8007 => "Bluetooth MAC",
        0x8008 => "Unique ID",
        0x8009 => "Unique device ID",
        _ => return None,
    })
}

/// Capability flags (`0xCB`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    pub low_power_wifi: bool,
    /// Content is refreshed "by refresh" (vs. only on new content).
    pub refresh_by_refresh: bool,
}

/// Parse the `0xCB` reply payload: flags in byte 1.
pub fn parse_features(payload: &[u8]) -> Option<Features> {
    let b = *payload.get(1)?;
    Some(Features {
        low_power_wifi: b & 1 == 1,
        refresh_by_refresh: (b >> 5) & 3 == 1,
    })
}

/// Parse the terms query reply (sub-command echo already removed):
/// records of `code:2` then either a 1-byte agreement flag (code 0, 0 = agreed)
/// or `len:2` + data (code 0xC000 = JSON describing the terms page).
pub fn parse_terms(data: &[u8]) -> Terms {
    let mut terms = Terms::default();
    let mut i = 0;
    while i + 3 <= data.len() {
        let code = u16::from(data[i]) << 8 | u16::from(data[i + 1]);
        if code == 0 {
            terms.agreed = Some(data[i + 2] == 0);
            i += 3;
            continue;
        }
        if i + 4 > data.len() {
            break;
        }
        let len = usize::from(data[i + 2]) << 8 | usize::from(data[i + 3]);
        let body = &data[(i + 4).min(data.len())..(i + 4 + len).min(data.len())];
        if code == 0xC000 {
            if let Ok(j) = serde_json::from_slice::<TermsJson>(body) {
                let join = |kv: &[KeyValue]| {
                    kv.iter()
                        .map(|p| format!("{}={}", p.key, p.value))
                        .collect::<Vec<_>>()
                        .join("&")
                };
                terms.url = Some(format!(
                    "{}/{}/{}?{}&{}",
                    j.url,
                    j.open_api,
                    j.id,
                    join(&j.param),
                    join(&j.support)
                ));
            }
        }
        i += len + 4;
    }
    terms
}

/// Decode a string reply, dropping NUL padding.
pub fn payload_string(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload)
        .trim_end_matches('\0')
        .to_string()
}

// ─── TLS: the display presents a self-signed certificate ────────────────────

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

/// Accepts any certificate (the display's is self-signed) but still checks
/// handshake signatures, like Node's `rejectUnauthorized: false`.
#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = provider();
            let config = ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("ring provider supports the default TLS versions")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// Hex dump of bytes for log lines, rendered only when a trace line is recorded.
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{b:02X}")?;
        }
        Ok(())
    }
}

async fn read_more<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut Vec<u8>) -> Result<()> {
    let mut chunk = [0u8; 1024];
    let n = stream.read(&mut chunk).await?;
    if n == 0 {
        return Err(MdcError::Closed);
    }
    buf.extend_from_slice(&chunk[..n]);
    Ok(())
}

// ─── Device + session ────────────────────────────────────────────────────────

/// Connection parameters for one display.
#[derive(Debug, Clone)]
pub struct Device {
    pub host: String,
    pub port: u16,
    pub pin: String,
    pub display_id: u8,
    pub command_timeout: Duration,
}

impl Device {
    pub fn new(host: impl Into<String>, pin: impl Into<String>) -> Self {
        Device {
            host: host.into(),
            port: DEFAULT_PORT,
            pin: pin.into(),
            display_id: 0,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_command_timeout(mut self, t: Duration) -> Self {
        self.command_timeout = t;
        self
    }

    /// Open an authenticated MDC session.
    pub async fn connect(&self, connect_timeout: Duration) -> Result<Session> {
        match timeout(connect_timeout, self.connect_inner()).await {
            Ok(result) => result,
            Err(_) => Err(MdcError::Timeout(format!(
                "Connection timed out after {}s — display may be asleep",
                connect_timeout.as_secs()
            ))),
        }
    }

    async fn connect_inner(&self) -> Result<Session> {
        let mut tcp = TcpStream::connect((self.host.as_str(), self.port)).await?;
        let _ = tcp.set_nodelay(true);

        let mut greeting = Vec::new();
        while find(&greeting, BANNER).is_none() {
            if greeting.len() > 1024 {
                return Err(MdcError::Protocol("no MDCSTART<<TLS>> greeting".into()));
            }
            read_more(&mut tcp, &mut greeting).await?;
        }

        let server_name = ServerName::try_from(self.host.clone())
            .map_err(|e| MdcError::InvalidArgument(format!("invalid host {:?}: {e}", self.host)))?;
        let mut tls = TlsConnector::from(tls_config())
            .connect(server_name, tcp)
            .await
            .map_err(|e| MdcError::Tls(e.to_string()))?;

        tls.write_all(self.pin.as_bytes()).await?;
        tls.flush().await?;

        let mut buf = Vec::new();
        loop {
            if let Some((result, consumed)) = parse_auth(&buf) {
                buf.drain(..consumed);
                return match result {
                    AuthResult::Pass => Ok(Session {
                        stream: tls,
                        buf,
                        display_id: self.display_id,
                        command_timeout: self.command_timeout,
                    }),
                    AuthResult::WrongPin => Err(MdcError::AuthFailed("Incorrect PIN")),
                    AuthResult::Blocked => Err(MdcError::AuthFailed("Blocked")),
                    AuthResult::Other(_) => Err(MdcError::AuthFailed("Unknown reason")),
                };
            }
            if buf.len() > 4096 {
                return Err(MdcError::Protocol("no MDCAUTH reply".into()));
            }
            read_more(&mut tls, &mut buf).await?;
        }
    }
}

/// An authenticated MDC session. Commands are strictly sequential.
pub struct Session {
    stream: TlsStream<TcpStream>,
    buf: Vec<u8>,
    display_id: u8,
    command_timeout: Duration,
}

impl Session {
    /// Send a command and wait for its ACK payload (NAK → [`MdcError::Nak`]).
    pub async fn command(&mut self, command: u8, data: &[u8]) -> Result<Vec<u8>> {
        let frame = encode_command(command, self.display_id, data)?;
        self.exchange(command, &frame, false).await
    }

    /// Like [`Session::command`], with 2-byte lengths both ways.
    pub async fn command_large(&mut self, command: u8, data: &[u8]) -> Result<Vec<u8>> {
        let frame = encode_command_large(command, self.display_id, data)?;
        self.exchange(command, &frame, true).await
    }

    async fn exchange(&mut self, command: u8, frame: &[u8], large: bool) -> Result<Vec<u8>> {
        tracing::trace!(command = %Hex(&[command]), frame = %Hex(frame), "MDC →");
        self.stream.write_all(frame).await?;
        self.stream.flush().await?;
        let result = match timeout(self.command_timeout, self.read_response(command, large)).await {
            Ok(result) => result,
            Err(_) => Err(MdcError::Timeout(format!(
                "No response to MDC command 0x{command:02X}"
            ))),
        };
        match &result {
            Ok(payload) => {
                tracing::trace!(command = %Hex(&[command]), payload = %Hex(payload), "MDC ←")
            }
            Err(e) => tracing::debug!(command = %Hex(&[command]), error = %e, "MDC command failed"),
        }
        result
    }

    async fn read_response(&mut self, command: u8, large: bool) -> Result<Vec<u8>> {
        loop {
            match decode_response_with(&self.buf, large) {
                Decoded::Incomplete => read_more(&mut self.stream, &mut self.buf).await?,
                Decoded::Skip(n) => {
                    self.buf.drain(..n);
                }
                Decoded::BadChecksum(n) => {
                    self.buf.drain(..n);
                    return Err(MdcError::ChecksumMismatch);
                }
                Decoded::Frame(resp, n) => {
                    self.buf.drain(..n);
                    if resp.command != command {
                        continue; // stale reply to an earlier command
                    }
                    return if resp.ack {
                        Ok(resp.payload)
                    } else {
                        Err(MdcError::Nak {
                            command,
                            payload: resp.payload,
                        })
                    };
                }
            }
        }
    }

    /// Set the display's clock (the ePaper app sends the phone's local time).
    pub async fn set_clock(&mut self, t: &chrono::NaiveDateTime) -> Result<()> {
        self.command(cmd::CLOCK, &clock_data(t)).await.map(drop)
    }

    /// Whether first-time setup was completed.
    pub async fn setup_done(&mut self) -> Result<bool> {
        let p = self.command(cmd::BATTERY, &[sub::SETUP_STATUS]).await?;
        Ok(p.last().is_some_and(|b| *b != 0 && p.len() > 1))
    }

    /// Mark first-time setup as completed (or not).
    pub async fn set_setup_done(&mut self, done: bool) -> Result<()> {
        self.command(cmd::BATTERY, &[sub::SETUP_STATUS, u8::from(done)])
            .await
            .map(drop)
    }

    /// Change the MDC PIN (applies to future sessions).
    pub async fn set_pin(&mut self, pin: &str) -> Result<()> {
        if pin.len() != 6 || !pin.bytes().all(|b| b.is_ascii_digit()) {
            return Err(MdcError::InvalidArgument("the PIN must be 6 digits".into()));
        }
        let mut data = vec![sub::PIN];
        data.extend_from_slice(pin.as_bytes());
        self.command(cmd::BATTERY, &data).await.map(drop)
    }

    pub async fn set_device_name(&mut self, name: &str) -> Result<()> {
        if name.is_empty() || name.len() > 64 {
            return Err(MdcError::InvalidArgument(
                "the device name must be 1-64 bytes".into(),
            ));
        }
        self.command(cmd::DEVICE_NAME, name.as_bytes())
            .await
            .map(drop)
    }

    /// Wi-Fi MAC address as `AA:BB:CC:DD:EE:FF`.
    pub async fn wifi_mac(&mut self) -> Result<String> {
        let p = self.command(cmd::BATTERY, &[sub::WIFI_MAC]).await?;
        parse_wifi_mac(p.get(1..).unwrap_or_default())
            .ok_or_else(|| MdcError::Protocol("unexpected Wi-Fi MAC reply".into()))
    }

    /// Terms & conditions state and page.
    pub async fn terms(&mut self) -> Result<Terms> {
        let p = self.command_large(cmd::TERMS, &[sub::TERMS]).await?;
        Ok(parse_terms(p.get(1..).unwrap_or_default()))
    }

    /// Accept the terms & conditions (what the app sends on "Agree").
    pub async fn accept_terms(&mut self) -> Result<()> {
        self.command_large(cmd::TERMS, &[sub::TERMS, 0, 0, 0])
            .await
            .map(drop)
    }

    /// Which content source the display plays (see [`content_player`]).
    pub async fn content_player(&mut self) -> Result<u8> {
        let p = self
            .command(cmd::CONTENT_DOWNLOAD, &[sub::CONTENT_PLAYER])
            .await?;
        p.get(1)
            .copied()
            .ok_or_else(|| MdcError::Protocol("unexpected content player reply".into()))
    }

    /// Select the content source. Images sent over MDC are only shown with
    /// [`content_player::MOBILE_CMS`], which the app sets during first-time setup.
    pub async fn set_content_player(&mut self, value: u8) -> Result<()> {
        self.command(cmd::CONTENT_DOWNLOAD, &[sub::CONTENT_PLAYER, value])
            .await
            .map(drop)
    }

    /// Whether the display sleeps as soon as new content is shown (instead of
    /// waiting for the sleep time)
    pub async fn immediate_sleep(&mut self) -> Result<bool> {
        let p = self
            .command(cmd::CONTENT_DOWNLOAD, &[sub::IMMEDIATE_SLEEP])
            .await?;
        // The app: `[01, 01]` is on, anything else off
        Ok(p.get(1..3) == Some(&[1, 1][..]))
    }

    pub async fn set_immediate_sleep(&mut self, on: bool) -> Result<()> {
        self.command(
            cmd::CONTENT_DOWNLOAD,
            &[sub::IMMEDIATE_SLEEP, 1, u8::from(on)],
        )
        .await
        .map(drop)
    }

    pub async fn sleep_time(&mut self) -> Result<SleepTime> {
        let p = self.command(cmd::SLEEP_TIME, &[sub::SLEEP_TIME]).await?;
        SleepTime::parse(p.get(1..).unwrap_or_default())
            .ok_or_else(|| MdcError::Protocol("unexpected sleep time reply".into()))
    }

    pub async fn set_sleep_time(&mut self, t: SleepTime) -> Result<()> {
        let mut data = vec![sub::SLEEP_TIME];
        data.extend(t.to_data()?);
        self.command(cmd::SLEEP_TIME, &data).await.map(drop)
    }

    /// The next scheduled daily refresh.
    pub async fn daily_refresh(&mut self) -> Result<DailyRefresh> {
        let p = self.command(cmd::BATTERY, &[sub::DAILY_REFRESH]).await?;
        parse_daily_refresh(p.get(1..).unwrap_or_default())
            .ok_or_else(|| MdcError::Protocol(format!("unexpected daily refresh reply {p:02X?}")))
    }

    /// Refresh every day at `hour:minute` (`None`: refresh the screen now).
    pub async fn set_daily_refresh(&mut self, at: Option<(u8, u8)>) -> Result<()> {
        let mut data = vec![sub::DAILY_REFRESH];
        data.extend(daily_refresh_data(at)?);
        self.command(cmd::BATTERY, &data).await.map(drop)
    }

    /// Erase stored content or certificates.
    pub async fn empty_storage(&mut self, kind: StorageKind) -> Result<()> {
        self.command(cmd::BATTERY, &[sub::EMPTY_STORAGE, kind.code(), 0])
            .await
            .map(drop)
    }

    /// Reset the display. It may restart before replying: a timeout or a
    /// closed connection after sending is reported as `Ok(false)` (sent,
    /// not confirmed); `Ok(true)` means it acknowledged.
    pub async fn reset(&mut self, kind: ResetKind) -> Result<bool> {
        match self.command(cmd::RESET_CONTROL, &[kind.code()]).await {
            Ok(_) => Ok(true),
            Err(MdcError::Timeout(_) | MdcError::Closed | MdcError::Io(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Identity records (model, MACs, unique IDs…), see [`contact_info_label`].
    pub async fn contact_info(&mut self) -> Result<Vec<(u16, String)>> {
        let p = self.command_large(cmd::TERMS, &[sub::CONTACT_INFO]).await?;
        Ok(parse_contact_info(p.get(1..).unwrap_or_default()))
    }

    pub async fn features(&mut self) -> Result<Features> {
        let p = self.command(cmd::SUPPORT_FEATURE, &[]).await?;
        parse_features(&p).ok_or_else(|| MdcError::Protocol("unexpected feature reply".into()))
    }

    /// `true` when the display is set to portrait.
    pub async fn portrait(&mut self) -> Result<bool> {
        let p = self.command(cmd::ORIENTATION, &[sub::ORIENTATION]).await?;
        p.get(1)
            .map(|v| *v == 1)
            .ok_or_else(|| MdcError::Protocol("unexpected orientation reply".into()))
    }

    /// Set portrait (`true`) or landscape (`false`), as the app's device page does.
    pub async fn set_portrait(&mut self, portrait: bool) -> Result<()> {
        self.command(cmd::ORIENTATION, &[sub::ORIENTATION, u8::from(portrait)])
            .await
            .map(drop)
    }

    /// The background colour the display is set to (see [`bg_color`]).
    pub async fn background_color(&mut self) -> Result<u8> {
        let p = self.command(cmd::SCREEN, &[sub::BG_COLOR]).await?;
        // The app reads the colour at `payload[1]`; emdx keeps the sub-command byte,
        // which the display echoes back first
        p.get(2).copied().ok_or_else(|| {
            MdcError::Protocol(format!("unexpected background colour reply {p:02X?}"))
        })
    }

    /// Set the background colour (`bg_color::WHITE` … `bg_color::YELLOW`), as the app's
    /// device page does. Its payload is `[00, colour]`; what the leading `00` means is
    /// unknown, but the app always sends it.
    pub async fn set_background_color(&mut self, colour: u8) -> Result<()> {
        let name = bg_color::name(colour).ok_or_else(|| {
            MdcError::InvalidArgument(format!(
                "unknown background colour {colour}: use {}",
                bg_color::ALL
                    .iter()
                    .map(|(_, n)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        tracing::debug!(colour = name, "setting the background colour");
        self.command(cmd::SCREEN, &[sub::BG_COLOR, 0x00, colour])
            .await
            .map(drop)
    }

    /// Make the display advertise for Bluetooth setup for a while.
    pub async fn turn_on_ble(&mut self, duration: u16) -> Result<()> {
        self.command(cmd::BLUETOOTH, &turn_on_ble_data(duration))
            .await
            .map(drop)
    }

    pub async fn serial_number(&mut self) -> Result<String> {
        Ok(payload_string(
            &self.command(cmd::SERIAL_NUMBER, &[]).await?,
        ))
    }

    pub async fn software_version(&mut self) -> Result<String> {
        Ok(payload_string(
            &self.command(cmd::SOFTWARE_VERSION, &[]).await?,
        ))
    }

    pub async fn device_name(&mut self) -> Result<String> {
        Ok(payload_string(&self.command(cmd::DEVICE_NAME, &[]).await?))
    }

    /// `None` when unknown — EMDX displays NAK the power-state query.
    pub async fn power_state(&mut self) -> Option<&'static str> {
        let payload = self.command(cmd::POWER, &[]).await.ok()?;
        parse_power(&payload)
    }

    pub async fn set_power(&mut self, on: bool) -> Result<()> {
        self.command(cmd::POWER, &[u8::from(on)]).await.map(drop)
    }

    pub async fn set_network_standby(&mut self, enabled: bool) -> Result<()> {
        self.command(cmd::NETWORK_STANDBY, &[u8::from(enabled)])
            .await
            .map(drop)
    }

    pub async fn network_standby(&mut self) -> Option<bool> {
        let payload = self.command(cmd::NETWORK_STANDBY, &[]).await.ok()?;
        Some(payload.first() == Some(&0x01))
    }

    pub async fn set_on_timer(
        &mut self,
        enabled: bool,
        repeat: u8,
        hour: u8,
        minute: u8,
    ) -> Result<()> {
        self.command(cmd::ON_TIMER, &on_timer_data(enabled, repeat, hour, minute))
            .await
            .map(drop)
    }

    /// Raw On Timer register; `None` when unsupported.
    pub async fn on_timer(&mut self) -> Option<Vec<u8>> {
        self.command(cmd::ON_TIMER, &[]).await.ok()
    }

    pub async fn model_info(&mut self) -> Option<Vec<u8>> {
        self.command(cmd::MODEL_INFO, &[]).await.ok()
    }

    pub async fn clock(&mut self) -> Option<Clock> {
        let payload = self.command(cmd::CLOCK, &[]).await.ok()?;
        parse_clock(&payload)
    }

    pub async fn battery(&mut self) -> Result<BatteryState> {
        parse_battery(&self.command(cmd::BATTERY, &[0x73]).await?)
    }

    /// Point the display at a content manifest URL; it downloads it over HTTP.
    pub async fn set_content_download(&mut self, url: &str) -> Result<()> {
        self.command(cmd::CONTENT_DOWNLOAD, &content_download_data(url)?)
            .await
            .map(drop)
    }

    /// Ask the display to upload a thumbnail of what it is showing (`0xC7/0x54`).
    ///
    /// `url` is a URL *we* serve: the display POSTs `multipart/form-data` to it
    /// later, so run a [`crate::thumbnail::Endpoint`] before calling this. The
    /// display's reply is returned as text (the ePaper app only logs it). A real
    /// panel echoes `54 <len> <url>`; that header is stripped, leaving the URL.
    pub async fn request_thumbnail(&mut self, url: &str) -> Result<Option<String>> {
        let payload = self
            .command(cmd::CONTENT_DOWNLOAD, &thumbnail_data(url)?)
            .await?;
        let text = payload_string(thumbnail_echo(&payload));
        Ok((!text.is_empty()).then_some(text))
    }

    /// Hand the display the certificate of our HTTPS thumbnail receiver
    /// (`0xD2/0x20`, large frames). The ePaper app always does this right before
    /// `0xC7/0x54` and skips the thumbnail request when it fails.
    pub async fn push_cms_cert(&mut self, pem: &str) -> Result<()> {
        self.command_large(cmd::TERMS, &cms_cert_data(pem)?)
            .await
            .map(drop)
    }

    /// Show the content at `manifest_url`. The display only shows images sent this
    /// way with the mobile content player, so it is selected first when it isn't.
    /// Returns `true` when the content player was switched to it.
    pub async fn push_content(&mut self, manifest_url: &str) -> Result<bool> {
        self.push_cms_content(manifest_url, None).await
    }

    /// [`Session::push_content`] the way the app posts content: with `cert_pem` (an
    /// HTTPS manifest URL), the display is first handed that certificate (`0xD2/0x20`),
    /// and nothing more is sent when it refuses it.
    ///
    /// Two steps here are ours, not the app's: the content player is selected when it
    /// isn't the mobile one (the app leaves that to its device screen), and network
    /// standby is turned on so the display stays reachable.
    pub async fn push_cms_content(
        &mut self,
        manifest_url: &str,
        cert_pem: Option<&str>,
    ) -> Result<bool> {
        let switched = match self.content_player().await {
            Ok(content_player::MOBILE_CMS) => false,
            // Another player, or a reply we can't read: select it to be sure
            _ => {
                self.set_content_player(content_player::MOBILE_CMS).await?;
                true
            }
        };
        let _ = self.set_network_standby(true).await;
        if let Some(pem) = cert_pem {
            self.push_cms_cert(pem).await?;
        }
        self.set_content_download(manifest_url).await?;
        Ok(switched)
    }

    /// Power off. `network_standby = false` is a true deep sleep (only
    /// Wake-on-LAN or the On Timer wakes it); `true` stays MDC-wakeable.
    pub async fn power_off(&mut self, network_standby: bool) -> Result<()> {
        let _ = self.set_network_standby(network_standby).await;
        self.set_power(false).await
    }

    /// Close the TLS session (best effort).
    pub async fn close(mut self) {
        let _ = timeout(Duration::from_secs(2), self.stream.shutdown()).await;
    }
}

impl Device {
    /// Connect, run `f` on the session, and always close it afterwards.
    pub async fn with_session<T>(
        &self,
        connect_timeout: Duration,
        f: impl AsyncFnOnce(&mut Session) -> Result<T>,
    ) -> Result<T> {
        let mut session = self.connect(connect_timeout).await?;
        let result = f(&mut session).await;
        session.close().await;
        result
    }

    /// One-shot [`Session::push_content`].
    pub async fn push_content(&self, manifest_url: &str) -> Result<bool> {
        self.with_session(DEFAULT_CONNECT_TIMEOUT, async |s| {
            s.push_content(manifest_url).await
        })
        .await
    }

    /// One-shot [`Session::push_cms_content`].
    pub async fn push_cms_content(
        &self,
        manifest_url: &str,
        cert_pem: Option<&str>,
    ) -> Result<bool> {
        self.with_session(DEFAULT_CONNECT_TIMEOUT, async |s| {
            s.push_cms_content(manifest_url, cert_pem).await
        })
        .await
    }

    /// One-shot [`Session::power_off`].
    pub async fn power_off(&self, network_standby: bool) -> Result<()> {
        self.with_session(DEFAULT_CONNECT_TIMEOUT, async |s| {
            s.power_off(network_standby).await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(cmd: u8, ack: u8, payload: &[u8]) -> Vec<u8> {
        encode_response(&Response {
            display_id: 0,
            ack: ack == ACK,
            command: cmd,
            payload: payload.to_vec(),
        })
        .unwrap()
    }

    #[test]
    fn response_encoding_round_trips() {
        let resp = Response {
            display_id: 3,
            ack: false,
            command: cmd::POWER,
            payload: vec![0x01, 0x02],
        };
        let frame = encode_response(&resp).unwrap();
        assert_eq!(decode_response(&frame), Decoded::Frame(resp, frame.len()));
        let too_long = Response {
            payload: vec![0; 254],
            ..Response {
                display_id: 0,
                ack: true,
                command: 0,
                payload: vec![],
            }
        };
        assert!(encode_response(&too_long).is_err());
    }

    #[test]
    fn large_frames_round_trip() {
        let f = encode_command_large(cmd::TERMS, 0, &[sub::TERMS]).unwrap();
        assert_eq!(
            f,
            vec![
                0xAA,
                0xD2,
                0x00,
                0x00,
                0x01,
                0x70,
                checksum(&[0xD2, 0, 0, 1, 0x70])
            ]
        );
        // Large response: AA FF id len_hi len_lo ack cmd payload… cs
        let payload: Vec<u8> = (0..300u16).map(|i| i as u8).collect();
        let len = (payload.len() + 2) as u16;
        let mut r = vec![
            HEADER,
            RESPONSE,
            0,
            (len >> 8) as u8,
            len as u8,
            ACK,
            cmd::TERMS,
        ];
        r.extend_from_slice(&payload);
        r.push(checksum(&r[1..]));
        let Decoded::Frame(resp, n) = decode_response_with(&r, true) else {
            panic!()
        };
        assert_eq!(
            (resp.command, resp.payload.len(), n),
            (cmd::TERMS, 300, r.len())
        );
        assert_eq!(decode_response_with(&r[..10], true), Decoded::Incomplete);
    }

    #[test]
    fn clock_round_trips_the_app_layout() {
        use chrono::NaiveDate;
        let at = |h, m| {
            NaiveDate::from_ymd_opt(2026, 11, 9)
                .unwrap()
                .and_hms_opt(h, m, 0)
                .unwrap()
        };
        assert_eq!(
            clock_data(&at(0, 5)),
            [9, 12, 5, 11, 0x07, 0xEA, 1],
            "midnight = 12 AM"
        );
        assert_eq!(
            clock_data(&at(12, 0)),
            [9, 12, 0, 11, 0x07, 0xEA, 0],
            "noon = 12 PM"
        );
        assert_eq!(clock_data(&at(13, 30)), [9, 1, 30, 11, 0x07, 0xEA, 0]);
        for h in 0..24 {
            let c = parse_clock(&clock_data(&at(h, 7))).unwrap();
            assert_eq!(
                (c.hour, c.minute, c.day, c.month, c.year),
                (h as u8, 7, 9, 11, 2026)
            );
        }
        assert!(parse_clock(&[1, 2, 3]).is_none());
    }

    #[test]
    fn wifi_mac_is_ascii_hex() {
        // Observed on a real EM32DX: the reply is text, not raw bytes
        assert_eq!(
            parse_wifi_mac(b"b0f2f6550d0b").as_deref(),
            Some("B0:F2:F6:55:0D:0B")
        );
        assert_eq!(
            parse_wifi_mac(b"B0:F2:F6:55:0D:0B\0").as_deref(),
            Some("B0:F2:F6:55:0D:0B")
        );
        assert_eq!(parse_wifi_mac(&[0xB0, 0xF2, 0xF6, 0x55, 0x0D, 0x0B]), None);
        assert_eq!(parse_wifi_mac(b"b0f2"), None);
    }

    #[test]
    fn ble_on_payload() {
        assert_eq!(turn_on_ble_data(2000), [0x10, 0x00, 3, 0x07, 0xD0, 0xD2]);
    }

    #[test]
    fn parses_terms_records() {
        let json = br#"{"URL":"https://terms.example","OPENAPI":"api/v1","ID":"epaper","PARAM":[{"key":"lang","value":"fr"}],"SUPPORT":[{"key":"model","value":"EM32DX"}],"TERMLIST":["eula"],"HEADER":[]}"#;
        let mut d = vec![0x00, 0x00, 0x01]; // not agreed
        d.extend_from_slice(&[0xC0, 0x00, 0x00, json.len() as u8]);
        d.extend_from_slice(json);
        let t = parse_terms(&d);
        assert_eq!(t.agreed, Some(false));
        assert_eq!(
            t.url.as_deref(),
            Some("https://terms.example/api/v1/epaper?lang=fr&model=EM32DX")
        );
        assert_eq!(parse_terms(&[0, 0, 0]).agreed, Some(true));
        assert_eq!(parse_terms(&[]), Terms::default());
        assert_eq!(
            parse_terms(&[0xC0, 0x00, 0xFF]).url,
            None,
            "truncated record is ignored"
        );
    }

    #[test]
    fn checksum_wraps_mod_256() {
        assert_eq!(checksum(&[]), 0);
        assert_eq!(checksum(&[0xFF, 0x02]), 0x01);
        assert_eq!(checksum(&[0x11, 0x00, 0x00]), 0x11);
    }

    #[test]
    fn encodes_power_off() {
        // AA 11 00 01 00 → checksum 0x12
        assert_eq!(
            encode_command(cmd::POWER, 0, &[0x00]).unwrap(),
            vec![0xAA, 0x11, 0x00, 0x01, 0x00, 0x12]
        );
    }

    #[test]
    fn encodes_query_without_data() {
        assert_eq!(
            encode_command(cmd::SERIAL_NUMBER, 0, &[]).unwrap(),
            vec![0xAA, 0x0B, 0x00, 0x00, 0x0B]
        );
    }

    #[test]
    fn rejects_oversized_payload() {
        assert!(matches!(
            encode_command(0x01, 0, &[0; 256]),
            Err(MdcError::InvalidArgument(_))
        ));
    }

    #[test]
    fn decodes_ack_frame() {
        let frame = response(cmd::BATTERY, ACK, &[0x73, 0x01, 0x01, 0x01, 0x55]);
        match decode_response(&frame) {
            Decoded::Frame(r, n) => {
                assert_eq!(n, frame.len());
                assert!(r.ack);
                assert_eq!(r.command, cmd::BATTERY);
                assert_eq!(r.payload, vec![0x73, 0x01, 0x01, 0x01, 0x55]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn decodes_nak_frame() {
        let frame = response(cmd::POWER, NAK, &[0x01]);
        match decode_response(&frame) {
            Decoded::Frame(r, _) => {
                assert!(!r.ack);
                assert_eq!(r.payload, vec![0x01]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn incomplete_until_all_bytes_arrive() {
        let frame = response(cmd::SERIAL_NUMBER, ACK, b"ABC123");
        for cut in 0..frame.len() {
            assert_eq!(
                decode_response(&frame[..cut]),
                Decoded::Incomplete,
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn detects_bad_checksum() {
        let mut frame = response(cmd::POWER, ACK, &[0x01]);
        *frame.last_mut().unwrap() ^= 0xFF;
        assert_eq!(decode_response(&frame), Decoded::BadChecksum(frame.len()));
    }

    #[test]
    fn skips_leading_garbage() {
        let mut buf = b"xyz".to_vec();
        buf.extend(response(cmd::POWER, ACK, &[0x01]));
        assert_eq!(decode_response(&buf), Decoded::Skip(3));
        assert!(matches!(decode_response(&buf[3..]), Decoded::Frame(..)));
    }

    #[test]
    fn decodes_back_to_back_frames() {
        let mut buf = response(cmd::POWER, ACK, &[0x01]);
        let first_len = buf.len();
        buf.extend(response(cmd::NETWORK_STANDBY, ACK, &[0x00]));
        let Decoded::Frame(a, n) = decode_response(&buf) else {
            panic!()
        };
        assert_eq!((a.command, n), (cmd::POWER, first_len));
        let Decoded::Frame(b, _) = decode_response(&buf[n..]) else {
            panic!()
        };
        assert_eq!(b.command, cmd::NETWORK_STANDBY);
    }

    #[test]
    fn parses_auth_replies() {
        assert_eq!(parse_auth(b"MDCAUTH<<PASS>>"), Some((AuthResult::Pass, 15)));
        assert_eq!(
            parse_auth(b"MDCAUTH<<FAIL:0x01>>").unwrap().0,
            AuthResult::WrongPin
        );
        assert_eq!(
            parse_auth(b"MDCAUTH<<FAIL:0x02>>").unwrap().0,
            AuthResult::Blocked
        );
        assert_eq!(
            parse_auth(b"MDCAUTH<<WHAT>>").unwrap().0,
            AuthResult::Other("WHAT".into())
        );
        assert_eq!(parse_auth(b"MDCAUTH<<PA"), None);
    }

    #[test]
    fn auth_reply_followed_by_frame_keeps_the_rest() {
        let mut buf = b"MDCAUTH<<PASS>>".to_vec();
        buf.extend(response(cmd::POWER, ACK, &[0x01]));
        let (_, consumed) = parse_auth(&buf).unwrap();
        assert!(matches!(
            decode_response(&buf[consumed..]),
            Decoded::Frame(..)
        ));
    }

    #[test]
    fn content_download_payload() {
        let data = content_download_data("http://x/c.json").unwrap();
        assert_eq!(&data[..3], &[0x53, 0x80, 15]);
        assert_eq!(&data[3..], b"http://x/c.json");
        assert!(content_download_data("").is_err());
        assert!(content_download_data(&"a".repeat(253)).is_err());
        let max = content_download_data(&"a".repeat(252)).unwrap();
        assert!(encode_command(cmd::CONTENT_DOWNLOAD, 0, &max).is_ok());
    }

    /// `v4/d.a()` writes `len = data.len + 1` when a sub-command is present, so putting
    /// the sub-command first in `data` reproduces the app's frame byte for byte.
    #[test]
    fn thumbnail_request_frame_matches_the_app() {
        let url = "http://192.168.1.5:6868/device-thumbnail";
        let frame =
            encode_command(cmd::CONTENT_DOWNLOAD, 0, &thumbnail_data(url).unwrap()).unwrap();
        assert_eq!(frame[0], HEADER);
        assert_eq!(frame[1], 0xC7);
        assert_eq!(frame[2], 0x00);
        assert_eq!(frame[3], (1 + url.len()) as u8);
        assert_eq!(frame[4], 0x54);
        assert_eq!(&frame[5..frame.len() - 1], url.as_bytes());
        assert_eq!(frame[frame.len() - 1], checksum(&frame[1..frame.len() - 1]));

        assert!(thumbnail_data("").is_err());
        assert!(encode_command(
            cmd::CONTENT_DOWNLOAD,
            0,
            &thumbnail_data(&"a".repeat(254)).unwrap()
        )
        .is_ok());
        assert!(thumbnail_data(&"a".repeat(255)).is_err());
    }

    /// The live EM32DX answered `54 29 http://192.168.1.39:6868/device-thumbnail`
    #[test]
    fn thumbnail_reply_echoes_the_url() {
        let url = b"http://192.168.1.39:6868/device-thumbnail";
        let mut reply = vec![0x54, url.len() as u8];
        reply.extend_from_slice(url);
        assert_eq!(reply[1], 0x29);
        assert_eq!(thumbnail_echo(&reply), url);
        assert_eq!(thumbnail_echo(b"something else"), b"something else");
        assert_eq!(thumbnail_echo(&[0x54, 9, b'x']), &[0x54, 9, b'x']);
    }

    /// `AA D2 00 lenHi lenLo 20 00 00 06 0D "CMSServer.crt" lenHi lenLo pem chk`
    #[test]
    fn cms_cert_frame_matches_the_app() {
        let pem = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
        let frame = encode_command_large(cmd::TERMS, 0, &cms_cert_data(pem).unwrap()).unwrap();
        let body_len = 1 + 1 + 2 + 1 + 13 + 2 + pem.len();
        assert_eq!(&frame[..3], &[HEADER, 0xD2, 0x00]);
        assert_eq!(&frame[3..5], &(body_len as u16).to_be_bytes());
        assert_eq!(&frame[5..10], &[0x20, 0x00, 0x00, 0x06, 13]);
        assert_eq!(&frame[10..23], b"CMSServer.crt");
        assert_eq!(&frame[23..25], &(pem.len() as u16).to_be_bytes());
        assert_eq!(&frame[25..frame.len() - 1], pem.as_bytes());
        assert_eq!(frame[frame.len() - 1], checksum(&frame[1..frame.len() - 1]));
    }

    #[test]
    fn on_timer_payload() {
        assert_eq!(on_timer_data(true, repeat::DAILY, 7, 30), [1, 2, 7, 30, 1]);
        assert_eq!(on_timer_data(false, repeat::ONCE, 0, 0), [0, 1, 0, 0, 1]);
    }

    #[test]
    fn parses_power() {
        assert_eq!(parse_power(&[0x00]), Some("Off"));
        assert_eq!(parse_power(&[0x01]), Some("On"));
        assert_eq!(parse_power(&[0x02]), Some("Reboot"));
        assert_eq!(parse_power(&[0x09]), None);
        assert_eq!(parse_power(&[]), None);
    }

    #[test]
    fn parses_battery() {
        let b = parse_battery(&[0x73, 0x01, 0x01, 0x00, 0x42, 0x10]).unwrap();
        assert!(b.charging && b.present && !b.healthy);
        assert_eq!(b.level, 0x42);
        assert_eq!(b.raw.len(), 6);
        assert!(parse_battery(&[0x73, 0x01]).is_err());
        assert_eq!(b.power_source, None, "not reported");
        let plugged = parse_battery(&[0x73, 0x01, 0x01, 0x01, 50, 0x00, 0x01]).unwrap();
        assert_eq!(plugged.power_source, Some(PowerSource::AcPower));
        assert_eq!(plugged.plugged_in(), Some(true));
        let on_battery = parse_battery(&[0x73, 0x00, 0x01, 0x01, 50, 0x00, 0x00]).unwrap();
        assert_eq!(on_battery.plugged_in(), Some(false));
    }

    #[test]
    fn battery_serializes_like_node() {
        let b = parse_battery(&[0x73, 0x00, 0x01, 0x01, 80]).unwrap();
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"charging":false,"present":true,"healthy":true,"level":80,"raw":[115,0,1,1,80]}"#
        );
    }

    #[test]
    fn parses_clock() {
        let c = parse_clock(&[9, 2, 41, 11, 0x07, 0xEA, 0]).unwrap();
        assert_eq!(
            (c.day, c.hour, c.minute, c.month, c.year),
            (9, 14, 41, 11, 2026)
        );
        assert!(parse_clock(&[1, 2, 3]).is_none());
    }

    #[test]
    fn payload_strings_drop_nul_padding() {
        assert_eq!(payload_string(b"EM32DX\0\0"), "EM32DX");
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn sleep_time_codes_round_trip() {
        for t in [
            SleepTime::Off,
            SleepTime::Minutes(5),
            SleepTime::Minutes(20),
            SleepTime::Minutes(60),
            SleepTime::Custom(0x012C),
        ] {
            assert_eq!(SleepTime::parse(&t.to_data().unwrap()), Some(t));
        }
        // The app's codes: 0 off, 6..=10 = 5/10/20/30/60 min, F0 = custom
        assert_eq!(SleepTime::Minutes(20).to_data().unwrap(), [8]);
        assert_eq!(
            SleepTime::Custom(300).to_data().unwrap(),
            [0xF0, 0x01, 0x2C]
        );
        assert!(SleepTime::Minutes(15).to_data().is_err());
        assert_eq!(SleepTime::parse(&[0x42]), None);
    }

    #[test]
    fn custom_sleep_times_are_minutes() {
        assert_eq!(SleepTime::from_minutes(20).unwrap(), SleepTime::Minutes(20));
        assert_eq!(SleepTime::from_minutes(90).unwrap(), SleepTime::Custom(90));
        // 23 h 59 min, big-endian after the custom code
        assert_eq!(
            SleepTime::from_minutes(1439).unwrap().to_data().unwrap(),
            [0xF0, 0x05, 0x9F]
        );
        assert!(SleepTime::from_minutes(4).is_err());
        assert!(SleepTime::from_minutes(1440).is_err());
        assert_eq!(SleepTime::Custom(90).to_string(), "1 h 30 min");
        assert_eq!(SleepTime::Custom(120).to_string(), "2 h");
        assert_eq!(SleepTime::Custom(45).to_string(), "45 minutes");
    }

    #[test]
    fn daily_refresh_formats() {
        // App: "at a time" = 80 02 hour minute; "now" = 00 00
        assert_eq!(daily_refresh_data(Some((23, 0))).unwrap(), [0x80, 2, 23, 0]);
        assert_eq!(daily_refresh_data(None).unwrap(), [0, 0]);
        assert!(daily_refresh_data(Some((24, 0))).is_err());
        // Real EM32DX reply after setup (sub-command removed): 80 02 01 00 = 01:00
        assert_eq!(
            parse_daily_refresh(&[0x80, 2, 1, 0]),
            Some(DailyRefresh {
                hour: 1,
                minute: 0,
                next: None
            })
        );
        // The format the app's Get parses
        assert_eq!(
            parse_daily_refresh(&[0x81, 4, 23, 0, 13, 11]),
            Some(DailyRefresh {
                hour: 23,
                minute: 0,
                next: Some((13, 11))
            })
        );
        assert_eq!(parse_daily_refresh(&[0x81, 4, 23, 0]), None);
        assert_eq!(parse_daily_refresh(&[]), None);
    }

    #[test]
    fn contact_info_tlv() {
        let mut data = vec![0x80, 0x00, 6];
        data.extend_from_slice(b"EM32DX");
        data.extend_from_slice(&[0x80, 0x06, 17]);
        data.extend_from_slice(b"B0:F2:F6:55:0D:0B");
        data.extend_from_slice(&[0x12, 0x34, 1, 0]); // out of range: stop
        let info = parse_contact_info(&data);
        assert_eq!(
            info,
            [
                (0x8000, "EM32DX".to_string()),
                (0x8006, "B0:F2:F6:55:0D:0B".to_string())
            ]
        );
        assert_eq!(contact_info_label(0x8006), Some("Wi-Fi MAC"));
        assert!(parse_contact_info(&[0x80, 0x00, 9, b'x']).is_empty());
    }

    #[test]
    fn feature_flags() {
        // byte 1: bit 0 low-power Wi-Fi, bits 5-6 refresh type (1 = by refresh)
        let f = parse_features(&[0x00, 0b0010_0001]).unwrap();
        assert!(f.low_power_wifi && f.refresh_by_refresh);
        let f = parse_features(&[0x00, 0b0100_0000]).unwrap();
        assert!(!f.low_power_wifi && !f.refresh_by_refresh);
        assert_eq!(parse_features(&[0x00]), None);
        assert_eq!(StorageKind::NetworkCertificates.code(), 0x80);
        // The app: ResetSystem ("Reset Settings") = [2], ResetAll ("Reset All") = [3]
        assert_eq!(ResetKind::Settings.code(), 2);
        assert_eq!(ResetKind::All.code(), 3);
    }

    #[test]
    fn background_color_names_round_trip() {
        for (value, name) in bg_color::ALL {
            assert_eq!(bg_color::name(*value), Some(*name));
            assert_eq!(bg_color::parse(name), Some(*value));
        }
        assert_eq!(bg_color::parse("Red"), Some(bg_color::RED));
        assert_eq!(bg_color::parse(" yellow "), Some(bg_color::YELLOW));
        // The app's own parser refuses them too (it matches its six entries)
        assert_eq!(bg_color::parse("pink"), None);
        assert_eq!(bg_color::name(6), None);
    }

    #[test]
    fn background_color_frames_match_the_app() {
        // The ePaper app's frames, byte for byte: `AA 21 00 <len> 90 00 <colour> <chk>`
        assert_eq!(
            encode_command(cmd::SCREEN, 0, &[sub::BG_COLOR]).unwrap(),
            [0xAA, 0x21, 0x00, 0x01, 0x90, 0xB2]
        );
        let expected: [u8; 6] = [0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9];
        for (i, chk) in expected.iter().enumerate() {
            assert_eq!(
                encode_command(cmd::SCREEN, 0, &[sub::BG_COLOR, 0x00, i as u8]).unwrap(),
                [0xAA, 0x21, 0x00, 0x03, 0x90, 0x00, i as u8, *chk],
                "colour {i}"
            );
        }
    }

    #[test]
    fn background_color_replies_decode() {
        // `AA FF 00 05 41 21 90 00 <colour> <chk>`, the app's expected answer
        let expected: [u8; 6] = [0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFB];
        for (i, chk) in expected.iter().enumerate() {
            let frame = [
                0xAA, 0xFF, 0x00, 0x05, 0x41, 0x21, 0x90, 0x00, i as u8, *chk,
            ];
            let Decoded::Frame(resp, n) = decode_response(&frame) else {
                panic!("not a frame: {frame:02X?}");
            };
            assert_eq!(n, frame.len());
            assert_eq!(resp.command, cmd::SCREEN);
            assert!(resp.ack);
            assert_eq!(resp.payload, [sub::BG_COLOR, 0x00, i as u8]);
            assert_eq!(bg_color::name(resp.payload[2]), bg_color::name(i as u8));
        }
        // A short reply (no colour byte) is an error, not a wrong colour
        let f = encode_response(&Response {
            display_id: 0,
            ack: true,
            command: cmd::SCREEN,
            payload: vec![sub::BG_COLOR],
        })
        .unwrap();
        let Decoded::Frame(resp, _) = decode_response(&f) else {
            panic!("not a frame");
        };
        assert_eq!(resp.payload, [sub::BG_COLOR]);
    }
}
