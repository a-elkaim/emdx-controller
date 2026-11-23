//! The app's other mode ("Secure Mobile CMS" off): plain HTTP, no certificate.
//! In its own file: the mode is set for the whole process.

mod common;

use common::Behavior;
use emdx_controller::{
    cms::{self, Image},
    mdc::{cmd, Device},
};

#[tokio::test]
async fn plain_http_sends_no_certificate() {
    cms::set_plain_http(true);
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

    assert_eq!(mock.downloads(), [b"poster".to_vec()]);
    let cmds = mock.commands();
    assert!(cmds.iter().all(|(c, _)| *c != cmd::TERMS));
    let url = cmds
        .iter()
        .filter(|(c, d)| *c == cmd::CONTENT_DOWNLOAD && d.len() > 3)
        .map(|(_, d)| String::from_utf8_lossy(&d[3..]).into_owned())
        .next_back()
        .unwrap();
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
}
