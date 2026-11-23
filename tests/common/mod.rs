//! A fake EMDX display: MDC over TLS on a local port. On a content-download
//! command it fetches the manifest and image over HTTP, like the real panel.
#![allow(dead_code)]

use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc, Mutex,
};

use emdx_controller::mdc::{self, cmd};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::TlsAcceptor;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Behavior {
    Normal,
    /// Rejects the PIN
    WrongPin,
    /// Authenticates but never answers commands
    Silent,
    /// Authenticates and accepts the content command, but never downloads
    NoDownload,
    /// Like `NoDownload`, with the USB content player selected
    UsbPlayer,
    /// Downloads, and reports its progress to the server first
    ReportsProgress,
    /// Fetches the manifest, then reports that the transfer failed
    ReportsFailure,
}

pub type CommandLog = Arc<Mutex<Vec<(u8, Vec<u8>)>>>;

/// Fields of each thumbnail upload the display POSTed: one entry per multipart field
pub type FieldLog = Arc<Mutex<Vec<Vec<(String, String)>>>>;

pub struct MockDisplay {
    pub port: u16,
    pub pin: String,
    /// Every command received: (command id, data)
    pub commands: CommandLog,
    /// Image bytes downloaded after content-download commands
    pub downloads: Arc<Mutex<Vec<Vec<u8>>>>,
    /// HTTP status codes the display got when POSTing a thumbnail to the callback URL
    pub uploads: Arc<Mutex<Vec<u16>>>,
    /// Multipart fields the display sent with each thumbnail upload
    pub upload_fields: FieldLog,
    /// Manifests (unescaped JSON) downloaded
    pub manifests: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Terms & conditions accepted on the display
    pub terms_agreed: Arc<AtomicBool>,
    /// Its background colour (`21 90`), its own setting; white at first
    pub background: Arc<AtomicU8>,
    /// The PIN it accepts now (`1B 87` changes it, like a real display)
    pub current_pin: Arc<Mutex<String>>,
}

impl MockDisplay {
    pub fn commands(&self) -> Vec<(u8, Vec<u8>)> {
        self.commands.lock().unwrap().clone()
    }

    pub fn downloads(&self) -> Vec<Vec<u8>> {
        self.downloads.lock().unwrap().clone()
    }

    pub fn sent(&self, command: u8) -> Vec<Vec<u8>> {
        self.commands()
            .into_iter()
            .filter(|(c, _)| *c == command)
            .map(|(_, d)| d)
            .collect()
    }

