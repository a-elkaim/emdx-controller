//! The thumbnail flow end to end: MDC `0xC7/0x54` goes out, a multipart upload
//! comes back in, and the image lands on disk.

mod common;

use std::time::Duration;

use common::Behavior;
use emdx_controller::{
    mdc::{cmd, sub, Device},
    thumbnail::{self, Endpoint},
};

#[tokio::test]
async fn asks_for_a_thumbnail_and_receives_the_upload() {
    let mock = common::start("123456", Behavior::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let mut endpoint = Endpoint::start("127.0.0.1", 0, dir.path()).await.unwrap();
    assert!(endpoint.url.ends_with(thumbnail::CALLBACK_PATH));

    let mut s = Device::new("127.0.0.1", "123456")
        .with_port(mock.port)
        .connect(Duration::from_secs(5))
        .await
        .unwrap();
    let reply = s.request_thumbnail(&endpoint.url).await.unwrap();
    // The panel echoes `54 <len> <url>`; only the URL is kept
    assert_eq!(reply.as_deref(), Some(endpoint.url.as_str()));
    s.close().await;

    // What the panel really received: 54 then the callback URL
    let sent = mock.sent(cmd::CONTENT_DOWNLOAD);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0][0], sub::GET_THUMBNAIL);
    assert_eq!(&sent[0][1..], endpoint.url.as_bytes());

    let upload = endpoint.wait(Duration::from_secs(10)).await.unwrap();
    assert_eq!(upload.device_id, "SN123");
    assert_eq!(upload.content_id.as_deref(), Some("CONTENT-1"));
    assert_eq!(upload.content_name.as_deref(), Some("Mock poster"));
    assert_eq!(upload.content_type.as_deref(), Some("ImageContent"));
    assert_eq!(upload.content_player_type.as_deref(), Some("4"));
    assert_eq!(
        upload.schedule_downloaded_timestamp.as_deref(),
        Some("09:05 01/02")
    );
    assert_eq!(upload.extension(), "jpg");

    // Saved in the app's layout, and the panel was told it went through
    let path = upload.saved.clone().expect("saved");
    assert!(path.starts_with(dir.path().join("SN123")));
    assert_eq!(std::fs::read(&path).unwrap(), upload.bytes);
    assert_eq!(common::dims(&upload.bytes), (24, 12));
    mock.wait_for_uploads(1).await;
    assert_eq!(mock.uploads.lock().unwrap().as_slice(), &[200]);

    endpoint.shutdown();
}

#[tokio::test]
async fn rejects_an_upload_the_panel_cannot_have_meant() {
    let dir = tempfile::tempdir().unwrap();
    let endpoint = Endpoint::start("127.0.0.1", 0, dir.path()).await.unwrap();
    let url = endpoint.url.clone();

    // Fields but no image
    let res = reqwest::Client::new()
        .post(&url)
        .multipart(reqwest::multipart::Form::new().text("device_id", "SN1"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert!(res.text().await.unwrap().contains("no image part"));

    // An image but no serial
    let res = reqwest::Client::new()
        .post(&url)
        .multipart(reqwest::multipart::Form::new().part(
            "thumbnail",
            reqwest::multipart::Part::bytes(common::png(8, 8, [1, 2, 3])).file_name("t.jpg"),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert!(res.text().await.unwrap().contains("Invalid device id"));

    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    endpoint.shutdown();
}

#[tokio::test]
async fn keeps_listening_in_watch_mode() {
    let mock = common::start("123456", Behavior::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let mut endpoint = Endpoint::start("127.0.0.1", 0, dir.path()).await.unwrap();
    let url = endpoint.url.clone();

    let s = Device::new("127.0.0.1", "123456").with_port(mock.port);
    for _ in 0..2 {
        let mut session = s.connect(Duration::from_secs(5)).await.unwrap();
        session.request_thumbnail(&url).await.unwrap();
        session.close().await;
        let upload = endpoint.wait(Duration::from_secs(10)).await.unwrap();
        assert_eq!(upload.device_id, "SN123");
    }
    mock.wait_for_uploads(2).await;
    assert_eq!(mock.uploads.lock().unwrap().as_slice(), &[200, 200]);
    assert_eq!(
        std::fs::read_dir(dir.path().join("SN123")).unwrap().count(),
        2
    );
    endpoint.shutdown();
}

/// What the ePaper app does by default: push our certificate (`0xD2/0x20`), then
/// ask with an `https://` callback, and the panel uploads over TLS trusting that cert.
#[tokio::test]
async fn secure_mode_pushes_the_certificate_and_receives_over_https() {
    let mock = common::start("123456", Behavior::Normal).await;
    let dir = tempfile::tempdir().unwrap();
    let mut endpoint = Endpoint::start_secure("127.0.0.1", 0, dir.path())
        .await
        .unwrap();
    assert!(endpoint.url.starts_with("https://127.0.0.1:"));
    let pem = endpoint.cert_pem.clone().expect("a certificate to push");

    let mut s = Device::new("127.0.0.1", "123456")
        .with_port(mock.port)
        .connect(Duration::from_secs(5))
        .await
        .unwrap();
    s.push_cms_cert(&pem).await.unwrap();
    s.request_thumbnail(&endpoint.url).await.unwrap();
    // Kept open while the upload comes in, like the app's socket
    let upload = endpoint.wait(Duration::from_secs(10)).await.unwrap();
    s.close().await;

    let certs = mock.sent(cmd::TERMS);
    assert_eq!(certs.len(), 1);
    assert_eq!(
        &certs[0][..5],
        &[sub::DOWNLOAD_FILE_CERT, 0x00, 0x00, 0x06, 13]
    );
    assert_eq!(&certs[0][5..18], b"CMSServer.crt");
    assert!(certs[0].ends_with(pem.as_bytes()));
    assert!(!mock.terms_agreed.load(std::sync::atomic::Ordering::SeqCst));

    assert_eq!(upload.device_id, "SN123");
    mock.wait_for_uploads(1).await;
    assert_eq!(mock.uploads.lock().unwrap().as_slice(), &[200]);
    endpoint.shutdown();
}
