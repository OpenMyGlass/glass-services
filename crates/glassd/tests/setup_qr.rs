//! The setup QR's validation and glassd's supplicant config must agree: whatever `glass-qr` lets
//! through for the user to confirm, glassd must be able to write. Otherwise a code would be offered
//! and then fail after the tap. Passwords here are made up.

use glass_qr::setup::{self, TAG_WIFI_AUTH, TAG_WIFI_PASSWORD, TAG_WIFI_SSID};
use glassd::companion::bootstrap::{WifiAuth, wpa_supplicant_conf};

fn body(ssid: &[u8], auth: u8, password: &[u8]) -> Vec<u8> {
    let mut b = vec![TAG_WIFI_SSID, ssid.len() as u8];
    b.extend_from_slice(ssid);
    b.extend_from_slice(&[TAG_WIFI_AUTH, 1, auth]);
    if auth != 0 {
        b.extend_from_slice(&[TAG_WIFI_PASSWORD, password.len() as u8]);
        b.extend_from_slice(password);
    }
    b
}

fn conf_of(s: &setup::Setup) -> Result<String, String> {
    let w = s.wifi().expect("a network");
    let auth = match w.auth {
        setup::WifiAuth::Open => WifiAuth::Open,
        setup::WifiAuth::Wep => WifiAuth::Wep,
        setup::WifiAuth::WpaPsk => WifiAuth::WpaPsk,
        setup::WifiAuth::Wpa3Sae => WifiAuth::Wpa3Sae,
    };
    wpa_supplicant_conf(w.ssid, auth, w.password).map_err(|e| e.to_string())
}

/// Passwords built to sit on each edge of the two sets of rules, over every kind of network.
#[test]
fn what_the_qr_accepts_glassd_can_write() {
    let alphabet: &[u8] = b"aZ0 \"#\\'=;,\x7f\n\t";
    let mut state = 0x1234_5678_9abc_def1u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as usize
    };
    let (mut accepted, mut refused) = (0, 0);
    for round in 0..40_000 {
        let auth = (round % 4) as u8;
        let len = match next() % 4 {
            0 => [5, 13, 16][next() % 3],
            1 => [7, 8, 63, 64][next() % 4],
            2 => [1, 128, 129, 200][next() % 4],
            _ => next() % 70,
        };
        let password: Vec<u8> = (0..len)
            .map(|_| match next() % 4 {
                0 => alphabet[next() % alphabet.len()],
                _ => b'a' + (next() % 26) as u8,
            })
            .collect();
        match setup::parse(setup::seal(&body(b"Test-Network", auth, &password)).as_bytes()) {
            Ok(s) => {
                accepted += 1;
                if let Err(e) = conf_of(&s) {
                    panic!("accepted but cannot be written: auth {auth}, {e}");
                }
            }
            Err(_) => refused += 1,
        }
    }
    // Both sides of the edge were exercised.
    assert!(accepted > 1000 && refused > 1000, "{accepted} {refused}");
}

#[test]
fn an_ssid_from_the_qr_stays_one_line_of_hex() {
    let s = setup::parse(setup::seal(&body("Caffè \"x\"".as_bytes(), 0, b"")).as_bytes()).unwrap();
    assert!(conf_of(&s).unwrap().contains("\tssid=436166"));
}
