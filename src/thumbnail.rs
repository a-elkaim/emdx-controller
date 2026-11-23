//! The thumbnail flow of the ePaper app (MDC `0xC7` / sub `0x54`).
//!
//! The phone never pulls a thumbnail. It sends the display a **callback URL**,
//! and the display later `POST`s a `multipart/form-data` upload to it:
//!
//! ```text
//! POST /device-thumbnail
//!   device_id, content_id, content_name, content_type, content_player_type,
//!   thumbnail_timestamp, schedule_downloaded_timestamp,
//!   and one file part whose *name* carries the image extension
//! ```
//!
//! [`Endpoint`] runs that receiver and hands the parsed [`Upload`] back to the
//! caller. The protocol was reverse-engineered from the ePaper app.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    extract::{multipart::MultipartRejection, DefaultBodyLimit, Multipart, State},
    http::{Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
    serve::{Listener, ListenerExt as _},
    Router,
};
use chrono::Datelike as _;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::json;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_rustls::{server::TlsStream, TlsAcceptor};

/// The path the ePaper app registers (`ConfigWebServerRoutingKt`).
pub const CALLBACK_PATH: &str = "/device-thumbnail";
/// The app's embedded server ports: `NON_SECURITY_PORT` / `SECURITY_PORT`.
pub const NON_SECURE_PORT: u16 = 6868;
/// Used when the app runs with "Secure Mobile CMS" (needs the cert command `0xD2/0x20`).
pub const SECURE_PORT: u16 = 6869;

/// Every saved thumbnail is named `thumbnail_<uuid>.<ext>`, like the app's own files.
const SAVED_PREFIX: &str = "thumbnail_";

/// Refuse absurd uploads; a panel's JPEG is well under this.
pub const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

/// The URL to put in the MDC `0xC7/0x54` payload (`NettyEmbeddedServer.Companion.c()`).
pub fn callback_url(local_ip: &str, port: u16, secure: bool) -> String {
    let scheme = if secure { "https" } else { "http" };
    format!("{scheme}://{local_ip}:{port}{CALLBACK_PATH}")
}

/// How long a display gets to finish the TLS handshake with the HTTPS receiver.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// The self-signed certificate of the HTTPS receiver, like the app's
/// `SelfSignedCertificateGenerator` (`CN=ePaper CMS Server`, SANs `0.0.0.0`,
/// `localhost`; we add our LAN address too). The display is handed its PEM with
/// MDC `0xD2/0x20` ([`crate::mdc::Session::push_cms_cert`]) before it uploads.
///
/// The app's key is RSA-4096; `ring` can only generate ECDSA keys, so this one is P-256.
#[derive(Clone)]
pub struct CmsCert {
    /// `-----BEGIN CERTIFICATE-----\n…\n-----END CERTIFICATE-----`, 64-char lines,
    /// `\n` line ends and no trailing newline, exactly like the app's `d()`.
    pub pem: String,
    config: Arc<rustls::ServerConfig>,
}

impl CmsCert {
    /// The one certificate of this process, made on first use. Like the app, whose
    /// single server answers for content and thumbnails alike: the display keeps one
    /// `CMSServer.crt`, so every server here must present the same one.
    pub fn shared(local_ip: &str) -> Result<Self> {
        static SHARED: std::sync::Mutex<Option<CmsCert>> = std::sync::Mutex::new(None);
        let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cert) = shared.as_ref() {
            return Ok(cert.clone());
        }
        let cert = Self::generate(local_ip)?;
        *shared = Some(cert.clone());
        Ok(cert)
    }

    pub fn generate(local_ip: &str) -> Result<Self> {
        let mut names = vec!["0.0.0.0".to_string(), "localhost".to_string()];
        if !local_ip.is_empty() && !names.iter().any(|n| n == local_ip) {
            names.push(local_ip.to_string());
        }
        let mut params = rcgen::CertificateParams::new(names)?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "ePaper CMS Server");
        let key = rcgen::KeyPair::generate()?;
        let cert = params.self_signed(&key)?;
        let pem = cert.pem().replace("\r\n", "\n").trim_end().to_string();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)?;
        Ok(Self {
            pem,
            config: Arc::new(config),
        })
    }
}

