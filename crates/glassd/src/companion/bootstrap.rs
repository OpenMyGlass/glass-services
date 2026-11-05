//! Wi-Fi QR bootstrap decode (`OMG1:` payload) and the Wi-Fi association config it drives.
//!
//! The companion shows a QR; the Glass camera scans it; glassd decodes the `OMG1:` string with
//! [`omg_protocol::bootstrap::WifiBootstrap::decode`] (the same type the companion encodes, so
//! the two cannot drift), brings Wi-Fi up with the merged radio control, joins the network, and
//! then connects to the companion at `companion_addr` (or discovers it later via mDNS).
//!
//! The camera-side QR scan is a later phase; this module implements the decode + join planning
//! and exposes [`decode`] so a decoded payload can be fed in directly (see
//! [`crate::companion::Companion::bootstrap`]). The actual radio/association steps live in
//! [`crate::radio::Radio::join_wifi`], which this module builds the supplicant config for.

pub use omg_protocol::bootstrap::{BootstrapError, WifiAuth, WifiBootstrap};

/// Decodes an `OMG1:` Wi-Fi bootstrap string.
pub fn decode(s: &str) -> Result<WifiBootstrap, BootstrapError> {
    WifiBootstrap::decode(s)
}

/// Why a network from a QR cannot be written to the supplicant's config. The values come from
/// outside, so they must not be able to add config lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadNetwork {
    /// An SSID is 1 to 32 bytes.
    SsidLength(usize),
    /// A WPA passphrase is 8 to 63 bytes.
    PassphraseLength(usize),
    /// A WEP key is 5, 13 or 16 bytes.
    WepKeyLength(usize),
    /// A password holds a control character, which the supplicant refuses anyway.
    ControlCharacter,
    /// A passphrase holds a `"` before a `#`, which cannot be written (see [`quoted_passphrase`]).
    QuoteBeforeHash,
}

impl std::fmt::Display for BadNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BadNetwork::SsidLength(n) => write!(f, "SSID of {n} bytes"),
            BadNetwork::PassphraseLength(n) => write!(f, "passphrase of {n} bytes"),
            BadNetwork::WepKeyLength(n) => write!(f, "WEP key of {n} bytes"),
            BadNetwork::ControlCharacter => write!(f, "control character in the password"),
            BadNetwork::QuoteBeforeHash => write!(f, "a \" before a # in the passphrase"),
        }
    }
}

/// A `wpa_supplicant.conf` network block for `ssid`/`auth`/`psk`. Open networks use
/// `key_mgmt=NONE`.
///
/// A quoted value in this file honours no escapes: `wpa_config_parse_string` (2.11,
/// `src/utils/common.c`) takes everything from the first `"` to the last one as it is. So nothing
/// is escaped. The SSID, a WEP key and a WPA3 password go in hex, which that same function reads
/// and no value can break out of. A WPA passphrase cannot, because `psk=` takes hex only as a
/// finished PMK: see [`quoted_passphrase`].
pub fn wpa_supplicant_conf(
    ssid: &str,
    auth: WifiAuth,
    psk: Option<&str>,
) -> Result<String, BadNetwork> {
    if ssid.is_empty() || ssid.len() > 32 {
        return Err(BadNetwork::SsidLength(ssid.len()));
    }
    let mut out = String::from("ctrl_interface=/var/run/wpa_supplicant\nupdate_config=1\n\n");
    out.push_str("network={\n");
    out.push_str(&format!("\tssid={}\n", hex(ssid)));
    match (auth, psk) {
        (WifiAuth::Open, _) | (_, None) => {
            out.push_str("\tkey_mgmt=NONE\n");
        }
        (WifiAuth::Wep, Some(key)) => {
            if ![5, 13, 16].contains(&key.len()) {
                return Err(BadNetwork::WepKeyLength(key.len()));
            }
            no_control_characters(key)?;
            out.push_str("\tkey_mgmt=NONE\n");
            out.push_str(&format!("\twep_key0={}\n", hex(key)));
            out.push_str("\twep_tx_keyidx=0\n");
        }
        (WifiAuth::Wpa3Sae, Some(key)) => {
            if key.is_empty() {
                return Err(BadNetwork::PassphraseLength(0));
            }
            no_control_characters(key)?;
            out.push_str("\tkey_mgmt=SAE\n");
            out.push_str("\tieee80211w=2\n");
            out.push_str(&format!("\tsae_password={}\n", hex(key)));
        }
        (WifiAuth::WpaPsk, Some(key)) => {
            out.push_str("\tkey_mgmt=WPA-PSK\n");
            out.push_str(&format!("\tpsk={}\n", quoted_passphrase(key)?));
        }
    }
    out.push_str("}\n");
    Ok(out)
}

/// `psk="..."`, the one form a WPA passphrase has (`wpa_config_parse_psk`, 2.11): 8 to 63 bytes,
/// no control characters, taken as it is, so `"` and `\` need nothing.
///
/// One case has no spelling. `wpa_config_get_line` (`src/utils/config.c`) pairs up quotes from the
/// start of the line to decide whether a `#` begins a comment, so after an odd number of `"` in
/// the passphrase a `#` would cut the line there. That is refused rather than joined wrongly; the
/// way out, if a network ever needs it, is to compute the PMK and write `psk=<64 hex>`.
fn quoted_passphrase(key: &str) -> Result<String, BadNetwork> {
    if !(8..=63).contains(&key.len()) {
        return Err(BadNetwork::PassphraseLength(key.len()));
    }
    no_control_characters(key)?;
    let mut in_quotes = true;
    for b in key.bytes() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'#' if !in_quotes => return Err(BadNetwork::QuoteBeforeHash),
            _ => {}
        }
    }
    Ok(format!("\"{key}\""))
}

