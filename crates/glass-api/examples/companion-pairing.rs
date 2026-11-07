//! Opens or closes the pairing mode of a running glassd, or forgets its bonded phone, for testing
//! before the launcher has its card. Then prints what `TOPIC_COMPANION` says.
//!
//! ```text
//! GLASS_SERVICES_SOCKET=/run/glassd/glassd.sock cargo run -p glass-api --example companion-pairing -- open [ms]
//! ... -- close
//! ... -- forget
//! ```

use std::time::Duration;

use glass_api::Client;
use glass_api::proto::{
    CompanionForgetBond, CompanionPairingWindow, Topic, client_frame, service_frame,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let body = match args.next().as_deref() {
        Some("open") => client_frame::Body::CompanionPairingWindow(CompanionPairingWindow {
            open: true,
            duration_ms: args
                .next()
                .map_or(0, |ms| ms.parse().expect("a number of ms")),
        }),
        Some("close") => client_frame::Body::CompanionPairingWindow(CompanionPairingWindow {
            open: false,
            duration_ms: 0,
        }),
        Some("forget") => client_frame::Body::CompanionForgetBond(CompanionForgetBond {}),
        _ => {
            eprintln!("usage: companion-pairing open [ms] | close | forget");
            std::process::exit(2);
        }
    };
    let mut client = Client::connect_as(glass_api::socket_path(), "companion-pairing").unwrap();
    client.subscribe(&[Topic::Companion]).unwrap();
    client.send(body).unwrap();
    // The snapshot, then the change if there is one, or the error.
    while let Some(frame) = client.recv(Some(Duration::from_secs(2))).unwrap() {
        match frame.body {
            Some(service_frame::Body::CompanionState(s)) => println!(
                "paired: {} {:?}, pairing open until {} us (0: closed)",
                s.paired, s.paired_name, s.pairing_until_us
            ),
            Some(service_frame::Body::Error(e)) => {
                eprintln!("refused: {:?} {}", e.code(), e.message);
                std::process::exit(1);
            }
            _ => {}
        }
    }
}