/// TCP + TLS, so the receiver can serve `https://…:6869` like the app's Netty server.
/// A failed handshake is reported (a display refusing our certificate shows up here).
pub(crate) struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    /// Which server this is, for the connection lines
    what: &'static str,
}

impl TlsListener {
    pub(crate) fn new(tcp: TcpListener, cert: &CmsCert, what: &'static str) -> Self {
        Self {
            tcp,
            acceptor: TlsAcceptor::from(cert.config.clone()),
            what,
        }
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (tcp, addr) = Listener::accept(&mut self.tcp).await;
            crate::say!("🔌 {addr} connected to the {} (TLS)", self.what);
            match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, self.acceptor.accept(tcp)).await {
                Ok(Ok(tls)) => return (tls, addr),
                Ok(Err(e)) => crate::say_err!("⚠️  TLS handshake with {addr} failed: {e}"),
                Err(_) => crate::say_err!("⚠️  TLS handshake with {addr} timed out"),
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// One `POST /device-thumbnail` upload.
#[derive(Debug, Clone, Default)]
pub struct Upload {
    /// The display's serial number.
    pub device_id: String,
    pub content_id: Option<String>,
    pub content_name: Option<String>,
    pub content_type: Option<String>,
    pub content_player_type: Option<String>,
    pub thumbnail_timestamp: Option<String>,
    /// Sent as `HH:mm MM/dd` by the display.
    pub schedule_downloaded_timestamp: Option<String>,
    /// Multipart name of the image part (`thumbnail.jpg` in the app's own logs).
    pub part_name: String,
    /// `filename="…"` of that part, when the display sets one.
    pub file_name: Option<String>,
    /// `Content-Type` of that part, when the display sets one.
    pub part_content_type: Option<String>,
    pub bytes: Vec<u8>,
    /// Set by [`Upload::save`]: where this upload was written.
    pub saved: Option<PathBuf>,
    /// Parts we don't know about yet — handy while reverse engineering a real panel.
    pub extra: BTreeMap<String, String>,
}

impl Upload {
    /// Extension of the image, taken from `filename=` first, then from the part name
    /// (the app takes it from the part name, last `.` wins).
    pub fn extension(&self) -> String {
        for candidate in [self.file_name.as_deref(), Some(self.part_name.as_str())] {
            let Some(c) = candidate else { continue };
            if let Some((_, ext)) = c.rsplit_once('.') {
                let ext = ext.trim().to_ascii_lowercase();
                if !ext.is_empty()
                    && ext.len() <= 5
                    && ext.bytes().all(|b| b.is_ascii_alphanumeric())
                {
                    return ext;
                }
            }
        }
        "jpg".to_string()
    }

    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// `HH:mm MM/dd`, the format the app parses with.
    pub fn schedule_time(&self) -> Option<chrono::NaiveDateTime> {
        self.schedule_downloaded_timestamp
            .as_deref()
            .and_then(parse_schedule_timestamp)
    }

    /// `<out_dir>/<device_id>/thumbnail_<uuid>.<ext>` — the app's layout
    /// (`ServerDatabaseHelper.o(deviceId)` + `thumbnail_<uuid>.<ext>`).
    pub fn path_in(&self, out_dir: &Path) -> PathBuf {
        let dir = self.device_dir(out_dir);
        dir.join(format!(
            "{SAVED_PREFIX}{}.{}",
            uuid::Uuid::new_v4(),
            self.extension()
        ))
    }

    pub fn device_dir(&self, out_dir: &Path) -> PathBuf {
        out_dir.join(sanitize_component(&self.device_id))
    }

    /// Save the image and remember the path. The app does the same, then rotates the
    /// file in place for portrait panels (`CoreImage.imageRotation90`).
    pub async fn save(&mut self, out_dir: &Path) -> Result<PathBuf> {
        let path = self.path_in(out_dir);
        tokio::fs::create_dir_all(path.parent().context("the thumbnail path has no parent")?)
            .await
            .with_context(|| format!("creating {}", path.parent().unwrap().display()))?;
        tokio::fs::write(&path, &self.bytes)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        self.saved = Some(path.clone());
        Ok(path)
    }

    /// Turn the saved image `degrees` clockwise (0, 90, 180 or 270), as the app does
    /// for portrait panels. It is rewritten as a JPEG, so a file saved under another
    /// extension moves to `.jpg`; [`Upload::saved`] follows it.
    pub async fn rotate_saved(&mut self, degrees: u16) -> Result<PathBuf> {
        let path = self.saved.clone().context("the upload was not saved")?;
        if degrees % 360 == 0 {
            return Ok(path);
        }
        let bytes = self.bytes.clone();
        let rotated = tokio::task::spawn_blocking(move || {
            crate::imaging::rotate_jpeg(&bytes, degrees % 360, crate::imaging::QUALITY_DEFAULT)
        })
        .await??;
        let new_path = path.with_extension("jpg");
        tokio::fs::write(&new_path, &rotated)
            .await
            .with_context(|| format!("writing {}", new_path.display()))?;
        if new_path != path {
            let _ = tokio::fs::remove_file(&path).await;
        }
        self.bytes = rotated;
        self.saved = Some(new_path.clone());
        Ok(new_path)
    }

    /// `device-dir/file` of the saved file, ready to put in a URL
    pub fn relative_path(&self) -> Option<String> {
        self.saved.as_deref().and_then(saved_relative)
    }

    /// The metadata as JSON (for the web API and for saving next to the image).
    pub fn metadata_json(&self) -> serde_json::Value {
        json!({
            "deviceId": self.device_id,
            "contentId": self.content_id,
            "contentName": self.content_name,
            "contentType": self.content_type,
            "contentPlayerType": self.content_player_type,
            "thumbnailTimestamp": self.thumbnail_timestamp,
            "scheduleDownloadedTimestamp": self.schedule_downloaded_timestamp,
            "partName": self.part_name,
            "fileName": self.file_name,
            "partContentType": self.part_content_type,
            "bytes": self.bytes.len(),
            "extraParts": self.extra,
        })
    }

    /// Human-readable summary lines (for the CLI).
    pub fn summary(&self) -> Vec<String> {
        let mut lines = Vec::new();
        macro_rules! push {
            ($label:literal, $value:expr) => {
                if let Some(v) = $value {
                    lines.push(format!("{:<24}{}", $label, v));
                }
            };
        }
        push!("Device", Some(self.device_id.as_str()));
        push!("Content", self.content_name.clone());
        push!("Content id", self.content_id.clone());
        push!("Content type", self.content_type.clone());
        push!("Player type", self.content_player_type.clone());
        push!("Thumbnail stamp", self.thumbnail_timestamp.clone());
        push!(
            "Content downloaded",
            self.schedule_time()
                .map(|t| t.format("%H:%M %m/%d").to_string())
                .or_else(|| self.schedule_downloaded_timestamp.clone())
        );
        push!("Multipart part", Some(self.part_name.as_str()));
        push!("Filename", self.file_name.clone());
        push!("Part content type", self.part_content_type.clone());
        lines.push(format!("{:<24}{} bytes", "Image", self.bytes.len()));
        for (name, value) in &self.extra {
            lines.push(format!(
                "{:<24}{value}  (unknown part)",
                format!("Part {name}")
            ));
        }
        lines
    }
}

/// The `device-dir/file` form of a saved thumbnail, safe to put in a URL
pub fn saved_relative(path: &Path) -> Option<String> {
    let mut parts = path.components().rev();
    let file = parts.next()?.as_os_str().to_string_lossy().to_string();
    let dir = parts.next()?.as_os_str().to_string_lossy().to_string();
    (is_saved_name(&file) && safe_component(&dir)).then(|| format!("{dir}/{file}"))
}

/// Resolve a `device-dir/file` pair inside `root`, refusing anything else
pub fn resolve_saved(root: &Path, relative: &str) -> Option<PathBuf> {
    let (dir, file) = relative.split_once('/')?;
    if relative.contains('\\') || relative.contains('\0') {
        return None;
    }
    if !safe_component(dir) || !is_saved_name(file) {
        return None;
    }
    let path = root.join(dir).join(file);
    path.is_file().then_some(path)
}

/// Only our own `thumbnail_…` files may be served back
fn is_saved_name(file: &str) -> bool {
    file.starts_with(SAVED_PREFIX) && safe_component(file)
}

fn safe_component(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// `HH:mm MM/dd` (`14:23 06/29`), the app's format; the year is this year.
pub fn parse_schedule_timestamp(text: &str) -> Option<chrono::NaiveDateTime> {
    let year = chrono::Local::now().date_naive().year();
    chrono::NaiveDateTime::parse_from_str(&format!("{year} {text}"), "%Y %H:%M %m/%d").ok()
}

/// Non-alphanumerics become `_`, so a serial can never escape the output directory.
fn sanitize_component(s: &str) -> String {
    let clean: String = s
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let clean = clean.trim_matches('_').to_string();
    if clean.is_empty() {
        "unknown-device".to_string()
    } else {
        clean
    }
}

// ─── Receiver ────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Receiver {
    out_dir: PathBuf,
    tx: mpsc::UnboundedSender<Upload>,
}

/// A running `POST /device-thumbnail` receiver.
pub struct Endpoint {
    pub local_ip: String,
    pub port: u16,
    /// What to put in the MDC `0xC7/0x54` payload.
    pub url: String,
    /// PEM to push with MDC `0xD2/0x20` when the receiver speaks HTTPS.
    pub cert_pem: Option<String>,
    rx: mpsc::UnboundedReceiver<Upload>,
    server: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    /// Bind `port` (falling back to an ephemeral one when it is taken) and serve
    /// [`CALLBACK_PATH`] over plain HTTP.
    pub async fn start(
        local_ip: &str,
        preferred_port: u16,
        out_dir: impl Into<PathBuf>,
    ) -> Result<Self> {
        Self::launch(local_ip, preferred_port, out_dir.into(), None).await
    }

    /// Like [`Endpoint::start`], over HTTPS with this process's [`CmsCert`]: what the app
    /// does by default ("Secure Mobile CMS", port [`SECURE_PORT`]).
    pub async fn start_secure(
        local_ip: &str,
        preferred_port: u16,
        out_dir: impl Into<PathBuf>,
    ) -> Result<Self> {
        let cert = CmsCert::shared(local_ip)?;
        Self::launch(local_ip, preferred_port, out_dir.into(), Some(cert)).await
    }

    async fn launch(
        local_ip: &str,
        preferred_port: u16,
        out_dir: PathBuf,
        cert: Option<CmsCert>,
    ) -> Result<Self> {
        let listener =
            match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], preferred_port))).await {
                Ok(l) => l,
                Err(e) => {
                    crate::say_err!("⚠️  Port {preferred_port} is busy ({e}), using a spare port");
                    TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
                        .await
                        .context("no free port for the thumbnail receiver")?
                }
            };
        let port = listener.local_addr()?.port();
        let (tx, rx) = mpsc::unbounded_channel();
        let receiver = Receiver { out_dir, tx };
        let app = Router::new()
            .route(CALLBACK_PATH, post(handle))
            .fallback(unexpected)
            .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
            .with_state(receiver);
        let secure = cert.is_some();
        let cert_pem = cert.as_ref().map(|c| c.pem.clone());
        let server = match cert {
            Some(cert) => {
                let listener = TlsListener::new(listener, &cert, "thumbnail receiver");
                tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                })
            }
            None => {
                // Say when something connects: tells "nothing reached us" (firewall,
                // display never tried) apart from "it connected but was refused"
                let listener = listener.tap_io(|tcp: &mut TcpStream| {
                    if let Ok(addr) = tcp.peer_addr() {
                        crate::say!("🔌 {addr} connected to the thumbnail receiver");
                    }
                });
                tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                })
            }
        };
        Ok(Self {
            local_ip: local_ip.to_string(),
            port,
            url: callback_url(local_ip, port, secure),
            cert_pem,
            rx,
            server,
        })
    }

    /// Wait up to `timeout` for the display's upload.
    pub async fn wait(&mut self, timeout: Duration) -> Result<Upload> {
        tokio::time::timeout(timeout, self.rx.recv())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "the display did not upload a thumbnail to {} within {} seconds",
                    self.url,
                    timeout.as_secs()
                )
            })?
            .context("the upload receiver stopped")
    }

    /// The next upload, or `None` once the receiver is gone.
    pub async fn next(&mut self) -> Option<Upload> {
        self.rx.recv().await
    }

    /// Stop the HTTP server.
    pub fn shutdown(self) {
        self.server.abort();
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn handle(
    State(receiver): State<Receiver>,
    mp: Result<Multipart, MultipartRejection>,
) -> Response {
    let mp = match mp {
        Ok(mp) => mp,
        Err(e) => {
            crate::say_err!("⚠️  POST {CALLBACK_PATH} is not a multipart upload: {e}");
            return e.into_response();
        }
    };
    match receiver.receive(mp).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err((status, message)) => {
            crate::say_err!("⚠️  Refused an upload ({status}): {message}");
            (status, message).into_response()
        }
    }
}

