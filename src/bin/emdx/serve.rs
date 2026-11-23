//! `emdx serve`: a web UI and JSON API over the same features and saved
//! displays as the CLI (`~/.config/emdx/config.json` is shared).

use std::{
    collections::HashMap,
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::Args;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use emdx_controller::{
    content, discovery, imaging,
    mdc::{self, content_player, Device, SleepTime, StorageKind, DEFAULT_CONNECT_TIMEOUT},
    schedule,
    thumbnail::{self, Endpoint},
    wol,
};

use crate::{
    config::{self, CliConfig, DisplayEntry, Target},
    saved_slideshow, settings,
};

static INDEX_HTML: &str = include_str!("ui/index.html");
static APP_JS: &str = include_str!("ui/app.js");
static STYLE_CSS: &str = include_str!("ui/style.css");

#[derive(Args, Debug)]
pub(crate) struct ServeArgs {
    /// Port to listen on
    #[arg(long, default_value_t = 3001, env = "EMDX_PORT")]
    pub port: u16,
    /// Address to listen on (0.0.0.0: every interface)
    #[arg(long, default_value = "0.0.0.0", env = "EMDX_BIND")]
    pub bind: String,
    /// Seconds between background Wi-Fi scans, for the setup dialog's network list
    /// (0: only when asked)
    #[arg(long, default_value_t = 120, env = "EMDX_WIFI_SCAN_EVERY")]
    pub wifi_scan_every: u64,
}

#[derive(Clone)]
struct App {
    config_path: PathBuf,
    /// One MDC session at a time per display (the EM32DX allows a single one)
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Where thumbnails the displays POST back are kept
    thumb_dir: PathBuf,
    /// The receiver's callback URL and the certificate to push before asking. The
    /// receiver itself belongs to [`drain_uploads`], which waits on it all the time.
    callback: Arc<Callback>,
    gallery: Gallery,
    /// Panel serial per saved display name, learned from its status or a thumbnail
    serials: Arc<Mutex<HashMap<String, String>>>,
    /// Whether a saved display is in portrait, as last read or set: a display posts
    /// thumbnails on its own too, and they are turned upright from this
    portraits: Arc<Mutex<HashMap<String, bool>>>,
    /// Tells open pages (`GET /api/events`) when a thumbnail arrives
    events: tokio::sync::broadcast::Sender<Value>,
    /// The latest Wi-Fi networks around this computer, scanned in the background
    wifi: Arc<WifiCache>,
}

/// The Wi-Fi list, kept up to date by [`scan_wifi_in_background`]
#[derive(Default)]
struct WifiCache {
    latest: Mutex<Option<WifiScan>>,
    /// Held while a scan runs: a second caller waits for it instead of scanning again
    scanning: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
struct WifiScan {
    result: Result<Vec<crate::wifi::WifiNetwork>, String>,
    at: chrono::DateTime<chrono::Local>,
}

impl WifiCache {
    fn json(&self) -> Value {
        let scanning = self.scanning.try_lock().is_err();
        match self
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            None => json!({ "available": null, "networks": [], "scanning": scanning }),
            Some(WifiScan { result, at }) => {
                let scanned_at = at.to_rfc3339();
                match result {
                    Ok(networks) => json!({
                        "available": true, "networks": networks,
                        "scannedAt": scanned_at, "scanning": scanning,
                    }),
                    Err(reason) => json!({
                        "available": false, "networks": [], "reason": reason,
                        "scannedAt": scanned_at, "scanning": scanning,
                    }),
                }
            }
        }
    }

    /// Scan now, or wait for the scan already running; `true` when a list is known
    async fn refresh(&self) -> bool {
        match self.scanning.try_lock() {
            Ok(_running) => {
                let scanned = tokio::task::spawn_blocking(crate::wifi::scan)
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r)
                    .map_err(|e| format!("{e:#}"));
                if let Err(e) = &scanned {
                    emdx_controller::say!("📶 No Wi-Fi list: {e}");
                }
                let ok = scanned.is_ok();
                *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(WifiScan {
                    result: scanned,
                    at: chrono::Local::now(),
                });
                ok
            }
            // Someone else's scan: its result will do
            Err(_) => {
                drop(self.scanning.lock().await);
                self.latest
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|s| s.result.is_ok())
            }
        }
    }
}

/// Keep the Wi-Fi list fresh: a scan at start, then every `every`. When this
/// computer may not scan at all (e.g. not root on Linux), only every 10 minutes.
fn scan_wifi_in_background(app: App, every: Duration) {
    tokio::spawn(async move {
        loop {
            let ok = app.wifi.refresh().await;
            // Open setup dialogs show the new list
            let _ = app.events.send(json!({ "type": "wifi" }));
            if every.is_zero() {
                break;
            }
            tokio::time::sleep(if ok { every } else { Duration::from_secs(600) }).await;
        }
    });
}

impl App {
    /// Where the saved slideshows are kept (next to the settings)
    fn slideshows(&self) -> PathBuf {
        saved_slideshow::root(&self.config_path)
    }

    fn load(&self) -> Result<CliConfig, ApiError> {
        config::load(&self.config_path).map_err(ApiError::internal)
    }

    fn save(&self, cfg: &CliConfig) -> Result<(), ApiError> {
        config::save(&self.config_path, cfg).map_err(ApiError::internal)
    }

    fn target(&self, name: &str) -> Result<Target, ApiError> {
        self.load()?
            .targets()
            .into_iter()
            .find(|t| t.name.as_deref() == Some(name))
            .ok_or_else(|| ApiError::not_found(format!("unknown display {name:?}")))
    }

    fn host_lock(&self, host: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(host.to_string())
            .or_default()
            .clone()
    }

    async fn lock(&self, host: &str) -> tokio::sync::OwnedMutexGuard<()> {
        self.host_lock(host).lock_owned().await
    }

    /// The display's session, unless a request is using it right now
    fn try_lock(&self, host: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.host_lock(host).try_lock_owned().ok()
    }

    /// The panel serial learned for a saved display, if we ever read it
    fn serial_of(&self, name: &str) -> Option<String> {
        self.serials
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// The saved display's name when there is exactly one of them
    fn sole_display(&self) -> Option<String> {
        let cfg = self.load().ok()?;
        let mut names = cfg.displays.keys();
        let only = names.next()?.clone();
        names.next().is_none().then_some(only)
    }

    fn remember_portrait(&self, name: &str, portrait: bool) {
        self.portraits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), portrait);
    }

    /// The saved display a panel serial belongs to
    fn display_with_serial(&self, serial: &str) -> Option<String> {
        let known = self
            .serials
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(_, s)| s.as_str() == serial)
            .map(|(name, _)| name.clone());
        known.or_else(|| self.sole_display())
    }

    /// Clockwise turn that makes an upload from this panel upright: 90° for portrait,
    /// as the app does. `None` while the display's orientation is unknown.
    fn upright_degrees(&self, serial: &str) -> Option<u16> {
        let name = self.display_with_serial(serial)?;
        self.upright_degrees_of(&name)
    }

    fn upright_degrees_of(&self, name: &str) -> Option<u16> {
        let portrait = *self
            .portraits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)?;
        Some(if portrait { 90 } else { 0 })
    }

    fn remember_serial(&self, name: &str, serial: &str) {
        self.serials
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), serial.to_string());
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Status, message, and a machine-readable `reason` when there is one
/// (`wrong-pin`, `blocked`: the display answered but refused the login)
struct ApiError(StatusCode, String, Option<&'static str>);

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.into(), None)
    }
    fn not_found(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::NOT_FOUND, msg.into(), None)
    }
    fn internal(e: impl Into<anyhow::Error>) -> Self {
        let e = e.into();
        let message = format!("{e:#}");
        match e.downcast_ref::<settings::LoginRefused>().map(|r| r.kind) {
            Some(settings::LoginRefusal::Blocked) => {
                ApiError(StatusCode::LOCKED, message, Some("blocked"))
            }
            Some(settings::LoginRefusal::WrongPin) => {
                ApiError(StatusCode::FORBIDDEN, message, Some("wrong-pin"))
            }
            None => ApiError(StatusCode::BAD_GATEWAY, message, None),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e)
    }
}

impl From<mdc::MdcError> for ApiError {
    fn from(e: mdc::MdcError) -> Self {
        ApiError::internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.1 });
        if let Some(reason) = self.2 {
            body["reason"] = json!(reason);
        }
        (self.0, Json(body)).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn ok(message: impl Into<String>) -> ApiResult {
    Ok(Json(json!({ "ok": true, "message": message.into() })))
}

// ─── Server ──────────────────────────────────────────────────────────────────

