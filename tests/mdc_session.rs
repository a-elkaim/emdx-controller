//! MDC client against the mock display (real TLS, real framing).

mod common;

use std::time::Duration;

use common::Behavior;
use emdx_controller::mdc::{cmd, Device, MdcError};

fn device(port: u16, pin: &str) -> Device {
    Device::new("127.0.0.1", pin).with_port(port)
}

#[tokio::test]
async fn authenticates_and_reads_status() {
    let mock = common::start("123456", Behavior::Normal).await;
    let mut s = device(mock.port, "123456")
        .connect(Duration::from_secs(5))
        .await
        .unwrap();

    let battery = s.battery().await.unwrap();
    assert_eq!(battery.level, 77);
    assert!(battery.present && battery.healthy && !battery.charging);
    assert_eq!(s.serial_number().await.unwrap(), "SN123");
    assert_eq!(s.device_name().await.unwrap(), "Mock EMDX");
    assert_eq!(s.software_version().await.unwrap(), "T-EMDX-1.0");
    assert_eq!(s.power_state().await, None, "EMDX NAKs the power query");
    assert_eq!(s.network_standby().await, Some(true));
    s.close().await;

    assert_eq!(mock.sent(cmd::BATTERY), vec![vec![0x73]]);
}

#[tokio::test]
async fn sends_power_standby_timer_and_content_commands() {
    let mock = common::start("0000", Behavior::NoDownload).await;
    let mut s = device(mock.port, "0000")
        .connect(Duration::from_secs(5))
        .await
        .unwrap();
    s.set_network_standby(false).await.unwrap();
    s.set_on_timer(true, 0x02, 7, 45).await.unwrap();
    s.set_content_download("http://10.0.0.2:3001/x/content.json")
        .await
        .unwrap();
    s.set_power(false).await.unwrap();
    s.close().await;

    let cmds = mock.commands();
    assert_eq!(cmds[0], (cmd::NETWORK_STANDBY, vec![0x00]));
    assert_eq!(cmds[1], (cmd::ON_TIMER, vec![1, 2, 7, 45, 1]));
    assert_eq!(cmds[2].0, cmd::CONTENT_DOWNLOAD);
    assert_eq!(&cmds[2].1[..3], &[0x53, 0x80, 35]);
    assert_eq!(&cmds[2].1[3..], b"http://10.0.0.2:3001/x/content.json");
    assert_eq!(cmds[3], (cmd::POWER, vec![0x00]));
}

#[tokio::test]
async fn wrong_pin_is_reported() {
    let mock = common::start("123456", Behavior::Normal).await;
    let err = device(mock.port, "654321")
        .connect(Duration::from_secs(5))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, MdcError::AuthFailed(_)));
    assert_eq!(err.to_string(), "Authentication Failed: Incorrect PIN");
}

#[tokio::test]
async fn unanswered_command_times_out() {
    let mock = common::start("1", Behavior::Silent).await;
    let mut s = device(mock.port, "1")
        .with_command_timeout(Duration::from_millis(300))
        .connect(Duration::from_secs(5))
        .await
        .unwrap();
    let err = s.serial_number().await.unwrap_err();
    assert!(matches!(err, MdcError::Timeout(_)), "{err}");
}

#[tokio::test]
async fn connect_timeout_when_nothing_answers() {
    // A listener that accepts but never greets
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _held = listener.accept().await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    let err = device(port, "1")
        .connect(Duration::from_millis(300))
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("timed out"), "{err}");
}

#[tokio::test]
async fn refused_connection_is_an_io_error() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let err = device(port, "1")
        .connect(Duration::from_secs(2))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, MdcError::Io(_)), "{err}");
}

#[tokio::test]
async fn push_content_enables_standby_then_sends_url() {
    let mock = common::start("1", Behavior::NoDownload).await;
    let switched = device(mock.port, "1")
        .push_content("http://10.0.0.2/c.json")
        .await
        .unwrap();
    // Already on the mobile content player: only read, not set
    assert!(!switched);
    let cmds = mock.commands();
    assert_eq!(cmds[0], (cmd::CONTENT_DOWNLOAD, vec![0x81]));
    assert_eq!(cmds[1], (cmd::NETWORK_STANDBY, vec![0x01]));
    assert_eq!(cmds[2].0, cmd::CONTENT_DOWNLOAD);
    assert_eq!(&cmds[2].1[3..], b"http://10.0.0.2/c.json");
    assert_eq!(cmds.len(), 3);
}

#[tokio::test]
async fn push_content_selects_the_mobile_player_when_needed() {
    // A display left on the USB content player
    let mock = common::start("1", Behavior::UsbPlayer).await;
    let switched = device(mock.port, "1")
        .push_content("http://10.0.0.2/c.json")
        .await
        .unwrap();
    assert!(switched);
    let cmds = mock.commands();
    assert_eq!(
        cmds[1],
        (cmd::CONTENT_DOWNLOAD, vec![0x81, 4]),
        "MobileCMS selected"
    );
    assert_eq!(cmds[2], (cmd::NETWORK_STANDBY, vec![0x01]));
    assert_eq!(&cmds[3].1[3..], b"http://10.0.0.2/c.json");
}

#[tokio::test]
async fn power_off_sets_standby_then_powers_down() {
    let mock = common::start("1", Behavior::Normal).await;
    device(mock.port, "1").power_off(false).await.unwrap();
    device(mock.port, "1").power_off(true).await.unwrap();
    assert_eq!(
        mock.commands(),
        vec![
            (cmd::NETWORK_STANDBY, vec![0x00]),
            (cmd::POWER, vec![0x00]),
            (cmd::NETWORK_STANDBY, vec![0x01]),
            (cmd::POWER, vec![0x00]),
        ]
    );
}