/// Anything but `POST /device-thumbnail`: report it, a real panel may differ from the app
async fn unexpected(method: Method, uri: Uri) -> StatusCode {
    crate::say_err!("⚠️  Unexpected request on the thumbnail receiver: {method} {uri}");
    StatusCode::NOT_FOUND
}

impl Receiver {
    /// Parse the upload, save it like the app does, and report it to the listener.
    async fn receive(&self, mp: Multipart) -> Result<PathBuf, (StatusCode, String)> {
        let mut upload = parse_parts(mp).await?;
        let path = upload.save(&self.out_dir).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Server error: {e:#}"),
            )
        })?;
        let _ = self.tx.send(upload);
        Ok(path)
    }
}

/// The file part of the upload, before it is folded into an [`Upload`].
#[derive(Default)]
struct ImagePart {
    name: String,
    file_name: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// Read every part, mirroring the app's handler (which logs `device-thumbnail part - name: value`).
async fn parse_parts(mut mp: Multipart) -> Result<Upload, (StatusCode, String)> {
    let bad = |msg: String| (StatusCode::BAD_REQUEST, msg);
    let mut upload = Upload::default();
    let mut image: Option<ImagePart> = None;

    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                // axum reports a missing boundary, a malformed part, or a size breach
                return Err(if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    (
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "upload too large".to_string(),
                    )
                } else {
                    bad(format!("malformed multipart upload: {e}"))
                });
            }
        };
        let name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(str::to_string);
        let part_type = field.content_type().map(str::to_string);
        // A part with a filename is a file part; the app keys the extension off the name.
        let is_file = file_name.is_some() || name.contains('.');

        let mut field = field;
        let bytes = read_bounded(&mut field).await?;
        if is_file {
            match &mut image {
                Some(part) => {
                    // Keep the largest file part if a panel sends several
                    if bytes.len() > part.bytes.len() {
                        part.bytes = bytes;
                    }
                }
                None => {
                    image = Some(ImagePart {
                        name,
                        file_name,
                        content_type: part_type,
                        bytes,
                    })
                }
            }
            continue;
        }
        let text = String::from_utf8_lossy(&bytes).trim().to_string();
        match name.as_str() {
            "device_id" => upload.device_id = text,
            "content_id" => upload.content_id = Some(text),
            "content_name" if text != "null" && !text.is_empty() => {
                upload.content_name = Some(text)
            }
            "content_type" => upload.content_type = Some(text),
            "content_player_type" => upload.content_player_type = Some(text),
            "thumbnail_timestamp" => upload.thumbnail_timestamp = Some(text),
            "schedule_downloaded_timestamp" => upload.schedule_downloaded_timestamp = Some(text),
            _ => {
                upload.extra.insert(name, text);
            }
        }
    }

    let Some(part) = image else {
        return Err(bad("the upload carries no image part".to_string()));
    };
    if part.bytes.is_empty() {
        return Err(bad("the image part is empty".to_string()));
    }
    if upload.device_id.is_empty() {
        return Err(bad("Invalid device id: ".to_string()));
    }
    upload.part_name = part.name;
    upload.file_name = part.file_name;
    upload.part_content_type = part.content_type;
    upload.bytes = part.bytes;
    Ok(upload)
}

