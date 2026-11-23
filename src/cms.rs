//! Sending content the way the Android ePaper app does: a small server shaped like
//! the app's embedded one (`NettyEmbeddedServer`), and the MDC commands that point
//! the display at it.
//!
//! - `GET /content?id=<content id>&content_type=<type>`: the manifest (JSON)
//! - `GET /image?path=<file name>`: one image
//! - `POST /content-transfer-progress`: the display reports how the download goes
//!
//! Like the app with its default "Secure Mobile CMS" setting, this is HTTPS on port
//! 6869 with a certificate the display is handed first (MDC `0xD2/0x20`); the app's
//! other mode is plain HTTP on port 6868 ([`set_plain_http`]). When the port is taken
//! (by `emdx serve`'s thumbnail receiver, for one) a spare one is used: the display
//! follows whatever URL it is given.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use tokio::{net::TcpListener, sync::mpsc};

use crate::{
    content::{self, ContentType, Slide},
    mdc::Device,
    thumbnail::{self, CmsCert, TlsListener},
};

static PLAIN_HTTP: AtomicBool = AtomicBool::new(false);

/// Serve content over plain HTTP on port 6868 instead of HTTPS on 6869 (the app
/// with "Secure Mobile CMS" turned off). Applies to everything sent afterwards.
pub fn set_plain_http(plain: bool) {
    PLAIN_HTTP.store(plain, Ordering::Relaxed);
}

/// An image to send: its bytes and file extension (`jpg`, `png`…)
#[derive(Debug, Clone)]
pub struct Image {
    pub bytes: Vec<u8>,
    pub ext: String,
}

impl Image {
    pub fn jpeg(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            ext: "jpg".into(),
        }
    }

    fn mime(&self) -> &'static str {
        match self.ext.to_ascii_lowercase().as_str() {
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            _ => "image/jpeg",
        }
    }
}

/// How long to wait for the display to fetch the images: a minute for the first
/// one, then 30 s for each further one
fn download_window(count: usize) -> Duration {
    Duration::from_secs(60 + 30 * count.saturating_sub(1) as u64)
}

/// What the server tells the sender
enum Event {
    /// Every image has been fetched
    AllFetched,
    /// A progress report: the overall status, and its error message
    Report(String, Option<String>),
}

struct Served {
    content_id: String,
    manifest: String,
    what: &'static str,
    /// File name, media type, bytes
    images: Vec<(String, &'static str, Vec<u8>)>,
    fetched: Mutex<Vec<bool>>,
    events: mpsc::UnboundedSender<Event>,
}

/// Show one image (the app's `ImageContent`). Returns `true` when the content
/// player had to be switched to mobile first.
pub async fn send_image(device: &Device, local_ip: &str, image: Image, name: &str) -> Result<bool> {
    deliver(
        device,
        local_ip,
        vec![image],
        ContentType::Image,
        "image",
        |_, slides| content::image_manifest(name, &slides[0], &content::create_time_now()),
    )
    .await
}

/// Serve `images` and the manifest `manifest` builds for them (it gets the content id
/// and one [`Slide`] per image, same order), hand the display the certificate and the
/// manifest URL, and wait until it has downloaded every image or reports the outcome.
/// `what` names the content in progress lines.
pub(crate) async fn deliver(
    device: &Device,
    local_ip: &str,
    images: Vec<Image>,
    content_type: ContentType,
    what: &'static str,
    manifest: impl FnOnce(&str, &[Slide]) -> String,
) -> Result<bool> {
    if images.is_empty() {
        bail!("nothing to send: no image");
    }
    let secure = !PLAIN_HTTP.load(Ordering::Relaxed);
    let preferred = if secure {
        thumbnail::SECURE_PORT
    } else {
        thumbnail::NON_SECURE_PORT
    };
    let listener = match TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], preferred))).await {
        Ok(l) => l,
        Err(_) => TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
            .await
            .context("no free port to serve the content from")?,
    };
    let port = listener.local_addr()?.port();
    let base = format!(
        "{}://{local_ip}:{port}",
        if secure { "https" } else { "http" }
    );

    // A single image is its own content; in a playlist or a schedule every image
    // is a content of its own, with its own id and folder on the display
    let content_id = content::new_content_id();
    let slides: Vec<Slide> = images
        .iter()
        .enumerate()
        .map(|(n, image)| {
            let file_id = if content_type == ContentType::Image && n == 0 {
                content_id.clone()
            } else {
                content::new_content_id()
            };
            let file_name = format!("{file_id}.{}", image.ext);
            Slide {
                image_url: format!("{base}/image?path={file_name}"),
                file_name,
                file_id,
                file_size: image.bytes.len(),
            }
        })
        .collect();
    let manifest = manifest(&content_id, &slides);
    let url = format!(
        "{base}/content?id={content_id}&content_type={}",
        content_type.name()
    );

    let count = images.len();
    let (events, mut events_rx) = mpsc::unbounded_channel();
    let served = Arc::new(Served {
        content_id,
        manifest,
        what,
        images: images
            .into_iter()
            .zip(&slides)
            .map(|(image, slide)| (slide.file_name.clone(), image.mime(), image.bytes))
            .collect(),
        fetched: Mutex::new(vec![false; count]),
        events,
    });
    let app = Router::new()
        .route("/content", get(serve_manifest))
        .route("/image", get(serve_image))
        .route("/content-transfer-progress", post(progress))
        .with_state(served);
    let cert = if secure {
        Some(CmsCert::shared(local_ip)?)
    } else {
        None
    };
    let server = match &cert {
        Some(cert) => {
            let listener = TlsListener::new(listener, cert, "content server");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            })
        }
        None => tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        }),
    };

    let result = async {
        let switched = device
            .push_cms_content(&url, cert.as_ref().map(|c| c.pem.as_str()))
            .await?;
        let wait = download_window(count);
        crate::say!(
            "⏳ Waiting for the display to download the {what} ({count} image{})...",
            if count == 1 { "" } else { "s" }
        );
        let outcome = tokio::time::timeout(wait, async {
            let mut reported = false;
            loop {
                match events_rx.recv().await {
                    Some(Event::AllFetched) => {
                        // A display that reports its progress gets a moment to send
                        // the last one; otherwise just let the transfer finish
                        let grace = if reported { 3000 } else { 500 };
                        let last = tokio::time::timeout(Duration::from_millis(grace), async {
                            while let Some(event) = events_rx.recv().await {
                                if let Event::Report(status, message) = event {
                                    if let Some(failure) = failure(&status, message) {
                                        return Err(failure);
                                    }
                                    if status == "Successful" {
                                        break;
                                    }
                                }
                            }
                            Ok(())
                        })
                        .await;
                        return last.unwrap_or(Ok(()));
                    }
                    Some(Event::Report(status, message)) => {
                        reported = true;
                        if let Some(failure) = failure(&status, message) {
                            return Err(failure);
                        }
                        if status == "Successful" {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
        })
        .await
        .with_context(|| {
            format!(
                "the display didn't download the {what} within {} s",
                wait.as_secs()
            )
        })?;
        outcome.map_err(anyhow::Error::msg)?;
        Ok(switched)
    }
    .await;
    server.abort();
    result
}

/// The app's `ContentTransferStatus` values that end a transfer badly
fn failure(status: &str, message: Option<String>) -> Option<String> {
    matches!(status, "Failed" | "Cancelled").then(|| {
        format!(
            "the display reported the transfer as {}{}",
            status.to_lowercase(),
            message
                .filter(|m| !m.is_empty())
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        )
    })
}

/// `GET /content`: the manifest, with the app's `Cache-Control`
async fn serve_manifest(
    State(served): State<Arc<Served>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if query.get("id") != Some(&served.content_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Server error: content not found",
        )
            .into_response();
    }
    crate::say!("🔄 Serving the {} manifest...", served.what);
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "max-age=300"),
        ],
        served.manifest.clone(),
    )
        .into_response()
}

