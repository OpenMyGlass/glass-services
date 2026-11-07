//! Starts or stops a Wi-Fi setup session on a running glassd, and prints its states, for testing
//! without the launcher. This prints the access point's password, which is its job: do not send
//! its output to a log.
//!
//! ```text
//! GLASS_SERVICES_SOCKET=/tmp/g.sock cargo run -p glass-api --example wifi-setup -- start
//! ```
//!
//! `start` turns on `wifi_setup_ap_enabled`, asks for a session and prints until it ends. `ask`
//! does the same without touching the setting, to see the refusal. `stop` asks to end it and
//! leaves. The setting stays as it is afterwards.

use std::time::Duration;

use glass_api::Client;
use glass_api::proto::{
    SetSettings, Settings, Topic, WifiSetupPhase, WifiSetupRequest, client_frame, service_frame,
};

fn main() {
    let (start, enable) = match std::env::args().nth(1).as_deref() {
        Some("start") => (true, true),
        Some("ask") => (true, false),
        Some("stop") => (false, false),
        _ => panic!("usage: wifi-setup start|ask|stop"),
    };
    let mut c = Client::connect_as(glass_api::socket_path(), "wifi-setup").expect("connect");
    c.subscribe(&[Topic::WifiSetup]).expect("subscribe");
    if enable {
        c.send(client_frame::Body::SetSettings(SetSettings {
            settings: Some(Settings {
                wifi_setup_ap_enabled: Some(true),
                ..Default::default()
            }),
        }))
        .expect("turn the setting on");
    }
    c.send(client_frame::Body::WifiSetup(WifiSetupRequest { start }))
        .expect("send the request");
    if !start {
        return;
    }
    loop {
        let frame = c
            .recv(Some(Duration::from_secs(900)))
            .expect("read from glassd")
            .expect("no answer from glassd");
        let Some(service_frame::Body::WifiSetupState(s)) = frame.body else {
            continue;
        };
        println!(
            "phase={:?} ssid={} password={} url={} seconds_left={} clients={} network={} error={}",
            s.phase(),
            s.ssid,
            s.password,
            s.url,
            s.seconds_left,
            s.clients,
            s.network,
            s.error
        );
        let ended = matches!(
            s.phase(),
            WifiSetupPhase::Off | WifiSetupPhase::Joined | WifiSetupPhase::Failed
        );
        if ended && !s.snapshot {
            return;
        }
    }
}
