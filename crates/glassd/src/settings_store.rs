//! Persistent settings (docs/api.md "Settings"): one small text file, `key=value` per line,
//! written atomically (temporary file, `fsync`, `rename`, `fsync` of the directory). On
//! start the file is read over the defaults; unknown keys and bad values are ignored, and
//! values are clamped as for `SetSettings`.

use glass_api::hub::settings_rules;
use glass_api::proto::{Locale, Settings};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Environment variable for the state directory.
pub const STATE_DIR_ENV: &str = "GLASSD_STATE_DIR";

/// File name inside the state directory.
pub const FILE_NAME: &str = "settings";

/// `$GLASSD_STATE_DIR`, else `/data/glassd` on Android and `/var/lib/glassd` elsewhere.
pub fn default_state_dir() -> PathBuf {
    match std::env::var_os(STATE_DIR_ENV) {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ if cfg!(target_os = "android") => PathBuf::from("/data/glassd"),
        _ => PathBuf::from("/var/lib/glassd"),
    }
}

/// The file's text for `s`.
pub fn encode(s: &Settings) -> String {
    let d = settings_rules::defaults();
    let mut out = String::from("# glassd settings (docs/api.md \"Settings\"); written by glassd\n");
    let b = |v: Option<bool>, d: Option<bool>| v.or(d).unwrap_or(false);
    out += &format!(
        "head_wake_enabled={}\n",
        b(s.head_wake_enabled, d.head_wake_enabled)
    );
    out += &format!(
        "head_wake_angle_deg={}\n",
        s.head_wake_angle_deg
            .or(d.head_wake_angle_deg)
            .unwrap_or(30.0)
    );
    out += &format!("wink_enabled={}\n", b(s.wink_enabled, d.wink_enabled));
    out += &format!(
        "on_head_detection_enabled={}\n",
        b(s.on_head_detection_enabled, d.on_head_detection_enabled)
    );
    out += &format!(
        "screen_timeout_ms={}\n",
        s.screen_timeout_ms
            .or(d.screen_timeout_ms)
            .unwrap_or(120_000)
    );
    out += &format!(
        "nudge_sleep_enabled={}\n",
        b(s.nudge_sleep_enabled, d.nudge_sleep_enabled)
    );
    out += &format!("wifi_enabled={}\n", b(s.wifi_enabled, d.wifi_enabled));
    out += &format!(
        "bluetooth_enabled={}\n",
        b(s.bluetooth_enabled, d.bluetooth_enabled)
    );
    out += &format!("airplane_mode={}\n", b(s.airplane_mode, d.airplane_mode));
    // Only once chosen: an absent locale is what sends the launcher to its first-run picker.
    if s.locale.is_some() {
        out += &format!("locale={}\n", locale_code(s.locale));
    }
    out += &format!("power_saver={}\n", b(s.power_saver, d.power_saver));
    out += &format!(
        "deep_sleep_enabled={}\n",
        b(s.deep_sleep_enabled, d.deep_sleep_enabled)
    );
    out += &format!(
        "deep_sleep_idle_ms={}\n",
        s.deep_sleep_idle_ms
            .or(d.deep_sleep_idle_ms)
            .unwrap_or(60_000)
    );
    out += &format!("time_24h={}\n", b(s.time_24h, d.time_24h));
    out += &format!(
        "timezone_id={}\n",
        s.timezone_id
            .or(d.timezone_id)
            .unwrap_or(settings_rules::TIMEZONE_DEFAULT)
    );
    out += &format!(
        "audio_volume={}\n",
        s.audio_volume
            .or(d.audio_volume)
            .unwrap_or(settings_rules::AUDIO_VOLUME_DEFAULT)
    );
    out
}

/// The ISO 639-1 code (`it`/`en`/`fr`/`es`) stored for a locale field, defaulting to `en`.
fn locale_code(v: Option<i32>) -> &'static str {
    match v.and_then(|v| Locale::try_from(v).ok()) {
        Some(Locale::It) => "it",
        Some(Locale::Fr) => "fr",
        Some(Locale::Es) => "es",
        // Unspecified / unknown / En all persist as the default "en".
        _ => "en",
    }
}