/// `GET /image?path=`: one image, as an attachment like the app sends it
async fn serve_image(
    State(served): State<Arc<Served>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let found = query
        .get("path")
        .and_then(|path| served.images.iter().position(|(name, ..)| name == path));
    let Some(n) = found else {
        return (StatusCode::BAD_REQUEST, "Image not found").into_response();
    };
    let (name, mime, bytes) = &served.images[n];
    let count = served.images.len();
    let done = {
        let mut fetched = served.fetched.lock().unwrap_or_else(|e| e.into_inner());
        fetched[n] = true;
        fetched.iter().filter(|x| **x).count()
    };
    crate::say!(
        "🔄 Serving image {}/{count} ({done}/{count} downloaded)",
        n + 1
    );
    if done == count {
        let _ = served.events.send(Event::AllFetched);
    }
    (
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CACHE_CONTROL, "max-age=1800".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename={name}"),
            ),
        ],
        bytes.clone(),
    )
        .into_response()
}

/// `POST /content-transfer-progress`: the app's `ContentTransferProgress` JSON. Like
/// the app, it needs `device_id` and `content_id` and answers with the same JSON.
async fn progress(State(served): State<Arc<Served>>, body: String) -> Response {
    let Ok(report) = serde_json::from_str::<serde_json::Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "Invalid progress report").into_response();
    };
    let text = |key: &str| report[key].as_str().unwrap_or_default().to_string();
    if text("device_id").is_empty() || text("content_id").is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing device_id or content_id").into_response();
    }
    let status = text("status");
    let image = &report["current_image_status"];
    crate::say!(
        "📶 The display reports: {status}{}{}",
        report["progress"]
            .as_i64()
            .map(|p| format!(" {p} %"))
            .unwrap_or_default(),
        image["image_name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(|n| format!(" ({n}: {})", image["status"].as_str().unwrap_or_default()))
            .unwrap_or_default()
    );
    let message = Some(text("error_message")).filter(|m| !m.is_empty());
    let _ = served.events.send(Event::Report(status, message));
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_failed_and_cancelled_end_a_transfer_badly() {
        for ok in [
            "Pending",
            "Downloading",
            "PreparingShowImage",
            "Successful",
            "Initializing",
            "Unknown",
        ] {
            assert!(failure(ok, None).is_none(), "{ok}");
        }
        assert_eq!(
            failure("Failed", Some("disk full".into())).unwrap(),
            "the display reported the transfer as failed: disk full"
        );
        assert_eq!(
            failure("Cancelled", None).unwrap(),
            "the display reported the transfer as cancelled"
        );
    }
}