pub(crate) async fn run(args: ServeArgs) -> anyhow::Result<()> {
    // Logging is set up by main before we get here.
    let config_path = config::default_path()?;
    let thumb_dir = thumbnail::default_out_dir(&config_path);
    std::fs::create_dir_all(&thumb_dir)
        .with_context(|| format!("creating the thumbnail folder {}", thumb_dir.display()))?;
    let receiver = start_receiver(&args.bind, &thumb_dir).await?;
    let app = App {
        config_path,
        locks: Arc::default(),
        thumb_dir,
        callback: Arc::new(Callback::of(&receiver)),
        gallery: Gallery::default(),
        serials: Arc::default(),
        portraits: Arc::default(),
        events: tokio::sync::broadcast::channel(16).0,
        wifi: Arc::default(),
    };
    emdx_controller::say!("📥 thumbnails: a display uploads to {}", receiver.url);
    drain_uploads(receiver, app.clone());
    scan_wifi_in_background(app.clone(), Duration::from_secs(args.wifi_scan_every));
    let addr: SocketAddr = format!("{}:{}", args.bind, args.port)
        .parse()
        .with_context(|| format!("invalid address {}:{}", args.bind, args.port))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    emdx_controller::say!(
        "🌐 emdx web UI on http://{addr} (displays: {})",
        app.config_path.display()
    );
    axum::serve(listener, router(app)).await?;
    Ok(())
}

/// The display has to reach *us*, so the callback URL carries this machine's LAN address
async fn start_receiver(bind: &str, thumb_dir: &FsPath) -> anyhow::Result<Endpoint> {
    let host = match bind.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) if !v4.is_unspecified() => v4.to_string(),
        _ => emdx_controller::local_ip::get_local_ip(None),
    };
    if host.is_empty() {
        return Err(anyhow::anyhow!(
            "could not work out this machine's LAN address: the displays cannot upload without it"
        ));
    }
    // Like the ePaper app: HTTPS with a certificate pushed to the display before each
    // request (the display never uploaded over plain HTTP), on the app's own port
    // when it is free, otherwise a spare one
    Endpoint::start_secure(&host, thumbnail::SECURE_PORT, thumb_dir).await
}

fn router(app: App) -> Router {
    let r = Router::new()
        .route(
            "/",
            get(|| async { asset(INDEX_HTML, "text/html; charset=utf-8") }),
        )
        .route(
            "/app.js",
            get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/style.css",
            get(|| async { asset(STYLE_CSS, "text/css; charset=utf-8") }),
        )
        .route("/api/displays", get(list_displays).post(add_display))
        .route(
            "/api/displays/{name}",
            axum::routing::patch(edit_display).delete(remove_display),
        )
        .route("/api/displays/{name}/default", post(make_default))
        .route("/api/displays/{name}/status", get(status))
        .route(
            "/api/displays/{name}/settings",
            axum::routing::put(set_settings),
        )
        .route("/api/displays/{name}/wake", post(wake))
        .route("/api/displays/{name}/sleep", post(sleep))
        .route("/api/displays/{name}/image", post(show_image))
        .route("/api/displays/{name}/slideshow", post(show_slideshow))
        .route(
            "/api/displays/{name}/slideshow/saved",
            get(saved_slideshow).put(save_slideshow),
        )
        .route(
            "/api/displays/{name}/slideshow/image/{id}",
            get(saved_slideshow_image),
        )
        .route(
            "/api/displays/{name}/slideshow/send",
            post(send_saved_slideshow),
        )
        .route("/api/displays/{name}/schedule", post(show_schedule))
        .route("/api/displays/{name}/identify", post(identify))
        .route("/api/displays/{name}/refresh-now", post(refresh_now))
        .route("/api/displays/{name}/empty-storage", post(empty_storage))
        .route("/api/displays/{name}/pairing-mode", post(pairing_mode))
        .route("/api/displays/{name}/change-pin", post(change_pin))
        .route(
            "/api/displays/{name}/thumbnail",
            get(last_thumbnail).post(fetch_thumbnail),
        )
        .route("/api/displays/{name}/thumbnail/image", get(thumbnail_image))
        .route("/api/displays/{name}/reset", post(reset))
        .route("/api/displays/{name}/forget", post(forget))
        .route("/api/discover", get(discover))
        .route("/api/features", get(features))
        .route("/api/wifi", get(wifi_networks))
        .route("/api/wifi/scan", post(rescan_wifi))
        .route("/api/events", get(events));
    #[cfg(feature = "ble")]
    let r = r
        .route("/api/ble/scan", get(ble::scan))
        .route("/api/ble/provision", post(ble::provision));
    r.layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(axum::middleware::from_fn(log_api_request))
        .with_state(app)
}

/// One line when an API request arrives and one when it is answered, with how long
/// it took (a request can wait for another one's MDC session to the same display).
async fn log_api_request(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    if !path.starts_with("/api/") {
        return next.run(req).await;
    }
    let method = req.method().clone();
    emdx_controller::say!("→ {method} {path}");
    let started = std::time::Instant::now();
    let response = next.run(req).await;
    emdx_controller::say!(
        "← {method} {path}: {} in {:.1}s",
        response.status().as_u16(),
        started.elapsed().as_secs_f64()
    );
    response
}

/// The UI files are built into the binary: `no-cache` makes the browser check for a
/// newer version every time, so a rebuilt `emdx serve` is picked up on reload
fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// The latest 2.4 GHz Wi-Fi networks around this computer, from the background
/// scan: `available` is `null` before the first scan ends, `false` when this
/// computer may not scan (on Linux: not root, no CAP_NET_ADMIN), and the setup
/// dialog then keeps its free Wi-Fi name field.
async fn wifi_networks(State(app): State<App>) -> ApiResult {
    Ok(Json(app.wifi.json()))
}

/// Scan now (or wait for the running scan), then the same answer as `GET /api/wifi`
async fn rescan_wifi(State(app): State<App>) -> ApiResult {
    app.wifi.refresh().await;
    let _ = app.events.send(json!({ "type": "wifi" }));
    Ok(Json(app.wifi.json()))
}

async fn features() -> Json<Value> {
    Json(json!({ "bluetooth": cfg!(feature = "ble"), "version": env!("CARGO_PKG_VERSION") }))
}

// ─── Saved displays ──────────────────────────────────────────────────────────

async fn list_displays(State(app): State<App>) -> ApiResult {
    let cfg = app.load()?;
    let default = cfg.default_name().map(str::to_string);
    let list: Vec<Value> = cfg
        .displays
        .iter()
        .map(|(name, d)| {
            json!({
                "name": name,
                "host": d.host,
                "mac": d.mac,
                "pin": config::mask_pin(&d.pin),
                "default": default.as_deref() == Some(name),
            })
        })
        .collect();
    Ok(Json(json!(list)))
}

#[derive(Deserialize)]
struct NewDisplay {
    name: String,
    host: String,
    pin: String,
    #[serde(default)]
    mac: Option<String>,
    #[serde(default)]
    make_default: bool,
    /// Check that the display accepts the PIN first (and read its MAC)
    #[serde(default = "yes")]
    verify: bool,
}

fn yes() -> bool {
    true
}