/// The same test the supplicant makes (`has_ctrl_char`). Bytes of UTF-8 above ASCII pass.
fn no_control_characters(s: &str) -> Result<(), BadNetwork> {
    if s.bytes().any(|b| b < 32 || b == 127) {
        return Err(BadNetwork::ControlCharacter);
    }
    Ok(())
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_what_the_companion_encodes() {
        let mut b = WifiBootstrap::wpa("Casa-Giorgio", "s3cr3t");
        b.companion_addr = Some("10.10.10.50:7700".into());
        b.pair_token = Some("tok-abc".into());
        let s = b.encode();
        assert!(s.starts_with("OMG1:"));
        assert_eq!(decode(&s).unwrap(), b);
    }

    #[test]
    fn rejects_a_plain_wifi_uri() {
        assert_eq!(decode("WIFI:S:x;;"), Err(BootstrapError::BadPrefix));
    }

    #[test]
    fn wpa_conf_has_psk_and_ssid() {
        let c = wpa_supplicant_conf("Net", WifiAuth::WpaPsk, Some("password")).unwrap();
        assert!(c.contains("\tssid=4e6574\n"));
        assert!(c.contains("key_mgmt=WPA-PSK"));
        assert!(c.contains("\tpsk=\"password\"\n"));
    }

    #[test]
    fn open_conf_is_key_mgmt_none() {
        let c = wpa_supplicant_conf("Guest", WifiAuth::Open, None).unwrap();
        assert!(c.contains("key_mgmt=NONE"));
        assert!(!c.contains("psk="));
    }

    #[test]
    fn sae_conf_requests_pmf() {
        let c = wpa_supplicant_conf("Net6", WifiAuth::Wpa3Sae, Some("pass")).unwrap();
        assert!(c.contains("key_mgmt=SAE"));
        assert!(c.contains("ieee80211w=2"));
        assert!(c.contains("\tsae_password=70617373\n"));
    }

    /// Whatever the QR says, the block has these lines and no others.
    fn keys_of(conf: &str) -> Vec<&str> {
        conf.lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim()))
            .collect()
    }

    #[test]
    fn an_ssid_cannot_add_config_lines() {
        let evil = "a\"\n\tkey_mgmt=NONE\n}\nx";
        let c = wpa_supplicant_conf(evil, WifiAuth::WpaPsk, Some("password")).unwrap();
        assert_eq!(c.lines().count(), 8);
        assert_eq!(
            keys_of(&c),
            [
                "ctrl_interface",
                "update_config",
                "network",
                "ssid",
                "key_mgmt",
                "psk"
            ]
        );
    }

    #[test]
    fn a_password_cannot_add_config_lines() {
        for auth in [WifiAuth::WpaPsk, WifiAuth::Wpa3Sae] {
            assert_eq!(
                wpa_supplicant_conf("Net", auth, Some("password\"\n\tkey_mgmt=NONE")),
                Err(BadNetwork::ControlCharacter)
            );
            assert_eq!(
                wpa_supplicant_conf("Net", auth, Some("password\r\t")),
                Err(BadNetwork::ControlCharacter)
            );
        }
        assert_eq!(
            wpa_supplicant_conf("Net", WifiAuth::Wep, Some("abc\n5")),
            Err(BadNetwork::ControlCharacter)
        );
    }

    #[test]
    fn quotes_and_backslashes_in_a_passphrase_stay_as_they_are() {
        // The supplicant honours no escapes in a quoted value: anything added would become part
        // of the passphrase, and the network would refuse it.
        let c = wpa_supplicant_conf("Net", WifiAuth::WpaPsk, Some(r#"a"b\c"d#e"#)).unwrap();
        assert!(c.contains(r#"psk="a"b\c"d#e""#));
    }

    #[test]
    fn a_hash_the_supplicant_would_take_for_a_comment_is_refused() {
        assert_eq!(
            wpa_supplicant_conf("Net", WifiAuth::WpaPsk, Some(r#"pass"word#1"#)),
            Err(BadNetwork::QuoteBeforeHash)
        );
        // Inside the quotes it is safe.
        assert!(wpa_supplicant_conf("Net", WifiAuth::WpaPsk, Some("pass#word")).is_ok());
    }

    #[test]
    fn lengths_the_supplicant_would_refuse_are_refused_here() {
        let long = "s".repeat(33);
        assert_eq!(
            wpa_supplicant_conf(&long, WifiAuth::Open, None),
            Err(BadNetwork::SsidLength(33))
        );
        assert_eq!(
            wpa_supplicant_conf("", WifiAuth::Open, None),
            Err(BadNetwork::SsidLength(0))
        );
        assert_eq!(
            wpa_supplicant_conf("Net", WifiAuth::WpaPsk, Some("short")),
            Err(BadNetwork::PassphraseLength(5))
        );
        assert_eq!(
            wpa_supplicant_conf("Net", WifiAuth::WpaPsk, Some(&"p".repeat(64))),
            Err(BadNetwork::PassphraseLength(64))
        );
        assert_eq!(
            wpa_supplicant_conf("Net", WifiAuth::Wep, Some("four")),
            Err(BadNetwork::WepKeyLength(4))
        );
    }

    #[test]
    fn utf8_passes_and_a_hex_ssid_needs_no_quoting() {
        let c = wpa_supplicant_conf("Caffè", WifiAuth::WpaPsk, Some("pàssword")).unwrap();
        assert!(c.contains("\tssid=43616666c3a8\n"));
        assert!(c.contains("\tpsk=\"pàssword\"\n"));
    }
}