async fn read_bounded(
    field: &mut axum::extract::multipart::Field<'_>,
) -> Result<Vec<u8>, (StatusCode, String)> {
    let mut out = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("malformed multipart upload: {e}"),
        )
    })? {
        if out.len() + chunk.len() > MAX_UPLOAD_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "upload too large".to_string(),
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `emdx thumbnail` and `emdx serve` write here unless `--out` says otherwise:
/// `<config dir>/thumbnails`.
pub fn default_out_dir(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(p) => p.join("thumbnails"),
        None => PathBuf::from("thumbnails"),
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn rotate_saved_turns_the_file_upright() {
        let dir = tempfile::tempdir().unwrap();
        let img = image::RgbImage::new(64, 36);
        let mut upload = Upload {
            device_id: "SN1".into(),
            part_name: "thumbnail.jpg".into(),
            bytes: crate::imaging::encode_jpeg_rgb(&img, 90).unwrap(),
            ..Default::default()
        };
        let saved = upload.save(dir.path()).await.unwrap();
        assert_eq!(upload.rotate_saved(0).await.unwrap(), saved);
        let rotated = upload.rotate_saved(90).await.unwrap();
        assert_eq!(rotated, saved);
        let on_disk = std::fs::read(&rotated).unwrap();
        assert_eq!(crate::imaging::dimensions(&on_disk).unwrap(), (36, 64));
        assert_eq!(on_disk, upload.bytes);
    }

    use super::*;

    #[test]
    fn callback_url_matches_the_app_route() {
        assert_eq!(
            callback_url("192.168.1.5", 6868, false),
            "http://192.168.1.5:6868/device-thumbnail"
        );
        assert_eq!(
            callback_url("192.168.1.5", 6869, true),
            "https://192.168.1.5:6869/device-thumbnail"
        );
        assert_eq!(CALLBACK_PATH, "/device-thumbnail");
        assert_eq!((NON_SECURE_PORT, SECURE_PORT), (6868, 6869));
    }

    #[test]
    fn extension_comes_from_the_part_name() {
        let mut u = Upload {
            part_name: "thumbnail.jpg".into(),
            ..Default::default()
        };
        assert_eq!(u.extension(), "jpg");
        u = Upload {
            part_name: "image".into(),
            file_name: Some("photo.PNG".into()),
            ..Default::default()
        };
        assert_eq!(u.extension(), "png");
        u = Upload {
            part_name: "no-ext".into(),
            ..Default::default()
        };
        assert_eq!(u.extension(), "jpg");
        u = Upload {
            part_name: "bad/../..".into(),
            ..Default::default()
        };
        assert_eq!(u.extension(), "jpg");
    }

    #[test]
    fn device_dir_stays_inside_the_output_dir() {
        let u = Upload {
            device_id: "SN 12/34".into(),
            ..Default::default()
        };
        assert_eq!(u.device_dir(Path::new("/t")), PathBuf::from("/t/SN_12_34"));
        let u = Upload {
            device_id: "../../etc".into(),
            ..Default::default()
        };
        assert_eq!(u.device_dir(Path::new("/t")), PathBuf::from("/t/etc"));
        let u = Upload {
            device_id: String::new(),
            ..Default::default()
        };
        assert_eq!(
            u.device_dir(Path::new("/t")),
            PathBuf::from("/t/unknown-device")
        );
    }

    #[test]
    fn thumbnail_filename_pattern() {
        let u = Upload {
            device_id: "SN1".into(),
            part_name: "thumb.webp".into(),
            ..Default::default()
        };
        let p = u.path_in(Path::new("/cache"));
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("thumbnail_"));
        assert!(name.ends_with(".webp"));
        assert_eq!(p.parent(), Some(Path::new("/cache/SN1")));
    }

    #[test]
    fn schedule_timestamp_uses_the_app_format() {
        let t = parse_schedule_timestamp("14:23 06/29").expect("parses");
        assert_eq!(t.format("%H:%M %m/%d").to_string(), "14:23 06/29");
        assert!(parse_schedule_timestamp("not a time").is_none());
    }

    #[test]
    fn a_saved_thumbnail_can_be_served_back_by_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("SN123")).unwrap();
        std::fs::write(root.join("SN123/thumbnail_x.jpg"), b"x").unwrap();
        std::fs::write(root.join("SN123/other.png"), b"x").unwrap();
        let rel = saved_relative(&root.join("SN123/thumbnail_x.jpg")).unwrap();
        assert_eq!(rel, "SN123/thumbnail_x.jpg");
        assert_eq!(saved_relative(Path::new("/tmp/a.jpg")), None);
        // Nothing that walks out of the folder is accepted
        assert_eq!(
            resolve_saved(root, "SN123/thumbnail_x.jpg").unwrap(),
            root.join("SN123/thumbnail_x.jpg")
        );
        assert_eq!(resolve_saved(root, "SN123/missing.jpg"), None, "must exist");
        for bad in [
            "../SN123/x.jpg",
            "SN123/../../etc/passwd",
            "SN123",
            "SN123/a/b.jpg",
            "SN123/..",
            "SN123/x.jpg\0",
            "SN123/other.png",
        ] {
            assert_eq!(resolve_saved(root, bad), None, "{bad}");
        }
    }

    #[test]
    fn summary_reports_the_known_fields() {
        let u = Upload {
            device_id: "SN1".into(),
            content_name: Some("Poster".into()),
            content_type: Some("ImageContent".into()),
            schedule_downloaded_timestamp: Some("09:05 01/02".into()),
            extra: [("weird_part".to_string(), "42".to_string())].into(),
            ..Default::default()
        };
        let s = u.summary().join("\n");
        assert!(s.contains("Poster"));
        assert!(s.contains("ImageContent"));
        assert!(s.contains("09:05 01/02"));
        assert!(s.contains("weird_part"));
        assert!(u.metadata_json()["extraParts"]["weird_part"] == "42");
    }

    #[test]
    fn cms_cert_pem_has_the_apps_shape() {
        let cert = CmsCert::generate("192.168.1.5").unwrap();
        assert!(cert.pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(cert.pem.ends_with("\n-----END CERTIFICATE-----"));
        assert!(!cert.pem.contains('\r'));
        assert!(cert.pem.lines().all(|l| l.len() <= 64));
    }

    #[test]
    fn default_out_dir_sits_next_to_the_config() {
        assert_eq!(
            default_out_dir(Path::new("/home/me/.config/emdx/config.json")),
            PathBuf::from("/home/me/.config/emdx/thumbnails")
        );
    }

    /// Feed the real parser a body exactly like a panel's upload.
    async fn parse(body: String, boundary: &str) -> Result<Upload, (StatusCode, String)> {
        use axum::{extract::FromRequest, http::Request};
        let req = Request::builder()
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(body))
            .unwrap();
        let mp = <Multipart as FromRequest<()>>::from_request(req, &())
            .await
            .map_err(|e| {
                let res = e.into_response();
                (res.status(), "invalid request".to_string())
            })?;
        parse_parts(mp).await
    }

    fn multipart(boundary: &str, parts: &[(&str, &str, Option<&str>)]) -> String {
        let mut body = String::new();
        for (name, value, filename) in parts {
            body.push_str(&format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\""
            ));
            if let Some(f) = filename {
                body.push_str(&format!("; filename=\"{f}\""));
            }
            body.push_str("\r\n\r\n");
            body.push_str(value);
            body.push_str("\r\n");
        }
        body.push_str(&format!("--{boundary}--\r\n"));
        body
    }

    #[tokio::test]
    async fn parses_a_panel_upload() {
        let body = multipart(
            "BOUNDARY",
            &[
                ("device_id", "SN12345", None),
                ("content_id", "ABC", None),
                ("content_name", "Poster", None),
                ("content_type", "ImageContent", None),
                ("content_player_type", "4", None),
                ("thumbnail_timestamp", "1751209800", None),
                ("schedule_downloaded_timestamp", "09:05 01/02", None),
                ("brand_new_part", "hello", None),
                ("thumbnail", "IMGDATA", Some("thumbnail.jpg")),
            ],
        );
        let u = parse(body, "BOUNDARY").await.expect("parses");
        assert_eq!(u.device_id, "SN12345");
        assert_eq!(u.content_id.as_deref(), Some("ABC"));
        assert_eq!(u.content_name.as_deref(), Some("Poster"));
        assert_eq!(u.content_player_type.as_deref(), Some("4"));
        assert_eq!(
            u.schedule_downloaded_timestamp.as_deref(),
            Some("09:05 01/02")
        );
        assert_eq!(u.extra["brand_new_part"], "hello");
        assert_eq!(u.part_name, "thumbnail");
        assert_eq!(u.file_name.as_deref(), Some("thumbnail.jpg"));
        assert_eq!(u.extension(), "jpg");
        assert_eq!(u.size(), 7);
    }

    #[tokio::test]
    async fn rejects_an_upload_without_an_image_or_serial() {
        let body = multipart("B", &[("device_id", "SN1", None)]);
        let (status, msg) = parse(body, "B").await.expect_err("no image part");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("no image part"), "{msg}");

        let body = multipart("B", &[("thumbnail", "data", Some("t.jpg"))]);
        let (status, msg) = parse(body, "B").await.expect_err("no device_id");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("Invalid device id"), "{msg}");
    }
}