#[tokio::test]
async fn one_shot_helpers_report_errors() {
    let mock = common::start("123456", Behavior::Normal).await;
    let err = device(mock.port, "000000")
        .power_off(false)
        .await
        .unwrap_err();
    assert!(matches!(err, MdcError::AuthFailed(_)));
    let err = device(mock.port, "123456")
        .push_content("")
        .await
        .unwrap_err();
    assert!(matches!(err, MdcError::InvalidArgument(_)));
}

#[tokio::test]
async fn sends_settings_and_reset_commands() {
    use emdx_controller::mdc::{sub, ResetKind, SleepTime, StorageKind};
    let mock = common::start("123456", Behavior::NoDownload).await;
    let mut s = device(mock.port, "123456")
        .connect(Duration::from_secs(5))
        .await
        .unwrap();
    s.set_sleep_time(SleepTime::Minutes(30)).await.unwrap();
    s.set_daily_refresh(Some((23, 5))).await.unwrap();
    s.set_portrait(true).await.unwrap();
    s.empty_storage(StorageKind::Files).await.unwrap();
    s.turn_on_ble(2000).await.unwrap();
    assert!(s.reset(ResetKind::Settings).await.unwrap(), "acknowledged");
    assert!(s.reset(ResetKind::All).await.unwrap());
    s.close().await;

    let cmds = mock.commands();
    assert_eq!(cmds[0], (cmd::SLEEP_TIME, vec![sub::SLEEP_TIME, 9]));
    assert_eq!(
        cmds[1],
        (cmd::BATTERY, vec![sub::DAILY_REFRESH, 0x80, 2, 23, 5])
    );
    assert_eq!(cmds[2], (cmd::ORIENTATION, vec![sub::ORIENTATION, 1]));
    assert_eq!(cmds[3], (cmd::BATTERY, vec![sub::EMPTY_STORAGE, 0, 0]));
    // Same bytes as the ePaper app's MDCTurnOnBLE.Set(2000)
    assert_eq!(
        cmds[4],
        (cmd::BLUETOOTH, vec![0x10, 0x00, 3, 0x07, 0xD0, 0xD2])
    );
    assert_eq!(cmds[5], (cmd::RESET_CONTROL, vec![2]));
    assert_eq!(cmds[6], (cmd::RESET_CONTROL, vec![3]));
}

#[tokio::test]
async fn sends_the_background_color_like_the_app() {
    use emdx_controller::mdc::{bg_color, sub};
    let mock = common::start("123456", Behavior::Normal).await;
    let mut s = device(mock.port, "123456")
        .connect(Duration::from_secs(5))
        .await
        .unwrap();

    // A query first: a display that was never asked shows white
    assert_eq!(s.background_color().await.unwrap(), bg_color::WHITE);
    // The app's MDCBackgroundColorCommand.Set(green): `AA 21 00 03 90 00 03 B7`
    s.set_background_color(bg_color::GREEN).await.unwrap();
    assert_eq!(s.background_color().await.unwrap(), bg_color::GREEN);
    s.close().await;

    assert_eq!(
        mock.sent(cmd::SCREEN),
        vec![
            vec![sub::BG_COLOR],
            vec![sub::BG_COLOR, 0x00, bg_color::GREEN],
            vec![sub::BG_COLOR],
        ]
    );
}

#[tokio::test]
async fn refuses_background_colors_the_app_does_not_know() {
    use emdx_controller::mdc::bg_color;
    let mock = common::start("123456", Behavior::Normal).await;
    let mut s = device(mock.port, "123456")
        .connect(Duration::from_secs(5))
        .await
        .unwrap();
    // The app's own parser knows only 0..=5, so emdx sends nothing else
    for value in [bg_color::YELLOW + 1, 200] {
        let err = s.set_background_color(value).await.unwrap_err();
        assert!(matches!(err, MdcError::InvalidArgument(_)), "{err}");
    }
    s.close().await;
    assert!(mock.sent(cmd::SCREEN).is_empty(), "nothing was sent");
}

#[tokio::test]
async fn an_unconfirmed_reset_is_not_an_error() {
    use emdx_controller::mdc::ResetKind;
    // A display that restarts without replying
    let mock = common::start("123456", Behavior::Silent).await;
    let mut s = device(mock.port, "123456")
        .with_command_timeout(Duration::from_millis(300))
        .connect(Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        !s.reset(ResetKind::All).await.unwrap(),
        "sent, not confirmed"
    );
}

#[tokio::test]
async fn changed_pin_is_needed_from_the_next_session() {
    let mock = common::start("123456", Behavior::Normal).await;
    let mut s = device(mock.port, "123456")
        .connect(Duration::from_secs(5))
        .await
        .unwrap();
    s.set_pin("654321").await.unwrap();
    s.close().await;
    // The app's frame: 1B 87 then the digits as text
    assert_eq!(mock.sent(cmd::BATTERY).last().unwrap(), b"\x87654321");

    let new = device(mock.port, "654321")
        .connect(Duration::from_secs(5))
        .await;
    assert!(new.is_ok(), "the new PIN logs in");
    new.unwrap().close().await;
    let old = device(mock.port, "123456")
        .connect(Duration::from_secs(5))
        .await;
    assert!(
        matches!(old, Err(MdcError::AuthFailed(_))),
        "the old PIN is refused"
    );
}
