//! Persistent settings (docs/api.md "Settings"): one small text file, `key=value` per line,
//! written atomically (temporary file, `fsync`, `rename`, `fsync` of the directory). On
//! start the file is read over the defaults; unknown keys and bad values are ignored, and
//! values are clamped as for `SetSettings`.

use glass_api::hub::settings_rules;
use glass_api::proto::Settings;
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
            .unwrap_or(15_000)
    );
    out += &format!(
        "nudge_sleep_enabled={}\n",
        b(s.nudge_sleep_enabled, d.nudge_sleep_enabled)
    );
    out
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
            "head_wake_enabled=true",
            "head_wake_angle_deg=30",
            "wink_enabled=false",
            "on_head_detection_enabled=false",
            "screen_timeout_ms=15000",
            "nudge_sleep_enabled=false",
        ] {
            assert!(t.lines().any(|l| l == k), "{k} in {t}");
        }
    }
}
