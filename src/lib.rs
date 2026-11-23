//! Samsung EMDX e-paper display control.
//!
//! - [`mdc`]: the Samsung MDC protocol (TCP 1515 → TLS → PIN auth → framed commands)
//! - [`ble`]: Bluetooth LE first-time setup (protocol reverse engineered from the ePaper app)
//! - [`cms`]: sending content like the app's embedded server (HTTPS, manifest, images, progress)
//! - [`provision`]: the setup flow (Bluetooth network phase, then MDC)
//! - [`discovery`]: SSDP discovery of EMDX displays on the LAN
//! - [`thumbnail`]: the `0xC7/0x54` thumbnail flow (a callback URL, then the display POSTs)
//! - [`slideshow`]: several images shown in turn (the app's playlists)
//! - [`schedule`]: different content at set times of day (the app's schedules)
//! - [`wol`]: Wake-on-LAN magic packets
//! - [`log`]: progress lines ([`say!`], [`say_err!`]) and logcat-style logging
//! - [`placeholder`]: the identification image shown after setup

pub mod ble;
pub mod cms;
pub mod content;
pub mod discovery;
pub mod imaging;
pub mod local_ip;
pub mod log;
pub mod mdc;
pub mod placeholder;
pub mod provision;
pub mod schedule;
pub mod slideshow;
pub mod thumbnail;
pub mod wol;