async fn add_display(State(app): State<App>, Json(b): Json<NewDisplay>) -> ApiResult {
    config::validate_name(&b.name).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let host = b.host.trim().to_string();
    if host.is_empty() {
        return Err(ApiError::bad_request("the IP address is required"));
    }
    let mut mac = b.mac.filter(|m| !m.trim().is_empty());
    if b.verify {
        let _g = app.lock(&host).await;
        let mut s = Device::new(&host, &b.pin)
            .connect(DEFAULT_CONNECT_TIMEOUT)
            .await
            .map_err(|e| ApiError::bad_request(format!("could not log in to {host}: {e}")))?;
        if mac.is_none() {
            mac = s.wifi_mac().await.ok();
        }
        s.close().await;
    }
    if mac.is_none() {
        mac = discovery::lookup_mac(&host).await;
    }
    let mut cfg = app.load()?;
    if cfg.displays.contains_key(&b.name) {
        return Err(ApiError::bad_request(format!(
            "a display named {:?} already exists",
            b.name
        )));
    }
    cfg.upsert(
        &b.name,
        DisplayEntry {
            host,
            pin: b.pin,
            mac,
        },
        b.make_default,
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    app.save(&cfg)?;
    ok(format!("Saved \"{}\"", b.name))
}

#[derive(Deserialize)]
struct DisplayEdit {
    #[serde(default)]
    rename: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    pin: Option<String>,
    #[serde(default)]
    mac: Option<String>,
}

async fn edit_display(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(b): Json<DisplayEdit>,
) -> ApiResult {
    let mut cfg = app.load()?;
    let entry = cfg
        .displays
        .get_mut(&name)
        .ok_or_else(|| ApiError::not_found(format!("unknown display {name:?}")))?;
    if let Some(h) = b.host.filter(|h| !h.trim().is_empty()) {
        entry.host = h.trim().to_string();
    }
    if let Some(p) = b.pin.filter(|p| !p.is_empty()) {
        entry.pin = p;
    }
    if let Some(m) = b.mac {
        entry.mac = Some(m.trim().to_string()).filter(|m| !m.is_empty());
    }
    let mut final_name = name.clone();
    if let Some(to) = b.rename.filter(|to| *to != name) {
        cfg.rename(&name, &to)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        final_name = to;
    }
    app.save(&cfg)?;
    if final_name != name {
        // Its saved slideshow goes with it
        let _ = saved_slideshow::rename(&app.slideshows(), &name, &final_name);
    }
    Ok(Json(json!({ "ok": true, "name": final_name })))
}

async fn remove_display(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let mut cfg = app.load()?;
    cfg.remove(&name)
        .map_err(|e| ApiError::not_found(e.to_string()))?;
    app.save(&cfg)?;
    let _ = saved_slideshow::remove(&app.slideshows(), &name);
    ok(format!("Removed \"{name}\""))
}

async fn make_default(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let mut cfg = app.load()?;
    cfg.set_default(&name)
        .map_err(|e| ApiError::not_found(e.to_string()))?;
    app.save(&cfg)?;
    ok(format!("\"{name}\" is now the default display"))
}

async fn discover(State(app): State<App>) -> ApiResult {
    let found = discovery::discover(Duration::from_secs(4), |_| {}).await?;
    let mut cfg = app.load()?;
    // Saved displays the router gave a new address to: follow them by MAC
    let moved = cfg.relocate_by_mac(
        found
            .iter()
            .filter_map(|d| Some((d.mac.as_deref()?, d.host.as_str()))),
    );
    if !moved.is_empty() {
        app.save(&cfg)?;
        for m in &moved {
            emdx_controller::say!(
                "📍 \"{}\" answered at {} (was {}): address updated",
                m.name,
                m.to,
                m.from
            );
        }
    }
    let list: Vec<Value> = found
        .into_iter()
        .map(|d| {
            let saved = cfg
                .displays
                .iter()
                .find(|(_, e)| e.host == d.host)
                .map(|(n, _)| n.clone());
            json!({
                "host": d.host, "name": d.name, "model": d.model,
                "serial": d.serial, "mac": d.mac, "saved": saved,
            })
        })
        .collect();
    Ok(Json(json!({ "displays": list, "moved": moved })))
}

// ─── Status and settings ─────────────────────────────────────────────────────

fn player_key(v: u8) -> &'static str {
    match v {
        content_player::MOBILE_CMS => "mobile",
        content_player::USB => "usb",
        content_player::VXT => "vxt",
        content_player::CUSTOM_APP => "custom-app",
        _ => "unknown",
    }
}

/// `landscape` or `portrait`, as reported by the display
fn orientation_key(portrait: bool) -> &'static str {
    if portrait {
        "portrait"
    } else {
        "landscape"
    }
}

/// `true` for `portrait`
fn parse_orientation(v: &str) -> Option<bool> {
    match v {
        "landscape" => Some(false),
        "portrait" => Some(true),
        _ => None,
    }
}

fn sleep_key(t: SleepTime) -> String {
    match t {
        SleepTime::Off => "off".into(),
        SleepTime::Minutes(m) => m.to_string(),
        // Minutes, like the presets
        SleepTime::Custom(v) => v.to_string(),
    }
}