/// Parses a stored locale code into the proto enum value; `None` for anything unsupported.
fn locale_from_code(v: &str) -> Option<i32> {
    let l = match v.to_ascii_lowercase().as_str() {
        "it" => Locale::It,
        "en" => Locale::En,
        "fr" => Locale::Fr,
        "es" => Locale::Es,
        _ => return None,
    };
    Some(l as i32)
}

/// The defaults, with the values found in `text` merged in (clamped).
pub fn decode(text: &str) -> Settings {
    let mut change = Settings::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        let flag = || v.parse::<bool>().ok();
        match k {
            "head_wake_enabled" => change.head_wake_enabled = flag(),
            "head_wake_angle_deg" => {
                change.head_wake_angle_deg = v.parse::<f32>().ok().filter(|v| v.is_finite())
            }
            "wink_enabled" => change.wink_enabled = flag(),
            "on_head_detection_enabled" => change.on_head_detection_enabled = flag(),
            "screen_timeout_ms" => change.screen_timeout_ms = v.parse().ok(),
            "nudge_sleep_enabled" => change.nudge_sleep_enabled = flag(),
            "wifi_enabled" => change.wifi_enabled = flag(),
            "bluetooth_enabled" => change.bluetooth_enabled = flag(),
            "airplane_mode" => change.airplane_mode = flag(),
            "locale" => change.locale = locale_from_code(v),
            "power_saver" => change.power_saver = flag(),
            "deep_sleep_enabled" => change.deep_sleep_enabled = flag(),
            "deep_sleep_idle_ms" => change.deep_sleep_idle_ms = v.parse().ok(),
            "audio_volume" => change.audio_volume = v.parse().ok(),
            "time_24h" => change.time_24h = v.parse().ok(),
            "timezone_id" => change.timezone_id = v.parse().ok(),
            _ => {}
        }
    }
    let mut s = settings_rules::defaults();
    settings_rules::merge(&mut s, &change);
    s
}

/// The settings file.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    dir: PathBuf,
}

impl SettingsStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        SettingsStore { dir: dir.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    /// The stored settings over the defaults; the defaults alone if there is no file.
    pub fn load(&self) -> io::Result<Settings> {
        match fs::read_to_string(self.path()) {
            Ok(t) => Ok(decode(&t)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(settings_rules::defaults()),
            Err(e) => Err(e),
        }
    }

    /// Writes `s` atomically, creating the directory if needed.
    pub fn save(&self, s: &Settings) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{FILE_NAME}.tmp"));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(encode(s).as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, self.path())?;
        sync_dir(&self.dir);
        Ok(())
    }
}

