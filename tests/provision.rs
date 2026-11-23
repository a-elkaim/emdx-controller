//! MDC phase of first-time setup against the mock display.

mod common;

use std::{sync::atomic::Ordering, time::Duration};

use common::Behavior;
use emdx_controller::{
    ble::session::NetworkPlan,
    mdc::{cmd, sub},
    provision::{finish_phase, Finished, Plan, Timing},
};

fn plan() -> Plan {
    Plan {
        pin: "123456".into(),
        network: NetworkPlan::Wifi {
            ssid: "Home".into(),
            password: "secret".into(),
        },
        device_name: Some("Lobby".into()),
        new_pin: Some("654321".into()),
        settings: Default::default(),
    }
}

fn timing() -> Timing {
    Timing {
        mdc_connect_window: Duration::from_millis(500),
        mdc_retry: Duration::from_millis(50),
        ..Timing::default()
    }
}

async fn finish(port: u16, accept: bool, urls: &mut Vec<String>) -> anyhow::Result<Finished> {
    finish_phase(
        "127.0.0.1",
        port,
        &plan(),
        timing(),
        &mut |_| {},
        &mut |url| {
            urls.push(url.to_string());
            Ok(accept)
        },
    )
    .await
}

/// Position of the first command matching `(command, first data byte)`.
fn position(sent: &[(u8, Vec<u8>)], command: u8, first: Option<u8>) -> usize {
    sent.iter()
        .position(|(c, d)| *c == command && (first.is_none() || d.first().copied() == first))
        .unwrap_or_else(|| panic!("command {command:#x}/{first:?} not sent"))
}

#[tokio::test]
async fn finishes_setup_in_the_apps_order() {
    let mock = common::start("123456", Behavior::Normal).await;
    let mut urls = Vec::new();
    let done = finish(mock.port, true, &mut urls).await.unwrap();
    assert_eq!(done.pin, "654321");
    assert_eq!(done.terms_accepted, Some(true));
    assert_eq!(done.wifi_mac.as_deref(), Some("B0:F2:F6:55:0D:0B"));
    assert_eq!(
        urls,
        ["https://terms.example/epaper/eula?lang=en&model=EM32DX"]
    );
    assert!(mock.terms_agreed.load(Ordering::SeqCst));

    // clock → terms → PIN → content player → setup done → device name (last)
    let sent = mock.commands();
    let clock = position(&sent, cmd::CLOCK, None);
    let terms = position(&sent, cmd::TERMS, Some(sub::TERMS));
    let pin = position(&sent, cmd::BATTERY, Some(sub::PIN));
    let player = sent
        .iter()
        .position(|(c, d)| {
            *c == cmd::CONTENT_DOWNLOAD
                && d == &[
                    sub::CONTENT_PLAYER,
                    emdx_controller::mdc::content_player::MOBILE_CMS,
                ]
        })
        .expect("mobile content player selected");
    let done_at = sent
        .iter()
        .position(|(c, d)| *c == cmd::BATTERY && d == &[sub::SETUP_STATUS, 1])
        .expect("setup marked complete");
    let name = position(&sent, cmd::DEVICE_NAME, Some(b'L'));
    assert!(
        clock < terms && terms < pin && pin < player && player < done_at && done_at < name,
        "{sent:?}"
    );
    assert_eq!(&sent[pin].1[1..], b"654321");
}

#[tokio::test]
async fn declining_the_terms_sends_nothing_but_finishes() {
    let mock = common::start("123456", Behavior::Normal).await;
    let mut urls = Vec::new();
    let done = finish(mock.port, false, &mut urls).await.unwrap();
    assert_eq!(urls.len(), 1, "the user was shown the terms");
    assert_eq!(done.terms_accepted, Some(false));
    assert!(!mock.terms_agreed.load(Ordering::SeqCst));
    assert!(
        !mock
            .commands()
            .iter()
            .any(|(c, d)| *c == cmd::TERMS && d.len() > 1),
        "no acceptance sent"
    );
    assert!(mock
        .commands()
        .iter()
        .any(|(c, d)| *c == cmd::BATTERY && d == &[sub::SETUP_STATUS, 1]));
}

#[tokio::test]
async fn already_accepted_terms_are_not_asked_again() {
    let mock = common::start("123456", Behavior::Normal).await;
    mock.terms_agreed.store(true, Ordering::SeqCst);
    let mut urls = Vec::new();
    let done = finish(mock.port, false, &mut urls).await.unwrap();
    assert!(urls.is_empty());
    assert_eq!(done.terms_accepted, Some(true));
}

#[tokio::test]
async fn wrong_pin_and_unreachable_display() {
    let mock = common::start("111111", Behavior::Normal).await;
    let err = finish(mock.port, true, &mut Vec::new()).await.unwrap_err();
    assert!(err.to_string().contains("MDC login failed"), "{err}");

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let err = finish(port, true, &mut Vec::new()).await.unwrap_err();
    assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
}

#[tokio::test]
async fn applies_the_setup_settings_in_the_apps_order() {
    use emdx_controller::{mdc::SleepTime, provision::SetupSettings};
    let mock = common::start("123456", Behavior::Normal).await;
    let plan = Plan {
        settings: SetupSettings {
            portrait: Some(true),
            sleep_time: Some(SleepTime::Minutes(30)),
            network_standby: Some(false),
            daily_refresh: Some((3, 15)),
        },
        ..plan()
    };
    let mut accept = |_: &str| -> anyhow::Result<bool> { Ok(true) };
    finish_phase(
        "127.0.0.1",
        mock.port,
        &plan,
        timing(),
        &mut |_| {},
        &mut accept,
    )
    .await
    .unwrap();
    let sent = mock.commands();
    let at = |c: u8, d: &[u8]| {
        sent.iter()
            .position(|(sc, sd)| *sc == c && sd.as_slice() == d)
            .unwrap_or_else(|| panic!("{c:02X} {d:02X?} not sent: {sent:?}"))
    };
    // App order: orientation, sleep time, network standby, daily refresh, PIN,
    // content player, setup done
    let orientation = at(cmd::ORIENTATION, &[sub::ORIENTATION, 1]);
    let sleep = at(cmd::SLEEP_TIME, &[sub::SLEEP_TIME, 9]);
    let standby = at(cmd::NETWORK_STANDBY, &[0]);
    let refresh = at(cmd::BATTERY, &[sub::DAILY_REFRESH, 0x80, 2, 3, 15]);
    let pin = sent
        .iter()
        .position(|(c, d)| *c == cmd::BATTERY && d.first() == Some(&sub::PIN))
        .unwrap();
    let done = at(cmd::BATTERY, &[sub::SETUP_STATUS, 1]);
    assert!(
        orientation < sleep && sleep < standby && standby < refresh && refresh < pin && pin < done,
        "{sent:?}"
    );
}

#[tokio::test]
async fn leaves_settings_alone_when_not_asked() {
    let mock = common::start("123456", Behavior::Normal).await;
    let mut accept = |_: &str| -> anyhow::Result<bool> { Ok(true) };
    finish_phase(
        "127.0.0.1",
        mock.port,
        &plan(),
        timing(),
        &mut |_| {},
        &mut accept,
    )
    .await
    .unwrap();
    let touched = mock
        .commands()
        .iter()
        .any(|(c, _)| [cmd::ORIENTATION, cmd::SLEEP_TIME, cmd::NETWORK_STANDBY].contains(c));
    assert!(!touched, "no setting changed without being asked");
}
