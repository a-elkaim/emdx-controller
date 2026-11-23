//! Sending content the app's way, against the mock display: HTTPS with the pushed
//! certificate, the app's URLs and manifest, and the display's progress reports.

mod common;

use common::Behavior;
use emdx_controller::{
    cms::{self, Image},
    mdc::{cmd, Device},
};

#[tokio::test]
async fn an_image_goes_over_https_with_the_pushed_certificate() {
    let mock = common::start("123456", Behavior::Normal).await;
    let device = Device::new("127.0.0.1", "123456").with_port(mock.port);

    cms::send_image(
        &device,
        "127.0.0.1",
        Image::jpeg(b"poster".to_vec()),
        "Poster",
    )
    .await
    .unwrap();

    // The mock trusts only the certificate it was pushed: it downloaded, so HTTPS worked
    assert_eq!(mock.downloads(), [b"poster".to_vec()]);
    // The certificate comes before the content command, as in the app
    let cmds = mock.commands();
    let cert = cmds.iter().position(|(c, _)| *c == cmd::TERMS).unwrap();
    let (content, url) = cmds
        .iter()
        .enumerate()
        .filter(|(_, (c, d))| *c == cmd::CONTENT_DOWNLOAD && d.len() > 3)
        .map(|(n, (_, d))| (n, String::from_utf8_lossy(&d[3..]).into_owned()))
        .next_back()
        .unwrap();
    assert!(cert < content);

    let manifests = mock.manifests.lock().unwrap().clone();
    let m = &manifests[0];
    let id = m["id"].as_str().unwrap();
    assert!(id.starts_with("content_info_"));
    assert!(url.starts_with("https://127.0.0.1:"), "{url}");
    assert!(
        url.ends_with(&format!("/content?id={id}&content_type=ImageContent")),
        "{url}"
    );
    assert_eq!(m["program_id"], "com.samsung.android.ePaper");
    assert_eq!(m["content_type"], "ImageContent");
    assert_eq!(m["name"], "Poster");
    let image = &m["schedule"][0]["contents"][0];
    // A single image is its own content
    assert_eq!(image["file_id"], id);
    assert_eq!(image["duration"], 86_400_000u64);
    let image_url = image["image_url"].as_str().unwrap();
    assert!(
        image_url.ends_with(&format!("/image?path={id}.jpg")),
        "{image_url}"
    );
}

#[tokio::test]
async fn a_display_that_reports_progress_is_listened_to() {
    let mock = common::start("123456", Behavior::ReportsProgress).await;
    let device = Device::new("127.0.0.1", "123456").with_port(mock.port);
    cms::send_image(
        &device,
        "127.0.0.1",
        Image::jpeg(b"poster".to_vec()),
        "Poster",
    )
    .await
    .unwrap();
    assert_eq!(mock.downloads(), [b"poster".to_vec()]);
}

#[tokio::test]
async fn a_failure_the_display_reports_ends_the_wait() {
    let mock = common::start("123456", Behavior::ReportsFailure).await;
    let device = Device::new("127.0.0.1", "123456").with_port(mock.port);
    let err = cms::send_image(
        &device,
        "127.0.0.1",
        Image::jpeg(b"poster".to_vec()),
        "Poster",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("failed: storage is full"), "{err}");
    assert!(mock.downloads().is_empty());
}