fn sync_dir(dir: &Path) {
    // Best effort: makes the rename durable on file systems that need it.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("glassd-settings-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn missing_file_gives_defaults() {
        let d = dir("missing");
        let s = SettingsStore::new(&d).load().unwrap();
        assert_eq!(s, settings_rules::defaults());
    }

    #[test]
    fn round_trip_and_atomic_write() {
        let d = dir("roundtrip");
        let st = SettingsStore::new(d.join("nested"));
        let mut s = settings_rules::defaults();
        s.head_wake_angle_deg = Some(22.5);
        s.wink_enabled = Some(true);
        s.screen_timeout_ms = Some(60_000);
        s.nudge_sleep_enabled = Some(true);
        st.save(&s).unwrap();
        assert!(!d.join("nested/settings.tmp").exists());
        assert_eq!(st.load().unwrap(), s);
        // Overwrite.
        s.on_head_detection_enabled = Some(true);
        st.save(&s).unwrap();
        assert_eq!(st.load().unwrap(), s);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn bad_values_unknown_keys_and_clamping() {
        let s = decode(
            "# c\nhead_wake_enabled=maybe\nhead_wake_angle_deg=90\nscreen_timeout_ms=10\n\
             nudge_sleep_enabled = true\nfuture_key=1\nhead_wake_angle_deg_x=3\ngarbage\n",
        );
        let d = settings_rules::defaults();
        assert_eq!(s.head_wake_enabled, d.head_wake_enabled);
        assert_eq!(s.head_wake_angle_deg, Some(40.0));
        assert_eq!(s.screen_timeout_ms, Some(3_000));
        assert_eq!(s.nudge_sleep_enabled, Some(true));
        assert_eq!(s.wink_enabled, Some(false));
        assert_eq!(
            decode("head_wake_angle_deg=NaN").head_wake_angle_deg,
            Some(30.0)
        );
        assert!(!s.snapshot);
    }

    #[test]
    fn encode_lists_every_field() {
        let t = encode(&settings_rules::defaults());
        for k in [
            "head_wake_enabled=false",
            "head_wake_angle_deg=30",
            "wink_enabled=false",
            "on_head_detection_enabled=true",
            "screen_timeout_ms=120000",
            "nudge_sleep_enabled=false",
            "wifi_enabled=false",
            "bluetooth_enabled=false",
            "airplane_mode=false",
            "power_saver=true",
            "deep_sleep_enabled=false",
            "deep_sleep_idle_ms=60000",
            "audio_volume=87",
            "time_24h=true",
            "timezone_id=2",
        ] {
            assert!(t.lines().any(|l| l == k), "{k} in {t}");
        }
    }

    #[test]
    fn energy_policy_settings_round_trip() {
        let s = decode("power_saver=false\ndeep_sleep_enabled=true\ndeep_sleep_idle_ms=120000\n");
        assert_eq!(s.power_saver, Some(false));
        assert_eq!(s.deep_sleep_enabled, Some(true));
        assert_eq!(s.deep_sleep_idle_ms, Some(120_000));
        // Out-of-range idle is clamped through the merge rules.
        let s = decode("deep_sleep_idle_ms=1\n");
        assert_eq!(
            s.deep_sleep_idle_ms,
            Some(settings_rules::DEEP_SLEEP_IDLE_MIN_MS)
        );
        // Defaults when absent.
        let d = settings_rules::defaults();
        assert_eq!(decode("").power_saver, d.power_saver);
        assert_eq!(decode("").deep_sleep_enabled, d.deep_sleep_enabled);
    }

    #[test]
    fn locale_round_trips_and_rejects_unsupported() {
        use glass_api::proto::Locale;
        let s = decode("locale=it\n");
        assert_eq!(s.locale, Some(Locale::It as i32));
        assert_eq!(decode("locale=FR\n").locale, Some(Locale::Fr as i32));
        // Unsupported or malformed codes keep the default (en).
        let d = settings_rules::defaults();
        assert_eq!(decode("locale=de\n").locale, d.locale);
        assert_eq!(decode("locale=\n").locale, d.locale);
        assert_eq!(d.locale, None);
        assert!(!encode(&d).contains("locale="));
        // Full file round trip.
        let dir = dir("locale");
        let st = SettingsStore::new(&dir);
        let mut want = settings_rules::defaults();
        want.locale = Some(Locale::Es as i32);
        st.save(&want).unwrap();
        assert_eq!(st.load().unwrap().locale, Some(Locale::Es as i32));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn radio_settings_round_trip() {
        let s = decode("wifi_enabled=true\nbluetooth_enabled=true\nairplane_mode=true\n");
        assert_eq!(s.wifi_enabled, Some(true));
        assert_eq!(s.bluetooth_enabled, Some(true));
        assert_eq!(s.airplane_mode, Some(true));
        // Defaults when absent.
        let d = settings_rules::defaults();
        assert_eq!(decode("").wifi_enabled, d.wifi_enabled);
        assert_eq!(decode("").airplane_mode, Some(false));
    }
}