async fn status(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    // Sequential probes: the EM32DX drops replies to back-to-back sessions
    let mut s = settings::connect(&t).await?;
    // connect() may have relocated the display to a new address (DHCP): report the
    // current one and tell open pages to refresh their list
    let host = app
        .target(&name)
        .map(|c| c.host)
        .unwrap_or_else(|_| t.host.clone());
    if host != t.host {
        let _ = app.events.send(json!({ "type": "displays" }));
    }
    let battery = s.battery().await.ok();
    let device_name = s.device_name().await.ok();
    let serial = s.serial_number().await.ok();
    if let Some(serial) = &serial {
        app.remember_serial(&name, serial);
    }
    let software = s.software_version().await.ok();
    let wifi_mac = s.wifi_mac().await.ok();
    let clock = s.clock().await.map(|c| {
        format!(
            "{}-{:02}-{:02} {:02}:{:02}",
            c.year, c.month, c.day, c.hour, c.minute
        )
    });
    let setup_done = s.setup_done().await.ok();
    let terms = s.terms().await.ok().and_then(|t| t.agreed);
    let player = s.content_player().await.ok();
    let portrait = s.portrait().await.ok();
    if let Some(p) = portrait {
        app.remember_portrait(&name, p);
    }
    let background = s.background_color().await.ok();
    let sleep_time = s.sleep_time().await.ok();
    let refresh = s.daily_refresh().await.ok();
    let standby = s.network_standby().await;
    let immediate_sleep = s.immediate_sleep().await.ok();
    let features = s.features().await.ok();
    let info: Vec<Value> = s
        .contact_info()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(code, value)| {
            json!({
                "label": mdc::contact_info_label(code)
                    .map_or_else(|| format!("0x{code:04X}"), str::to_string),
                "value": value,
            })
        })
        .collect();
    s.close().await;

    Ok(Json(json!({
        "name": name,
        "host": host,
        "battery": battery,
        "deviceName": device_name,
        "serial": serial,
        "software": software,
        "wifiMac": wifi_mac,
        "clock": clock,
        "setupDone": setup_done,
        "termsAccepted": terms,
        "settings": {
            "contentPlayer": player.map(player_key),
            "orientation": portrait.map(orientation_key),
            "backgroundColor": background.map(mdc::bg_color::name),
            "sleepTime": sleep_time.map(sleep_key),
            "dailyRefresh": refresh.map(|r| format!("{:02}:{:02}", r.hour, r.minute)),
            "networkStandby": standby,
            "sleepAfterUpdate": immediate_sleep,
            "sleepMode": sleep_time
                .zip(standby)
                .map(|(t, on)| settings::SleepMode::of(t, on).key()),
        },
        "features": features.map(|f| json!({
            "lowPowerWifi": f.low_power_wifi,
            "refreshByRefresh": f.refresh_by_refresh,
        })),
        "info": info,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsUpdate {
    content_player: Option<String>,
    orientation: Option<String>,
    /// `white`, `black`, `red`, `green`, `blue` or `yellow`
    background_color: Option<String>,
    sleep_time: Option<String>,
    daily_refresh: Option<String>,
    network_standby: Option<bool>,
    /// `sleep`, `deep-sleep` or `off` (sets the sleep time and network standby)
    sleep_mode: Option<String>,
    /// Sleep as soon as new content is shown
    sleep_after_update: Option<bool>,
}

async fn set_settings(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(b): Json<SettingsUpdate>,
) -> ApiResult {
    // Validate everything before talking to the display
    let player = b
        .content_player
        .as_deref()
        .map(|p| match p {
            "mobile" => Ok(content_player::MOBILE_CMS),
            "usb" => Ok(content_player::USB),
            "vxt" => Ok(content_player::VXT),
            "custom-app" => Ok(content_player::CUSTOM_APP),
            other => Err(ApiError::bad_request(format!(
                "unknown content player {other:?}"
            ))),
        })
        .transpose()?;
    let portrait = b
        .orientation
        .as_deref()
        .map(|o| {
            parse_orientation(o).ok_or_else(|| {
                ApiError::bad_request(format!(
                    "unknown orientation {o:?}: use landscape or portrait"
                ))
            })
        })
        .transpose()?;
    let background = b
        .background_color
        .as_deref()
        .map(|c| {
            mdc::bg_color::parse(c).ok_or_else(|| {
                ApiError::bad_request(format!(
                    "unknown background colour {c:?}: use {}",
                    mdc::bg_color::ALL
                        .iter()
                        .map(|(_, n)| *n)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
        })
        .transpose()?;
    let sleep_time = b
        .sleep_time
        .as_deref()
        .map(settings::parse_sleep_time)
        .transpose()
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let refresh = match b.daily_refresh.as_deref() {
        None => None,
        Some(v) => match settings::parse_refresh(v) {
            Ok(Some(at)) => Some(at),
            Ok(None) => return Err(ApiError::bad_request("use the refresh-now action")),
            Err(e) => return Err(ApiError::bad_request(format!("{e:#}"))),
        },
    };

    let sleep_mode = b
        .sleep_mode
        .as_deref()
        .map(settings::SleepMode::parse)
        .transpose()
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    let mut s = settings::connect(&t).await?;
    let mut changed = Vec::new();
    if let Some(v) = player {
        s.set_content_player(v).await?;
        changed.push("content player");
    }
    if let Some(portrait) = portrait {
        s.set_portrait(portrait).await?;
        app.remember_portrait(&name, portrait);
        changed.push("orientation");
    }
    if let Some(colour) = background {
        s.set_background_color(colour).await?;
        changed.push("background colour");
    }
    if let Some(v) = sleep_time {
        if v == SleepTime::Off {
            settings::ensure_plugged_in(&mut s).await?;
        }
        s.set_sleep_time(v).await?;
        changed.push("sleep time");
    }
    if let Some(at) = refresh {
        s.set_daily_refresh(Some(at)).await?;
        changed.push("screen protection time");
    }
    if let Some(on) = b.network_standby {
        s.set_network_standby(on).await?;
        changed.push("network standby");
    }
    if let Some(mode) = sleep_mode {
        mode.apply(&mut s).await?;
        changed.push("sleep mode");
    }
    if let Some(on) = b.sleep_after_update {
        s.set_immediate_sleep(on).await?;
        changed.push("sleep after a content update");
    }
    s.close().await;
    if changed.is_empty() {
        return Err(ApiError::bad_request("nothing to change"));
    }
    ok(format!("Updated {}", changed.join(", ")))
}

// ─── Actions ─────────────────────────────────────────────────────────────────

async fn wake(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let t = app.target(&name)?;
    let mac = t
        .mac
        .ok_or_else(|| ApiError::bad_request("no MAC address saved for this display"))?;
    wol::wake(&mac).await?;
    ok(format!("Wake-on-LAN sent to {mac}"))
}

async fn sleep(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    Device::new(&t.host, &t.pin).power_off(false).await?;
    ok("In deep sleep: press its power button to wake it")
}

async fn show_image(
    State(app): State<App>,
    Path(name): Path<String>,
    mut form: Multipart,
) -> ApiResult {
    let mut upload = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        if field.name() == Some("image") {
            let bytes = field
                .bytes()
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            upload = Some(bytes.to_vec());
        }
    }
    let upload = upload.ok_or_else(|| ApiError::bad_request("no image in the upload"))?;
    let jpeg = tokio::task::spawn_blocking(move || {
        imaging::normalize_jpeg(&upload, imaging::QUALITY_DEFAULT)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    crate::push_image(&t, jpeg, "jpg", None).await?;
    ok("Image sent; the display is refreshing")
}

/// A slideshow: `image` parts in the order shown, `interval` in minutes (5 to 1440),
/// optional `name`. Answers once the display has downloaded every image.
async fn show_slideshow(
    State(app): State<App>,
    Path(name): Path<String>,
    mut form: Multipart,
) -> ApiResult {
    let mut images = Vec::new();
    let mut interval = None;
    let mut title = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        let field_name = field.name().unwrap_or_default().to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        match field_name.as_str() {
            "image" => images.push(bytes.to_vec()),
            "interval" => interval = Some(String::from_utf8_lossy(&bytes).trim().to_string()),
            "name" => title = Some(String::from_utf8_lossy(&bytes).trim().to_string()),
            _ => {}
        }
    }
    if images.is_empty() {
        return Err(ApiError::bad_request("a slideshow needs at least 1 image"));
    }
    let interval = interval
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("the interval is required"))
        .and_then(|v| content::SlideInterval::parse(v).map_err(ApiError::bad_request))?;
    let jpegs = tokio::task::spawn_blocking(move || {
        images
            .iter()
            .map(|i| imaging::normalize_jpeg(i, imaging::QUALITY_DEFAULT))
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let count = jpegs.len();
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    let title = title
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "emdx slideshow".into());
    crate::push_slideshow(&t, jpegs, interval, &title, None).await?;
    ok(format!(
        "Slideshow sent: {count} images, each shown for {interval}"
    ))
}

/// A schedule: one `at` field (`HH:MM`) and one `image` part per entry, paired in the
/// order sent; optional `name`. Answers once the display has downloaded every image.
async fn show_schedule(
    State(app): State<App>,
    Path(name): Path<String>,
    mut form: Multipart,
) -> ApiResult {
    let mut images = Vec::new();
    let mut times = Vec::new();
    let mut title = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        let field_name = field.name().unwrap_or_default().to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let text = || String::from_utf8_lossy(&bytes).trim().to_string();
        match field_name.as_str() {
            "image" => images.push(bytes.to_vec()),
            "at" => times.push(text()),
            "name" => title = Some(text()),
            _ => {}
        }
    }
    if times.len() != images.len() {
        return Err(ApiError::bad_request(
            "every start time needs one image, and every image a start time",
        ));
    }
    let raw: Vec<schedule::Entry> = times
        .iter()
        .zip(images)
        .map(|(at, image)| {
            Ok(schedule::Entry {
                start: content::StartTime::parse(at)?,
                image,
            })
        })
        .collect::<Result<_, String>>()
        .map_err(ApiError::bad_request)?;
    schedule::check(&raw).map_err(ApiError::bad_request)?;
    let entries = tokio::task::spawn_blocking(move || {
        raw.into_iter()
            .map(|e| {
                Ok(schedule::Entry {
                    start: e.start,
                    image: imaging::normalize_jpeg(&e.image, imaging::QUALITY_DEFAULT)?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let count = entries.len();
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    let title = title
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "emdx schedule".into());
    crate::push_schedule(&t, entries, &title, None).await?;
    ok(format!("Schedule sent: {count} start times"))
}

/// The saved slideshow, with a URL per image; an empty one when nothing is saved
fn saved_json(name: &str, saved: Option<saved_slideshow::Saved>) -> Value {
    let saved = saved.unwrap_or(saved_slideshow::Saved {
        name: String::new(),
        interval: 60,
        images: Vec::new(),
    });
    json!({
        "name": saved.name,
        "interval": saved.interval,
        "images": saved.images.iter().map(|id| json!({
            "id": id,
            "url": format!("/api/displays/{name}/slideshow/image/{id}"),
        })).collect::<Vec<_>>(),
    })
}

async fn saved_slideshow(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    app.target(&name)?;
    let saved = saved_slideshow::load(&app.slideshows(), &name).map_err(ApiError::internal)?;
    Ok(Json(saved_json(&name, saved)))
}

/// Replace the saved slideshow: `name`, `interval` (minutes, 5 to 1440), then one
/// `slide` part per image in order: a file for a new image, or the text `id:<id>`
/// for one already saved. Nothing is sent to the display.
async fn save_slideshow(
    State(app): State<App>,
    Path(name): Path<String>,
    mut form: Multipart,
) -> ApiResult {
    app.target(&name)?;
    let (mut title, mut interval, mut slides) = (String::new(), None, Vec::new());
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        let field_name = field.name().unwrap_or_default().to_string();
        let is_file = field.file_name().is_some();
        let bytes = field
            .bytes()
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let text = || String::from_utf8_lossy(&bytes).trim().to_string();
        match field_name.as_str() {
            "name" => title = text(),
            "interval" => interval = Some(text()),
            "slide" if is_file => slides.push(saved_slideshow::Slide::New(bytes.to_vec())),
            "slide" => match text().strip_prefix("id:") {
                Some(id) => slides.push(saved_slideshow::Slide::Keep(id.to_string())),
                None => return Err(ApiError::bad_request("a slide is a file or id:<image id>")),
            },
            _ => {}
        }
    }
    let interval = interval
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("the interval is required"))
        .and_then(|v| content::SlideInterval::parse(v).map_err(ApiError::bad_request))?;
    let dir = app.slideshows();
    let display = name.clone();
    let saved = tokio::task::spawn_blocking(move || {
        saved_slideshow::save(&dir, &display, &title, interval, slides)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(saved_json(&name, Some(saved))))
}

async fn saved_slideshow_image(
    State(app): State<App>,
    Path((name, id)): Path<(String, String)>,
) -> Response {
    if let Err(e) = app.target(&name) {
        return e.into_response();
    }
    match saved_slideshow::image(&app.slideshows(), &name, &id) {
        Ok(jpeg) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "max-age=86400"),
            ],
            Bytes::from(jpeg),
        )
            .into_response(),
        Err(_) => ApiError::not_found("no such image in the saved slideshow").into_response(),
    }
}

/// Send the saved slideshow to the display
async fn send_saved_slideshow(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let t = app.target(&name)?;
    let saved = saved_slideshow::load(&app.slideshows(), &name)
        .map_err(ApiError::internal)?
        .filter(|s| !s.images.is_empty())
        .ok_or_else(|| ApiError::bad_request("the slideshow has no image yet"))?;
    let interval =
        content::SlideInterval::from_minutes(saved.interval).map_err(ApiError::bad_request)?;
    let jpegs =
        saved_slideshow::jpegs(&app.slideshows(), &name, &saved).map_err(ApiError::internal)?;
    let count = jpegs.len();
    let title = if saved.name.is_empty() {
        "emdx slideshow".to_string()
    } else {
        saved.name.clone()
    };
    let _g = app.lock(&t.host).await;
    crate::push_slideshow(&t, jpegs, interval, &title, None).await?;
    ok(format!(
        "Slideshow sent: {count} image{}, each shown for {interval}",
        if count == 1 { "" } else { "s" }
    ))
}

async fn identify(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    let jpeg = crate::identification_image(&t, &name, None).await?;
    crate::push_image(&t, jpeg, "jpg", None).await?;
    ok("Identification image sent")
}

async fn refresh_now(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    let mut s = settings::connect(&t).await?;
    s.set_daily_refresh(None).await?;
    s.close().await;
    ok("Refreshing the screen")
}

#[derive(Deserialize)]
struct EmptyStorage {
    what: String,
}

async fn empty_storage(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(b): Json<EmptyStorage>,
) -> ApiResult {
    let kind = match b.what.as_str() {
        "files" => StorageKind::Files,
        "network-certs" => StorageKind::NetworkCertificates,
        "app-certs" => StorageKind::AppCertificates,
        other => return Err(ApiError::bad_request(format!("unknown storage {other:?}"))),
    };
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    let mut s = settings::connect(&t).await?;
    s.empty_storage(kind).await?;
    s.close().await;
    ok("Erased")
}

/// Settings back to default and user content deleted ("Reset All"), then removed
async fn forget(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let t = app.target(&name)?;
    let done = {
        let _g = app.lock(&t.host).await;
        settings::forget_on_display(&t).await?.join(" ")
    };
    let mut cfg = app.load()?;
    cfg.remove(&name)
        .map_err(|e| ApiError::not_found(e.to_string()))?;
    app.save(&cfg)?;
    let _ = saved_slideshow::remove(&app.slideshows(), &name);
    ok(format!("{done} \"{name}\" was removed from emdx."))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangePin {
    new_pin: String,
}

/// Change the PIN on the display, check it, then save it
async fn change_pin(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(b): Json<ChangePin>,
) -> ApiResult {
    let new_pin = b.new_pin.trim().to_string();
    settings::check_new_pin(&new_pin).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let t = app.target(&name)?;
    if new_pin == t.pin {
        return Err(ApiError::bad_request("that is already the display PIN"));
    }
    let _g = app.lock(&t.host).await;
    settings::change_pin_on_display(&t, &new_pin).await?;
    let mut cfg = app.load()?;
    if let Some(e) = cfg.displays.get_mut(&name) {
        e.pin = new_pin;
    }
    app.save(&cfg)?;
    ok("PIN is changed successfully. Remember it: if you forget it, the display has to be reset.")
}

async fn pairing_mode(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    let mut s = settings::connect(&t).await?;
    s.turn_on_ble(2000).await?;
    s.close().await;
    ok("Pairing mode on: the LED should blink blue. Set it up again with \"Set up a new display\".")
}

#[derive(Deserialize)]
struct ResetRequest {
    /// `settings` or `all` (the app's "Reset All")
    kind: String,
}

async fn reset(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(b): Json<ResetRequest>,
) -> ApiResult {
    let kind = match b.kind.as_str() {
        "settings" => mdc::ResetKind::Settings,
        "all" => mdc::ResetKind::All,
        other => return Err(ApiError::bad_request(format!("unknown reset {other:?}"))),
    };
    let t = app.target(&name)?;
    let _g = app.lock(&t.host).await;
    let mut s = settings::connect(&t).await?;
    let confirmed = s.reset(kind).await?;
    s.close().await;
    let what = match kind {
        mdc::ResetKind::Settings => "Settings reset",
        mdc::ResetKind::All => "Factory reset",
    };
    Ok(Json(json!({
        "ok": true,
        "confirmed": confirmed,
        "message": if confirmed {
            format!("{what} sent and acknowledged")
        } else {
            format!("{what} sent (the display restarted before confirming)")
        },
    })))
}

// ─── Bluetooth setup ─────────────────────────────────────────────────────────

#[cfg(feature = "ble")]
mod ble {
    use super::*;
    use emdx_controller::{
        ble::{
            bluez::{self, BluezLink},
            proto,
            session::{BleSession, NetworkPlan},
        },
        provision::{self, Plan, Timing},
    };
    use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};

    pub(super) async fn scan() -> ApiResult {
        let found = bluez::scan(Duration::from_secs(8)).await?;
        let list: Vec<Value> = found
            .into_iter()
            .map(|f| {
                json!({
                    "address": f.address, "name": f.name, "rssi": f.rssi,
                    "setupMode": f.in_setup_mode(),
                    "setUpBefore": f.advert.is_some_and(|a| a.configured),
                })
            })
            .collect();
        Ok(Json(json!(list)))
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct ProvisionRequest {
        name: String,
        #[serde(default)]
        address: Option<String>,
        #[serde(default)]
        pairing_pin: Option<String>,
        /// Wi-Fi network (2.4 GHz); the address is assigned automatically (DHCP)
        #[serde(default)]
        ssid: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        accept_terms: bool,
        /// Decline the terms (also what happens when `acceptTerms` is false)
        #[serde(default)]
        decline_terms: bool,
        #[serde(default)]
        display_pin: Option<String>,
        #[serde(default)]
        device_name: Option<String>,
        /// `landscape` or `portrait`
        #[serde(default)]
        orientation: Option<String>,
        /// `off`, `5`, `10`, `20`, `30`, `60`
        #[serde(default)]
        sleep_time: Option<String>,
        #[serde(default)]
        network_standby: Option<bool>,
        /// `HH:MM`
        #[serde(default)]
        daily_refresh: Option<String>,
    }

    /// Runs the whole setup and streams progress as JSON lines:
    /// `{"msg": …}` …, then `{"done": {name, host}}` or `{"error": …}`.
    pub(super) async fn provision(
        State(app): State<App>,
        Json(req): Json<ProvisionRequest>,
    ) -> Response {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        // The setup flow holds non-Send callbacks across awaits: give it its
        // own thread and runtime
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let result = match rt {
                Ok(rt) => rt.block_on(run(&app, req, &tx)),
                Err(e) => Err(e.into()),
            };
            let _ = tx.send(match result {
                Ok(done) => json!({ "done": done }),
                Err(e) => json!({ "error": format!("{e:#}") }),
            });
        });
        let stream =
            UnboundedReceiverStream::new(rx).map(|v| Ok::<_, Infallible>(format!("{v}\n")));
        (
            [(header::CONTENT_TYPE, "application/x-ndjson")],
            Body::from_stream(stream),
        )
            .into_response()
    }

    async fn run(
        app: &App,
        req: ProvisionRequest,
        tx: &tokio::sync::mpsc::UnboundedSender<Value>,
    ) -> anyhow::Result<Value> {
        let say = |m: &str| {
            emdx_controller::say!("{m}");
            let _ = tx.send(json!({ "msg": m }));
        };
        config::validate_name(&req.name)?;
        if app
            .load()
            .map_err(|e| anyhow::anyhow!(e.1))?
            .displays
            .contains_key(&req.name)
        {
            anyhow::bail!("a display named {:?} already exists", req.name);
        }
        let pin = req
            .pairing_pin
            .clone()
            .filter(|p| !p.is_empty())
            .context("the pairing PIN (under the QR code on the display) is required")?;
        if pin.len() != 6 || !pin.bytes().all(|b| b.is_ascii_digit()) {
            anyhow::bail!("the PIN must be 6 digits");
        }
        // Required: the pairing PIN is only for the setup
        let display_pin = req
            .display_pin
            .clone()
            .filter(|p| !p.is_empty())
            .context("the display PIN (6 digits, asked on every connection) is required")?;
        if display_pin.len() != 6 || !display_pin.bytes().all(|b| b.is_ascii_digit()) {
            anyhow::bail!("the display PIN must be 6 digits");
        }
        if req.accept_terms && req.decline_terms {
            anyhow::bail!("accept or decline the terms, not both");
        }
        let ssid = req
            .ssid
            .clone()
            .filter(|s| !s.is_empty())
            .context("the Wi-Fi network name is required")?;
        let password = req.password.clone().unwrap_or_default();
        proto::wifi_data(&ssid, &password)?;
        let network = NetworkPlan::Wifi { ssid, password };

        let address = match req.address.clone().filter(|a| !a.is_empty()) {
            Some(a) => a,
            None => {
                say("🔍 Looking for displays in Bluetooth setup mode...");
                let found: Vec<_> = bluez::scan(Duration::from_secs(8))
                    .await?
                    .into_iter()
                    .filter(bluez::Found::in_setup_mode)
                    .collect();
                match found.as_slice() {
                    [one] => one.address.clone(),
                    [] => anyhow::bail!(
                        "no display in Bluetooth pairing mode was found. {}",
                        settings::PAIRING_MODE_HELP
                    ),
                    _ => anyhow::bail!("several displays are in setup mode; pick one"),
                }
            }
        };

        let non_empty = |v: &Option<String>| v.clone().filter(|s| !s.trim().is_empty());
        let portrait = match non_empty(&req.orientation).as_deref() {
            None => None,
            Some(o) => Some(parse_orientation(o).with_context(|| {
                format!("unknown orientation {o:?}: use landscape or portrait")
            })?),
        };
        let sleep_time = non_empty(&req.sleep_time)
            .as_deref()
            .map(settings::parse_sleep_time)
            .transpose()?;
        let daily_refresh = match non_empty(&req.daily_refresh) {
            None => None,
            Some(v) => Some(
                settings::parse_refresh(&v)?
                    .context("the screen protection time is a time, HH:MM")?,
            ),
        };
        let plan = Plan {
            pin,
            network,
            // The display is named like in emdx, unless told otherwise
            device_name: Some(
                req.device_name
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| req.name.clone()),
            ),
            new_pin: Some(display_pin),
            settings: provision::SetupSettings {
                portrait,
                sleep_time,
                network_standby: req.network_standby,
                daily_refresh,
            },
        };
        let timing = Timing::default();
        let mut progress = |m: &str| say(m);

        say(&format!("🔗 Connecting to {address} over Bluetooth..."));
        let link =
            BluezLink::connect(&address, plan.pin.parse().ok(), Duration::from_secs(30)).await?;
        let mut session = BleSession::new(link);
        let phase = provision::network_phase(&mut session, &plan, timing, &mut progress).await;
        session.into_link().close().await;
        let net = phase?;
        say(&format!("🌐 The display is on the network at {}", net.ip));

        let accept_flag = req.accept_terms && !req.decline_terms;
        let mut accept = |url: &str| -> anyhow::Result<bool> {
            say(&format!("📄 Terms & conditions: {url}"));
            if !accept_flag {
                say("   Declined.");
            }
            Ok(accept_flag)
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

        let mac = match done.wifi_mac.clone() {
            Some(m) => Some(m),
            None => discovery::lookup_mac(&host).await,
        };
        let mut cfg = config::load(&app.config_path)?;
        let first = cfg.displays.is_empty();
        cfg.upsert(
            &req.name,
            DisplayEntry {
                host: host.clone(),
                pin: done.pin.clone(),
                mac: mac.clone(),
            },
            first,
        )?;
        config::save(&app.config_path, &cfg)?;
        say(&format!("💾 Saved as \"{}\"", req.name));

        let target = Target {
            name: Some(req.name.clone()),
            host: host.clone(),
            pin: done.pin,
            mac,
        };
        let NetworkPlan::Wifi { ssid, .. } = &plan.network;
        let ssid = Some(ssid.as_str());
        say("🖼  Showing the identification image...");
        let shown = async {
            let jpeg = crate::identification_image(&target, &req.name, ssid).await?;
            crate::push_image(&target, jpeg, "jpg", None).await
        }
        .await;
        if let Err(e) = shown {
            say(&format!(
                "⚠️  Could not show the identification image ({e:#})"
            ));
        }
        say(&format!("🎉 Setup complete: {host}"));
        Ok(json!({ "name": req.name, "host": host, "termsAccepted": done.terms_accepted }))
    }
}

// ─── Thumbnails ──────────────────────────────────────────────────────────────

/// How long a display is given to POST its thumbnail back after being asked
const THUMBNAIL_WAIT: Duration = Duration::from_secs(45);

/// What the displays have uploaded, keyed by their serial number. A panel also
/// uploads on its own whenever what it shows changes, so this is filled by a
/// background drain rather than by the request that asked for one.
#[derive(Clone, Default)]
struct Gallery(Arc<Mutex<GalleryInner>>);

#[derive(Default)]
struct GalleryInner {
    /// serial -> (arrival number, upload)
    by_serial: HashMap<String, (u64, thumbnail::Upload)>,
    newest: Option<(u64, thumbnail::Upload)>,
    seen: u64,
}

impl Gallery {
    fn record(&self, upload: thumbnail::Upload) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.seen += 1;
        inner.newest = Some((inner.seen, upload.clone()));
        let at = inner.seen;
        inner
            .by_serial
            .insert(upload.device_id.clone(), (at, upload));
    }

    /// Uploads received so far; a caller compares against it to spot new ones
    fn seen(&self) -> u64 {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).seen
    }

    fn newest_since(&self, since: u64) -> Option<thumbnail::Upload> {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .newest
            .as_ref()
            .filter(|(at, _)| *at > since)
            .map(|(_, upload)| upload.clone())
    }

    /// The upload on record when exactly one panel has ever uploaded
    fn sole(&self) -> Option<thumbnail::Upload> {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.by_serial.len() == 1 {
            inner.newest.clone().map(|(_, upload)| upload)
        } else {
            None
        }
    }

    /// What this panel last uploaded; with `since`, only something newer than that
    fn uploaded_by(&self, serial: &str, since: Option<u64>) -> Option<thumbnail::Upload> {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .by_serial
            .get(serial)
            .filter(|(at, _)| since.is_none_or(|since| *at > since))
            .map(|(_, upload)| upload.clone())
    }
}

#[derive(Clone)]
struct Callback {
    url: String,
    cert_pem: Option<String>,
}

impl Callback {
    fn of(receiver: &Endpoint) -> Self {
        Callback {
            url: receiver.url.clone(),
            cert_pem: receiver.cert_pem.clone(),
        }
    }
}

/// Keep taking uploads: they arrive on their own, and a stale one left in the
/// queue would otherwise answer the next request instantly.
fn drain_uploads(mut receiver: Endpoint, app: App) {
    tokio::spawn(async move {
        loop {
            let Some(mut upload) = receiver.next().await else {
                break;
            };
            emdx_controller::say!(
                "📥 Thumbnail from {}{}",
                upload.device_id,
                match &upload.content_name {
                    Some(name) => format!(" ({name})"),
                    None => String::new(),
                }
            );
            // Upright before anyone sees it, asked for or not
            read_orientation(&app, &upload.device_id).await;
            match app.upright_degrees(&upload.device_id) {
                Some(0) => emdx_controller::say!("   Landscape: kept as sent"),
                Some(degrees) => match upload.rotate_saved(degrees).await {
                    Ok(_) => emdx_controller::say!("🔃 Rotated {degrees}° to show it upright"),
                    Err(e) => emdx_controller::say_err!("⚠️  Couldn't rotate it: {e:#}"),
                },
                None => emdx_controller::say!(
                    "   Orientation of this display not read yet: kept as sent"
                ),
            }
            let serial = upload.device_id.clone();
            app.gallery.record(upload);
            let display = app.display_with_serial(&serial);
            if let Some(name) = &display {
                app.remember_serial(name, &serial);
            }
            // Nobody listening is fine
            let _ = app.events.send(json!({
                "type": "thumbnail",
                "serial": serial,
                "display": display,
            }));
        }
    });
}

/// Server-sent events for the open pages: `thumbnail` when a display uploads one,
/// asked for or not (a display posts one by itself after redrawing)
async fn events(State(app): State<App>) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use tokio_stream::{wrappers::BroadcastStream, StreamExt as _};
    let stream = BroadcastStream::new(app.events.subscribe()).filter_map(|msg| {
        let msg = msg.ok()?;
        let kind = msg["type"].as_str().unwrap_or("message").to_string();
        Some(Ok::<_, std::convert::Infallible>(
            Event::default().event(kind).data(msg.to_string()),
        ))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Read the orientation of the display that just uploaded, so its thumbnail is
/// turned the right way even after a change made elsewhere. When a request holds
/// the display's session it just read or set the orientation itself (and waiting
/// would deadlock "Ask the display", which holds it until the upload is recorded);
/// a display that doesn't answer isn't woken: the last known orientation is used.
async fn read_orientation(app: &App, serial: &str) {
    let Some(name) = app.display_with_serial(serial) else {
        return;
    };
    let Ok(t) = app.target(&name) else {
        return;
    };
    let Some(_g) = app.try_lock(&t.host) else {
        return;
    };
    let read = async {
        let mut s = Device::new(&t.host, &t.pin)
            .connect(Duration::from_secs(5))
            .await?;
        let portrait = s.portrait().await;
        s.close().await;
        portrait
    };
    match read.await {
        Ok(p) => {
            emdx_controller::say!("   {name:?} is in {}", orientation_key(p));
            app.remember_portrait(&name, p);
        }
        Err(e) => emdx_controller::say!(
            "   Couldn't read {name:?}'s orientation ({e:#}): using the last one known"
        ),
    }
}

/// Ask the display to photograph what it is showing, then wait for the upload
async fn fetch_thumbnail(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    let t = app.target(&name)?;
    let Callback { url, cert_pem } = (*app.callback).clone();
    let serial = app.serial_of(&name);
    let _g = app.lock(&t.host).await;
    let since = app.gallery.seen();
    // The session stays open until the upload arrives, like the app's socket
    // Wakes the display with Wake-on-LAN when it doesn't answer
    let mut s = settings::connect(&t).await?;
    let asked: anyhow::Result<_> = async {
        // What the upload needs to be turned upright (see `drain_uploads`)
        if let Ok(p) = s.portrait().await {
            app.remember_portrait(&name, p);
        }
        if serial.is_none() {
            if let Ok(sn) = s.serial_number().await {
                app.remember_serial(&name, &sn);
            }
        }
        if let Some(pem) = &cert_pem {
            s.push_cms_cert(pem).await?;
        }
        let reply = s.request_thumbnail(&url).await?;
        let upload = tokio::time::timeout(THUMBNAIL_WAIT, async {
            loop {
                let found = match &serial {
                    Some(serial) => app.gallery.uploaded_by(serial, Some(since)),
                    None => app.gallery.newest_since(since),
                };
                if let Some(upload) = found {
                    return upload;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        Ok((reply, upload))
    }
    .await;
    s.close().await;
    let (reply, upload) = asked?;
    let upload = upload.map_err(|_| {
        ApiError::internal(anyhow::anyhow!(
            "no thumbnail arrived within {}s: the display has to reach {url} from its own \
             network (check that this machine's firewall lets it in)",
            THUMBNAIL_WAIT.as_secs()
        ))
    })?;
    app.remember_serial(&name, &upload.device_id);
    let relative = saved_relative(&upload)?;
    Ok(Json(json!({
        "ok": true,
        "message": "Received a thumbnail",
        "asked": url,
        "displayReply": reply,
        "thumbnail": thumbnail_json(&upload, &name, &relative),
    })))
}

/// The thumbnail this display last sent us, whenever that was
async fn last_thumbnail(State(app): State<App>, Path(name): Path<String>) -> ApiResult {
    app.target(&name)?;
    let upload = match app.serial_of(&name) {
        Some(serial) => app.gallery.uploaded_by(&serial, None),
        // Nothing ties this name to a panel yet; with one saved display and one
        // panel that has ever uploaded, there is nothing to confuse it with
        None => match app.sole_display() {
            Some(only) if only == name => app.gallery.sole(),
            _ => None,
        },
    }
    .ok_or_else(|| ApiError::not_found("no thumbnail yet: ask the display"))?;
    let relative = saved_relative(&upload)?;
    Ok(Json(thumbnail_json(&upload, &name, &relative)))
}

/// The image itself, served from the thumbnail folder and nothing else
async fn thumbnail_image(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let file = match query.get("file") {
        Some(file) => file,
        None => return ApiError::bad_request("file=<device/name> is required").into_response(),
    };
    if let Err(e) = app.target(&name) {
        return e.into_response();
    }
    let path = match thumbnail::resolve_saved(&app.thumb_dir, file) {
        Some(path) => path,
        None => {
            return ApiError::bad_request(format!("not a saved thumbnail: {file:?}"))
                .into_response()
        }
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => return ApiError::internal(e).into_response(),
    };
    let ctype = match path.extension().and_then(|e| e.to_str()) {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    };
    (
        [
            (header::CONTENT_TYPE, ctype),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Bytes::from(bytes),
    )
        .into_response()
}

fn saved_relative(upload: &thumbnail::Upload) -> Result<String, ApiError> {
    upload.relative_path().ok_or_else(|| {
        ApiError::internal(anyhow::anyhow!("the saved thumbnail has no usable path"))
    })
}

fn thumbnail_json(upload: &thumbnail::Upload, name: &str, relative: &str) -> Value {
    let mut json = upload.metadata_json();
    json["saved"] = json!(relative);
    json["image"] = json!(format!(
        "/api/displays/{name}/thumbnail/image?file={relative}"
    ));
    json
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_logins_are_not_unreachable() {
        let blocked = ApiError::from(settings::login_failed("Blocked"));
        assert_eq!(
            (blocked.0, blocked.2),
            (StatusCode::LOCKED, Some("blocked"))
        );
        // Context added on the way keeps the reason
        let wrong = ApiError::from(settings::login_failed("Incorrect PIN").context("status"));
        assert_eq!(
            (wrong.0, wrong.2),
            (StatusCode::FORBIDDEN, Some("wrong-pin"))
        );
        let other = ApiError::from(anyhow::anyhow!("No route to host"));
        assert_eq!((other.0, other.2), (StatusCode::BAD_GATEWAY, None));
    }

    async fn start() -> (String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let thumb_dir = dir.path().join("thumbnails");
        let app = App {
            config_path: dir.path().join("config.json"),
            locks: Arc::default(),
            thumb_dir: thumb_dir.clone(),
            callback: Arc::new(Callback::of(
                &Endpoint::start("127.0.0.1", 0, thumb_dir).await.unwrap(),
            )),
            gallery: Gallery::default(),
            serials: Arc::default(),
            portraits: Arc::default(),
            events: tokio::sync::broadcast::channel(16).0,
            wifi: Arc::default(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });
        (url, dir)
    }

    #[test]
    fn a_stale_thumbnail_does_not_answer_the_next_request() {
        let gallery = Gallery::default();
        let upload = |serial: &str| thumbnail::Upload {
            device_id: serial.to_string(),
            ..Default::default()
        };
        assert_eq!(gallery.seen(), 0);
        gallery.record(upload("SN1"));
        let since = gallery.seen();
        gallery.record(upload("SN2"));
        // Something newer arrived, and it is attributed to the panel that sent it
        assert_eq!(gallery.newest_since(since).unwrap().device_id, "SN2");
        assert_eq!(
            gallery.uploaded_by("SN2", Some(since)).unwrap().device_id,
            "SN2"
        );
        assert!(
            gallery.uploaded_by("SN1", Some(since)).is_none(),
            "SN1 sent nothing new"
        );
        assert!(gallery.uploaded_by("nope", None).is_none());
        // Without a "since", whatever is on record is good enough
        assert_eq!(gallery.uploaded_by("SN1", None).unwrap().device_id, "SN1");
        // A sole upload can be used when nothing identifies the panel
        let one = Gallery::default();
        one.record(upload("SN1"));
        assert_eq!(one.sole().unwrap().device_id, "SN1");
        assert!(gallery.sole().is_none(), "two panels have uploaded");
    }

    #[tokio::test]
    async fn thumbnails_are_asked_for_and_served_back() {
        let (url, dir) = start().await;
        let c = reqwest::Client::new();
        c.post(format!("{url}/api/displays"))
            .json(
                &json!({ "name": "lobby", "host": "192.0.2.10", "pin": "123456", "verify": false }),
            )
            .send()
            .await
            .unwrap();

        let r = c
            .get(format!("{url}/api/displays/lobby/thumbnail"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "nothing received yet");
        assert_eq!(
            c.get(format!("{url}/api/displays/nope/thumbnail"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );

        // A thumbnail already on disk is served, and nothing outside that folder is
        std::fs::create_dir_all(dir.path().join("thumbnails/SN123")).unwrap();
        std::fs::write(
            dir.path().join("thumbnails/SN123/thumbnail_x.jpg"),
            b"jpeg-bytes",
        )
        .unwrap();
        let r = c
            .get(format!(
                "{url}/api/displays/lobby/thumbnail/image?file=SN123/thumbnail_x.jpg"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-type"], "image/jpeg");
        assert_eq!(r.text().await.unwrap(), "jpeg-bytes");
        for bad in [
            "../config.json",
            "SN123/../../config.json",
            "SN123",
            "SN123/other.png",
            "..%2F..",
        ] {
            let r = c
                .get(format!(
                    "{url}/api/displays/lobby/thumbnail/image?file={bad}"
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{bad}");
        }
        assert_eq!(
            c.get(format!("{url}/api/displays/lobby/thumbnail/image"))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }

    #[tokio::test]
    async fn serves_the_ui() {
        let (url, _dir) = start().await;
        let page = reqwest::get(format!("{url}/")).await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("/app.js"));
        let js = reqwest::get(format!("{url}/app.js")).await.unwrap();
        assert!(js.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/javascript"));
    }

    #[tokio::test]
    async fn manages_saved_displays_in_the_cli_config() {
        let (url, dir) = start().await;
        let c = reqwest::Client::new();
        let add = |name: &str| {
            c.post(format!("{url}/api/displays"))
                .json(
                    &json!({ "name": name, "host": "192.0.2.10", "pin": "123456",
                               "mac": "AA:BB:CC:DD:EE:FF", "verify": false }),
                )
                .send()
        };
        assert_eq!(add("lobby").await.unwrap().status(), 200);
        assert_eq!(add("hall").await.unwrap().status(), 200);
        assert_eq!(add("lobby").await.unwrap().status(), 400, "duplicate");
        assert_eq!(add("bad name").await.unwrap().status(), 400, "invalid name");

        let list: Value = c
            .get(format!("{url}/api/displays"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list.as_array().unwrap().len(), 2);
        assert_eq!(list[1]["name"], "lobby");
        assert_eq!(
            list[1]["default"], true,
            "the first saved display is the default"
        );
        assert_eq!(list[1]["pin"], "1****6", "PINs are masked");

        let r = c
            .patch(format!("{url}/api/displays/lobby"))
            .json(&json!({ "rename": "entrance", "host": "192.0.2.11" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            c.post(format!("{url}/api/displays/hall/default"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            c.delete(format!("{url}/api/displays/entrance"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            c.delete(format!("{url}/api/displays/entrance"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );

        // Same file the CLI reads
        let cfg = config::load(&dir.path().join("config.json")).unwrap();
        assert_eq!(cfg.names(), ["hall"]);
        assert_eq!(cfg.default_name(), Some("hall"));
    }

    #[tokio::test]
    async fn forget_keeps_a_display_it_cannot_reach() {
        let (url, dir) = start().await;
        let c = reqwest::Client::new();
        // Nothing listens on MDC there: the connection is refused at once
        c.post(format!("{url}/api/displays"))
            .json(&json!({ "name": "gone", "host": "127.0.0.1", "pin": "123456", "verify": false }))
            .send()
            .await
            .unwrap();
        let r = c
            .post(format!("{url}/api/displays/gone/forget"))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_server_error(), "{}", r.status());
        let cfg = config::load(&dir.path().join("config.json")).unwrap();
        assert_eq!(
            cfg.names(),
            ["gone"],
            "not removed when the display wasn't changed"
        );
        // "Remove from emdx anyway" is the plain delete
        assert_eq!(
            c.delete(format!("{url}/api/displays/gone"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }

    #[tokio::test]
    async fn saved_slideshows_round_trip() {
        let (url, dir) = start().await;
        let c = reqwest::Client::new();
        c.post(format!("{url}/api/displays"))
            .json(&json!({ "name": "d", "host": "192.0.2.10", "pin": "123456", "verify": false }))
            .send()
            .await
            .unwrap();
        let empty: Value = c
            .get(format!("{url}/api/displays/d/slideshow/saved"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(empty["images"], json!([]));
        let r = c
            .post(format!("{url}/api/displays/d/slideshow/send"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "nothing to send yet");

        let png = || {
            let mut out = std::io::Cursor::new(Vec::new());
            image::RgbImage::new(4, 4)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            reqwest::multipart::Part::bytes(out.into_inner()).file_name("a.png")
        };
        let form = reqwest::multipart::Form::new()
            .text("name", "Menu")
            .text("interval", "90")
            .part("slide", png())
            .part("slide", png());
        let saved: Value = c
            .put(format!("{url}/api/displays/d/slideshow/saved"))
            .multipart(form)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            (saved["name"].as_str(), saved["interval"].as_u64()),
            (Some("Menu"), Some(90))
        );
        let ids: Vec<String> = saved["images"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(dir.path().join("slideshows/d/slideshow.json").is_file());

        // Reorder by id, the image is served, and it reads back the same
        let form = reqwest::multipart::Form::new()
            .text("name", "Menu")
            .text("interval", "90")
            .text("slide", format!("id:{}", ids[1]))
            .text("slide", format!("id:{}", ids[0]));
        let again: Value = c
            .put(format!("{url}/api/displays/d/slideshow/saved"))
            .multipart(form)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(again["images"][0]["id"], ids[1]);
        let img = c
            .get(format!(
                "{url}{}",
                again["images"][0]["url"].as_str().unwrap()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(img.headers()["content-type"], "image/jpeg");
        let read: Value = c
            .get(format!("{url}/api/displays/d/slideshow/saved"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(read, again);

        // Bad input is refused, the saved one is kept
        let form = reqwest::multipart::Form::new()
            .text("interval", "4")
            .part("slide", png());
        let r = c
            .put(format!("{url}/api/displays/d/slideshow/saved"))
            .multipart(form)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let r = c
            .get(format!("{url}/api/displays/d/slideshow/image/..%2Fconfig"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn rejects_bad_slideshows_before_connecting() {
        let (url, _dir) = start().await;
        let c = reqwest::Client::new();
        c.post(format!("{url}/api/displays"))
            .json(&json!({ "name": "d", "host": "192.0.2.10", "pin": "123456", "verify": false }))
            .send()
            .await
            .unwrap();
        let png = || {
            let mut out = std::io::Cursor::new(Vec::new());
            image::RgbImage::new(4, 4)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            reqwest::multipart::Part::bytes(out.into_inner()).file_name("a.png")
        };
        for (images, interval) in [(0, "30"), (2, "4"), (2, "24:01"), (2, "soon")] {
            let mut form = reqwest::multipart::Form::new().text("interval", interval);
            for _ in 0..images {
                form = form.part("image", png());
            }
            let r = c
                .post(format!("{url}/api/displays/d/slideshow"))
                .multipart(form)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{images} images every {interval}");
        }
    }

    #[tokio::test]
    async fn rejects_bad_schedules_before_connecting() {
        let (url, _dir) = start().await;
        let c = reqwest::Client::new();
        c.post(format!("{url}/api/displays"))
            .json(&json!({ "name": "d", "host": "192.0.2.10", "pin": "123456", "verify": false }))
            .send()
            .await
            .unwrap();
        let png = || {
            let mut out = std::io::Cursor::new(Vec::new());
            image::RgbImage::new(4, 4)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            reqwest::multipart::Part::bytes(out.into_inner()).file_name("a.png")
        };
        for (times, images, why) in [
            (vec![], 0, "no entries"),
            (vec!["8:00", "08:00"], 2, "same time twice"),
            (vec!["25:00"], 1, "bad time"),
            (vec!["8:00"], 0, "a time without an image"),
            (vec!["8:00"], 2, "an image without a time"),
        ] {
            let mut form = reqwest::multipart::Form::new();
            for t in times {
                form = form.text("at", t);
            }
            for _ in 0..images {
                form = form.part("image", png());
            }
            let r = c
                .post(format!("{url}/api/displays/d/schedule"))
                .multipart(form)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{why}");
        }
    }

    #[tokio::test]
    async fn rejects_bad_settings_before_connecting() {
        let (url, _dir) = start().await;
        let c = reqwest::Client::new();
        c.post(format!("{url}/api/displays"))
            .json(&json!({ "name": "d", "host": "192.0.2.10", "pin": "123456", "verify": false }))
            .send()
            .await
            .unwrap();
        let r = c
            .post(format!("{url}/api/displays/d/reset"))
            .json(&json!({ "kind": "everything" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            400,
            "unknown reset kind is refused before connecting"
        );
        let r = c
            .post(format!("{url}/api/displays/d/reset"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_client_error(), "the kind is required");
        for body in [
            json!({ "sleepTime": "4" }),
            json!({ "sleepTime": "24:00" }),
            json!({ "contentPlayer": "tv" }),
            json!({ "orientation": "landscape-upside-down" }),
            json!({ "backgroundColor": "pink" }),
            json!({ "backgroundColor": "9" }),
            json!({ "dailyRefresh": "25:00" }),
        ] {
            let r = c
                .put(format!("{url}/api/displays/d/settings"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{body}");
        }
        let r = c
            .put(format!("{url}/api/displays/nope/settings"))
            .json(&json!({ "orientation": "portrait" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }
}
