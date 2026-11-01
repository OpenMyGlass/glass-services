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

/// A `wpa_supplicant.conf` network block for `ssid`/`auth`/`psk`. Open networks use
/// `key_mgmt=NONE`; PSK networks embed the passphrase. Special characters are not expected in
/// the QR-provided values, but the SSID and PSK are quoted as `wpa_supplicant` requires.
pub fn wpa_supplicant_conf(ssid: &str, auth: WifiAuth, psk: Option<&str>) -> String {
    let mut out = String::from("ctrl_interface=/var/run/wpa_supplicant\nupdate_config=1\n\n");
    out.push_str("network={\n");
    out.push_str(&format!("\tssid=\"{}\"\n", escape(ssid)));
    match (auth, psk) {
        (WifiAuth::Open, _) | (_, None) => {
            out.push_str("\tkey_mgmt=NONE\n");
        }
        (WifiAuth::Wep, Some(key)) => {
            out.push_str("\tkey_mgmt=NONE\n");
            out.push_str(&format!("\twep_key0=\"{}\"\n", escape(key)));
            out.push_str("\twep_tx_keyidx=0\n");
        }
        (WifiAuth::Wpa3Sae, Some(key)) => {
            out.push_str("\tkey_mgmt=SAE\n");
            out.push_str("\tieee80211w=2\n");
            out.push_str(&format!("\tpsk=\"{}\"\n", escape(key)));
        }
        (WifiAuth::WpaPsk, Some(key)) => {
            out.push_str("\tkey_mgmt=WPA-PSK\n");
            out.push_str(&format!("\tpsk=\"{}\"\n", escape(key)));
        }
    }
    out.push_str("}\n");
    out
}

/// Escapes `"` and `\` for a quoted `wpa_supplicant` string.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
        let c = wpa_supplicant_conf("Net", WifiAuth::WpaPsk, Some("pass"));
        assert!(c.contains("ssid=\"Net\""));
        assert!(c.contains("key_mgmt=WPA-PSK"));
        assert!(c.contains("psk=\"pass\""));
    }

    #[test]
    fn open_conf_is_key_mgmt_none() {
        let c = wpa_supplicant_conf("Guest", WifiAuth::Open, None);
        assert!(c.contains("key_mgmt=NONE"));
        assert!(!c.contains("psk="));
    }

    #[test]
    fn sae_conf_requests_pmf() {
        let c = wpa_supplicant_conf("Net6", WifiAuth::Wpa3Sae, Some("pass"));
        assert!(c.contains("key_mgmt=SAE"));
        assert!(c.contains("ieee80211w=2"));
    }
}