    pub async fn wait_for_downloads(&self, n: usize) {
        for _ in 0..200 {
            if self.downloads.lock().unwrap().len() >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("display did not download {n} image(s)");
    }

    pub async fn wait_for_uploads(&self, n: usize) {
        for _ in 0..200 {
            if self.uploads.lock().unwrap().len() >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("display did not upload {n} thumbnail(s)");
    }
}

fn acceptor() -> TlsAcceptor {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![ck.cert.der().clone()], key)
    .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

fn reply(command: u8, ack: bool, payload: &[u8]) -> Vec<u8> {
    mdc::encode_response(&mdc::Response {
        display_id: 0,
        ack,
        command,
        payload: payload.to_vec(),
    })
    .unwrap()
}

/// What a real EM32DX answers.
pub const TERMS_JSON: &str = r#"{"URL":"https://terms.example","OPENAPI":"epaper","ID":"eula","PARAM":[{"key":"lang","value":"en"}],"SUPPORT":[{"key":"model","value":"EM32DX"}]}"#;

fn reply_large(command: u8, payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() + 2) as u16;
    let mut f = vec![
        mdc::HEADER,
        mdc::RESPONSE,
        0,
        (len >> 8) as u8,
        len as u8,
        mdc::ACK,
        command,
    ];
    f.extend_from_slice(payload);
    f.push(mdc::checksum(&f[1..]));
    f
}

/// What a real EM32DX answers (setup-related commands as seen in the ePaper app).
fn respond(command: u8, data: &[u8], terms_agreed: &AtomicBool, background: &AtomicU8) -> Vec<u8> {
    use emdx_controller::mdc::sub;
    match (command, data.first().copied()) {
        // Background colour: it keeps the one it was set to and answers a query with it
        (cmd::SCREEN, Some(s)) if s == sub::BG_COLOR && data.len() > 1 => {
            let colour = data.get(2).copied().unwrap_or(0);
            background.store(colour, Ordering::SeqCst);
            reply(command, true, &[sub::BG_COLOR, 0x00, colour])
        }
        (cmd::SCREEN, Some(s)) if s == sub::BG_COLOR => reply(
            command,
            true,
            &[sub::BG_COLOR, 0x00, background.load(Ordering::SeqCst)],
        ),
        // The Mobile CMS certificate the app pushes before asking for a thumbnail
        (cmd::TERMS, Some(sub::DOWNLOAD_FILE_CERT)) => reply_large(command, &[]),
        (cmd::TERMS, _) if data.len() > 1 => {
            terms_agreed.store(true, Ordering::SeqCst);
            reply_large(command, &[sub::TERMS])
        }
        (cmd::TERMS, _) => {
            let agreed = terms_agreed.load(Ordering::SeqCst);
            let mut p = vec![sub::TERMS, 0, 0, u8::from(!agreed)];
            p.extend_from_slice(&[0xC0, 0x00, 0, TERMS_JSON.len() as u8]);
            p.extend_from_slice(TERMS_JSON.as_bytes());
            reply_large(command, &p)
        }
        // A panel acknowledges a thumbnail request with `54 <len> <url>` before it uploads
        (cmd::CONTENT_DOWNLOAD, Some(s)) if s == sub::GET_THUMBNAIL => {
            let url = &data[1..];
            let mut p = vec![sub::GET_THUMBNAIL, url.len() as u8];
            p.extend_from_slice(url);
            reply(command, true, &p)
        }
        // Content player: mobile (MobileCMS), like a display after setup
        (cmd::CONTENT_DOWNLOAD, Some(s)) if s == sub::CONTENT_PLAYER && data.len() == 1 => {
            reply(command, true, &[sub::CONTENT_PLAYER, 4])
        }
        (cmd::BATTERY, Some(sub::WIFI_MAC)) => reply(command, true, b"\x81b0f2f6550d0b"),
        (cmd::BATTERY, Some(sub::SETUP_STATUS)) if data.len() == 1 => {
            reply(command, true, &[sub::SETUP_STATUS, 0])
        }
        (cmd::BATTERY, Some(s)) if s != sub::BATTERY => reply(command, true, &[s]),
        _ => respond_basic(command, data),
    }
}

fn respond_basic(command: u8, data: &[u8]) -> Vec<u8> {
    match command {
        cmd::POWER if data.is_empty() => reply(command, false, &[0x01]), // EMDX NAKs the power query
        cmd::BATTERY => reply(command, true, &[0x73, 0x00, 0x01, 0x01, 77]),
        cmd::SERIAL_NUMBER => reply(command, true, b"SN123\0"),
        cmd::SOFTWARE_VERSION => reply(command, true, b"T-EMDX-1.0"),
        cmd::DEVICE_NAME => reply(command, true, b"Mock EMDX"),
        cmd::NETWORK_STANDBY if data.is_empty() => reply(command, true, &[0x01]),
        _ => reply(command, true, data.get(..1).unwrap_or(&[])),
    }
}

async fn simulate_download(
    url: String,
    cert: Option<String>,
    downloads: Arc<Mutex<Vec<Vec<u8>>>>,
    manifests: Arc<Mutex<Vec<serde_json::Value>>>,
    report: Option<&'static str>,
) {
    // Over HTTPS it trusts exactly the certificate it was pushed, nothing else
    let mut client = reqwest::Client::builder().tls_built_in_root_certs(false);
    if let Some(pem) = &cert {
        client =
            client.add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes()).unwrap());
    }
    let client = client.build().unwrap();
    let Ok(res) = client.get(&url).send().await else {
        return;
    };
    let Ok(text) = res.text().await else { return };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    // Every slide of every block, in order (one block except for a schedule)
    let image_urls: Vec<String> = manifest["schedule"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["contents"].as_array())
        .flatten()
        .filter_map(|x| x["image_url"].as_str().map(str::to_string))
        .collect();
    let content_id = manifest["id"].as_str().unwrap_or_default().to_string();
    manifests.lock().unwrap().push(manifest);
    // A display that reports (the app's `ContentTransferProgress`) posts to the
    // server it got the manifest from
    if let Some(status) = report {
        let origin = url.split("/content?").next().unwrap_or_default();
        let _ = client
            .post(format!("{origin}/content-transfer-progress"))
            .json(&serde_json::json!({
                "device_id": "SN123",
                "content_id": content_id,
                "content_name": "Mock",
                "status": status,
                "error_message": if status == "Failed" { "storage is full" } else { "" },
                "current_image_status": { "image_id": "", "image_name": "", "status": status },
                "progress": 0,
            }))
            .send()
            .await;
        if status == "Failed" {
            return;
        }
    }
    for image_url in image_urls {
        if let Ok(res) = client.get(&image_url).send().await {
            if res.status().is_success() {
                if let Ok(bytes) = res.bytes().await {
                    downloads.lock().unwrap().push(bytes.to_vec());
                }
            }
        }
    }
}

