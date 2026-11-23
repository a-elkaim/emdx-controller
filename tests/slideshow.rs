//! A slideshow against the mock display: one manifest, every image downloaded.

mod common;

use common::Behavior;
use emdx_controller::{content::SlideInterval, mdc::Device, slideshow};

#[tokio::test]
async fn the_display_downloads_every_slide_in_order() {
    let mock = common::start("123456", Behavior::Normal).await;
    let device = Device::new("127.0.0.1", "123456").with_port(mock.port);
    let jpegs = vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()];
    let interval = SlideInterval::parse("30").unwrap();

    let switched = slideshow::send(&device, "127.0.0.1", jpegs.clone(), interval, "Lobby")
        .await
        .unwrap();
    assert!(
        !switched,
        "the mock is already on the mobile content player"
    );

    // Every image, in the manifest's order
    assert_eq!(mock.downloads(), jpegs);
    let manifests = mock.manifests.lock().unwrap().clone();
    assert_eq!(manifests.len(), 1);
    let m = &manifests[0];
    assert_eq!(m["content_type"], "PlaylistContent");
    assert_eq!(m["name"], "Lobby");
    let contents = m["schedule"][0]["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    assert!(contents.iter().all(|c| c["duration"] == 30 * 60_000));
    // Each slide has its own id and folder on the display
    let ids: std::collections::HashSet<_> = contents
        .iter()
        .map(|c| c["file_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
}
