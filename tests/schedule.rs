//! A schedule against the mock display: one manifest, one block per start time.

mod common;

use common::Behavior;
use emdx_controller::{
    content::StartTime,
    mdc::Device,
    schedule::{self, Entry},
};

#[tokio::test]
async fn the_display_gets_one_block_per_start_time() {
    let mock = common::start("123456", Behavior::Normal).await;
    let device = Device::new("127.0.0.1", "123456").with_port(mock.port);
    let at = |t: &str| StartTime::parse(t).unwrap();
    let entry = |t: &str, image: &[u8]| Entry {
        start: at(t),
        image: image.to_vec(),
    };
    let entries = vec![
        entry("12:00", b"noon"),
        entry("8:00", b"morning"),
        entry("18:30", b"morning"),
    ];

    schedule::send(&device, "127.0.0.1", entries, "Day")
        .await
        .unwrap();

    let manifests = mock.manifests.lock().unwrap().clone();
    assert_eq!(manifests.len(), 1);
    let m = &manifests[0];
    assert_eq!(m["content_type"], "ScheduleContent");
    assert_eq!(m["name"], "Day");
    let blocks = m["schedule"].as_array().unwrap();
    let times: Vec<_> = blocks
        .iter()
        .map(|b| b["start_time"].as_str().unwrap())
        .collect();
    assert_eq!(times, ["08:00:00", "12:00:00", "18:30:00"]);
    for b in blocks {
        assert_eq!(b["contents"].as_array().unwrap().len(), 1);
        assert_eq!(b["contents"][0]["duration"], 86_400_000u64);
    }
    // The same image at two times is one file on the display
    assert_eq!(
        blocks[0]["contents"][0]["file_id"],
        blocks[2]["contents"][0]["file_id"]
    );
    assert_ne!(
        blocks[0]["contents"][0]["file_id"],
        blocks[1]["contents"][0]["file_id"]
    );
    assert_eq!(
        mock.downloads(),
        [b"morning".to_vec(), b"noon".to_vec(), b"morning".to_vec()]
    );
}

#[tokio::test]
async fn a_bad_schedule_is_refused_before_connecting() {
    // No display at this address: the check must fail first
    let device = Device::new("192.0.2.1", "123456");
    let at = StartTime::parse("9:00").unwrap();
    let twice = vec![
        Entry {
            start: at,
            image: b"a".to_vec(),
        },
        Entry {
            start: at,
            image: b"b".to_vec(),
        },
    ];
    let err = schedule::send(&device, "127.0.0.1", twice, "x")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("09:00"), "{err}");
}