/// The PEM from a `0xD2/0x20` frame: `20 00 <type:2> <name len> name <len:2> pem`
fn pushed_cert(data: &[u8]) -> Option<String> {
    let name_len = usize::from(*data.get(4)?);
    let at = 5 + name_len;
    let len = usize::from(*data.get(at)?) << 8 | usize::from(*data.get(at + 1)?);
    let pem = data.get(at + 2..at + 2 + len)?;
    Some(String::from_utf8_lossy(pem).into_owned())
}

/// The panel POSTs a multipart thumbnail to the callback URL from a `0xC7/0x54` command.
/// Over HTTPS it trusts exactly the certificate it was pushed, nothing else.
async fn simulate_upload(
    url: String,
    serial: &str,
    cert: Option<String>,
    uploads: Arc<Mutex<Vec<u16>>>,
    upload_fields: FieldLog,
) {
    let fields: Vec<(String, String)> = [
        ("device_id", serial),
        ("content_id", "CONTENT-1"),
        ("content_name", "Mock poster"),
        ("content_type", "ImageContent"),
        ("content_player_type", "4"),
        ("thumbnail_timestamp", "1751209800"),
        ("schedule_downloaded_timestamp", "09:05 01/02"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();

    let mut form = reqwest::multipart::Form::new();
    for (name, value) in &fields {
        form = form.text(name.clone(), value.clone());
    }
    let form = form.part(
        "thumbnail",
        reqwest::multipart::Part::bytes(png(24, 12, [9, 9, 9])).file_name("thumbnail.jpg"),
    );

    let mut client = reqwest::Client::builder().tls_built_in_root_certs(false);
    if let Some(pem) = &cert {
        client =
            client.add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes()).unwrap());
    }
    let status = match client
        .build()
        .unwrap()
        .post(&url)
        .multipart(form)
        .send()
        .await
    {
        Ok(res) => res.status().as_u16(),
        Err(_) => 0,
    };
    uploads.lock().unwrap().push(status);
    upload_fields.lock().unwrap().push(fields);
}

pub async fn start(pin: &str, behavior: Behavior) -> MockDisplay {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let display = MockDisplay {
        port,
        pin: pin.to_string(),
        commands: Arc::default(),
        downloads: Arc::default(),
        uploads: Arc::default(),
        upload_fields: Arc::default(),
        manifests: Arc::default(),
        terms_agreed: Arc::default(),
        background: Arc::new(AtomicU8::default()),
        current_pin: Arc::new(Mutex::new(pin.to_string())),
    };
    let acceptor = acceptor();
    let (commands, downloads, manifests, terms) = (
        display.commands.clone(),
        display.downloads.clone(),
        display.manifests.clone(),
        display.terms_agreed.clone(),
    );
    let background = display.background.clone();
    let (uploads, upload_fields) = (display.uploads.clone(), display.upload_fields.clone());
    let current_pin = display.current_pin.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, commands, downloads, manifests, current_pin, terms) = (
                acceptor.clone(),
                commands.clone(),
                downloads.clone(),
                manifests.clone(),
                current_pin.clone(),
                terms.clone(),
            );
            let background = background.clone();
            let expected_pin = current_pin.lock().unwrap().clone();
            let (uploads, upload_fields) = (uploads.clone(), upload_fields.clone());
            tokio::spawn(async move {
                if tcp.write_all(mdc::BANNER).await.is_err() {
                    return;
                }
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };

                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while buf.len() < expected_pin.len() {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let pin: Vec<u8> = buf.drain(..expected_pin.len()).collect();
                if pin != expected_pin.as_bytes() || behavior == Behavior::WrongPin {
                    let _ = tls.write_all(b"MDCAUTH<<FAIL:0x01>>").await;
                    let _ = tls.shutdown().await;
                    return;
                }
                if tls.write_all(b"MDCAUTH<<PASS>>").await.is_err() {
                    return;
                }

                let mut cert = None;
                loop {
                    // Parse complete request frames: AA cmd id len data… checksum
                    // AA cmd id len data… cs, or large (terms): AA cmd id len_hi len_lo data… cs
                    loop {
                        let large = buf.len() > 1 && buf[1] == cmd::TERMS;
                        let head = if large { 5 } else { 4 };
                        if buf.len() < head {
                            break;
                        }
                        let len = if large {
                            usize::from(buf[3]) << 8 | usize::from(buf[4])
                        } else {
                            usize::from(buf[3])
                        };
                        let total = head + len + 1;
                        if buf.len() < total {
                            break;
                        }
                        let frame: Vec<u8> = buf.drain(..total).collect();
                        let (command, data) = (frame[1], frame[head..total - 1].to_vec());
                        commands.lock().unwrap().push((command, data.clone()));
                        // New PIN: for the next sessions
                        if command == cmd::BATTERY
                            && data.first() == Some(&emdx_controller::mdc::sub::PIN)
                        {
                            *current_pin.lock().unwrap() =
                                String::from_utf8_lossy(&data[1..]).into_owned();
                        }
                        if command == cmd::TERMS
                            && data.first() == Some(&emdx_controller::mdc::sub::DOWNLOAD_FILE_CERT)
                        {
                            cert = pushed_cert(&data);
                        }
                        if behavior == Behavior::Silent {
                            continue;
                        }
                        let answer = if behavior == Behavior::UsbPlayer
                            && command == cmd::CONTENT_DOWNLOAD
                            && data == [emdx_controller::mdc::sub::CONTENT_PLAYER]
                        {
                            reply(
                                command,
                                true,
                                &[emdx_controller::mdc::sub::CONTENT_PLAYER, 3],
                            )
                        } else {
                            respond(command, &data, &terms, &background)
                        };
                        if tls.write_all(&answer).await.is_err() {
                            return;
                        }
                        if command == cmd::CONTENT_DOWNLOAD
                            && !matches!(behavior, Behavior::NoDownload | Behavior::UsbPlayer)
                            && data.first() == Some(&emdx_controller::mdc::sub::GET_THUMBNAIL)
                        {
                            let url = String::from_utf8_lossy(&data[1..]).into_owned();
                            tokio::spawn(simulate_upload(
                                url,
                                "SN123",
                                cert.clone(),
                                uploads.clone(),
                                upload_fields.clone(),
                            ));
                        } else if command == cmd::CONTENT_DOWNLOAD
                            && !matches!(behavior, Behavior::NoDownload | Behavior::UsbPlayer)
                            && data.len() > 3
                        {
                            let url = String::from_utf8_lossy(&data[3..]).into_owned();
                            tokio::spawn(simulate_download(
                                url,
                                cert.clone(),
                                downloads.clone(),
                                manifests.clone(),
                                match behavior {
                                    Behavior::ReportsFailure => Some("Failed"),
                                    Behavior::ReportsProgress => Some("Downloading"),
                                    _ => None,
                                },
                            ));
                        }
                    }
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
            });
        }
    });
    display
}

/// A small solid-color PNG.
pub fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb(rgb));
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

pub fn dims(bytes: &[u8]) -> (u32, u32) {
    emdx_controller::imaging::dimensions(bytes).unwrap()
}
