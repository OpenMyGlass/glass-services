//! Model B energy policy (glass-kernel `docs/model-b-energy.md`, the `modelb-power` variant).
//!
//! glassd owns the idle -> low-power policy on plain Linux. This module has two halves:
//!
//! - [`PowerPolicy`]: a pure state machine. The screen policy ([`crate::screen`]) already blanks
//!   the panel (`FBIOBLANK`) on its own timeout; this adds the stages *on top* of screen-off:
//!   drop the cpufreq profile and panel brightness, and after a longer idle window let the
//!   system suspend to RAM. It decides *when*; the daemon applies the effects.
//! - [`Power`]: the side-effect back end that drives the documented sysfs/helper hooks. Like
//!   [`crate::radio::Radio`] it is injectable: [`Power::system`] performs the real writes and the
//!   real suspend, [`Power::recording`] only records them, so `--mock` and the tests never write
//!   the host's `/sys/power/state`, `/sys/power/wake_lock`, brightness or cpufreq.
//!
//! This 3.4 kernel has **no `PM_AUTOSLEEP`**: opportunistic suspend is the Android wakelock path.
//! glassd holds a named wakelock ([`WAKELOCK`]) while it wants the system awake; to suspend it
//! releases the lock and writes `mem` to `/sys/power/state`, and the kernel suspends once no lock
//! is held. A wake source (power button, touchpad, ...) resumes the system, which delivers input
//! that wakes glassd's poll loop; glassd then re-takes the lock. Everything is best effort: a
//! missing path is logged, never fatal (on Android the framework owns power and this is inert).

use crate::{info, warn};
use glass_api::proto::Screen;
use std::io;
use std::path::{Path, PathBuf};

/// The wakelock glassd holds while it wants the system awake (`/sys/power/wake_lock`).
pub const WAKELOCK: &str = "glassd-active";

/// A cpufreq profile, applied through the kernel's `omg-tune.sh` helper (the hook named in
/// `docs/model-b-energy.md` §4). Only the profiles that helper accepts are used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuProfile {
    /// Responsive, for when the screen is on (`omg-tune.sh balanced`).
    Balanced,
    /// Lightest, for idle / screen-off (`omg-tune.sh eco`, caps 600 MHz).
    Eco,
}

impl CpuProfile {
    /// The `omg-tune.sh` subcommand for this profile.
    pub fn arg(self) -> &'static str {
        match self {
            CpuProfile::Balanced => "balanced",
            CpuProfile::Eco => "eco",
        }
    }
}

/// The sysfs paths and helper the energy policy drives. Defaults are the Model B
/// (`modelb-power`) paths from `docs/model-b-energy.md`; nodes marked *verify* there can be
/// overridden (`--power-*`) once confirmed on a `modelb-power` boot.
#[derive(Debug, Clone)]
pub struct PowerPaths {
    /// `/sys/power/state` (write `mem` to suspend to RAM).
    pub state: PathBuf,
    /// `/sys/power/wake_lock` (take a named wakelock).
    pub wake_lock: PathBuf,
    /// `/sys/power/wake_unlock` (release a named wakelock).
    pub wake_unlock: PathBuf,
    /// Panel brightness (`.../panel-notle-dpi/brightness`); `None` leaves brightness alone.
    pub brightness: Option<PathBuf>,
    /// `power/control` of peripherals to set to `auto` (runtime-PM autosuspend). Best effort.
    pub runtime_pm: Vec<PathBuf>,
    /// Devices whose `power/wakeup` to arm (`enabled`) before suspend. Often already armed by
    /// their drivers; empty by default (see `docs/model-b-energy.md` §1, "verify").
    pub wakeup: Vec<PathBuf>,
    /// The cpufreq helper to run (`docs/model-b-energy.md` §4); found on `PATH`.
    pub cpu_tune: String,
}

/// `/sys/power/state`.
pub const DEFAULT_STATE: &str = "/sys/power/state";
/// `/sys/power/wake_lock`.
pub const DEFAULT_WAKE_LOCK: &str = "/sys/power/wake_lock";
/// `/sys/power/wake_unlock`.
pub const DEFAULT_WAKE_UNLOCK: &str = "/sys/power/wake_unlock";
/// Panel brightness (`docs/model-b-energy.md` §2, *verify*).
pub const DEFAULT_BRIGHTNESS: &str =
    "/sys/devices/platform/omapdss/manager2/panel-notle-dpi/brightness";
/// The cpufreq helper (`docs/model-b-energy.md` §4).
pub const DEFAULT_CPU_TUNE: &str = "omg-tune.sh";
/// Peripheral buses whose devices get `power/control = auto` by default (`docs/model-b-energy.md`
/// §5). Each existing `<bus>/devices/*/power/control` is set to `auto`, best effort.
pub const DEFAULT_RUNTIME_PM_BUSES: &[&str] = &[
    "/sys/bus/usb/devices",
    "/sys/bus/mmc/devices",
    "/sys/bus/i2c/devices",
    "/sys/bus/sdio/devices",
];

impl Default for PowerPaths {
    fn default() -> Self {
        PowerPaths {
            state: DEFAULT_STATE.into(),
            wake_lock: DEFAULT_WAKE_LOCK.into(),
            wake_unlock: DEFAULT_WAKE_UNLOCK.into(),
            brightness: Some(DEFAULT_BRIGHTNESS.into()),
            runtime_pm: Vec::new(),
            wakeup: Vec::new(),
            cpu_tune: DEFAULT_CPU_TUNE.into(),
        }
    }
}

impl PowerPaths {
    /// Expands [`DEFAULT_RUNTIME_PM_BUSES`] into the `power/control` files that exist now, so idle
    /// USB/MMC/I2C/SDIO devices autosuspend. Best effort: a bus that is absent is skipped.
    pub fn with_default_runtime_pm(mut self) -> Self {
        for bus in DEFAULT_RUNTIME_PM_BUSES {
            let dir = Path::new(bus);
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in entries.flatten() {
                let ctl = e.path().join("power/control");
                if ctl.exists() {
                    self.runtime_pm.push(ctl);
                }
            }
        }
        self
    }
}

/// Where the energy policy is in the idle -> low-power sequence. The screen state (on/dim/off) is
/// owned by [`crate::screen`]; this tracks only the suspend stage on top of screen-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// Screen on (or the policy is off): the system is kept awake.
    Active,
    /// Screen off, awake: eco profile, waiting out the deep-idle window before suspend.
    Idle,
    /// Suspend has been requested (wakelock released, `mem` written); waiting for a wake source.
    Suspended,
}

/// The pure idle -> suspend decision. Fed the screen state and the settings; it reports when to
/// suspend and which cpufreq profile fits.
#[derive(Debug)]
pub struct PowerPolicy {
    power_saver: bool,
    deep_sleep: bool,
    idle_ms: u32,
    screen_off: bool,
    /// When the screen last went off (`CLOCK_MONOTONIC` µs); the suspend window counts from here.
    off_since_us: Option<u64>,
    state: PowerState,
}

/// What [`PowerPolicy::on_timeout`] asks the daemon to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    /// The deep-idle window elapsed: run a suspend cycle.
    Suspend,
}

impl PowerPolicy {
    /// From the current settings (clamped values as stored).
    pub fn new(power_saver: bool, deep_sleep: bool, idle_ms: u32) -> PowerPolicy {
        PowerPolicy {
            power_saver,
            deep_sleep,
            idle_ms,
            screen_off: false,
            off_since_us: None,
            state: PowerState::Active,
        }
    }

    pub fn state(&self) -> PowerState {
        self.state
    }

    pub fn is_suspended(&self) -> bool {
        self.state == PowerState::Suspended
    }

    pub fn power_saver(&self) -> bool {
        self.power_saver
    }

    /// Settings changed. Re-derives the state so turning the policy off wakes it out of Idle.
    pub fn apply_settings(&mut self, power_saver: bool, deep_sleep: bool, idle_ms: u32) {
        self.power_saver = power_saver;
        self.deep_sleep = deep_sleep;
        self.idle_ms = idle_ms;
        if self.state != PowerState::Suspended {
            self.state = if self.screen_off && self.power_saver {
                PowerState::Idle
            } else {
                PowerState::Active
            };
        }
    }

    /// The cpufreq profile that fits now: eco while idle (screen off) with the policy on,
    /// responsive otherwise. With the policy off, always responsive.
    pub fn cpu_profile(&self) -> CpuProfile {
        if self.power_saver && self.screen_off {
            CpuProfile::Eco
        } else {
            CpuProfile::Balanced
        }
    }

    /// The screen state changed. `off` is whether the panel is now off (`SCREEN_OFF`); dim counts
    /// as on. Returns `true` when the next deadline may have changed.
    pub fn screen_changed(&mut self, now_us: u64, off: bool) -> bool {
        if off == self.screen_off && self.state != PowerState::Suspended {
            return false;
        }
        self.screen_off = off;
        if off {
            self.off_since_us = Some(now_us);
            self.state = if self.power_saver {
                PowerState::Idle
            } else {
                PowerState::Active
            };
        } else {
            self.off_since_us = None;
            self.state = PowerState::Active;
        }
        true
    }

    /// Whether a suspend may be attempted (policy on, deep sleep on, screen off).
    fn suspend_allowed(&self) -> bool {
        self.power_saver && self.deep_sleep && self.screen_off
    }

    /// When [`PowerPolicy::on_timeout`] next has work (`CLOCK_MONOTONIC` µs): the end of the
    /// deep-idle window, while idle and suspend is allowed.
    pub fn next_deadline(&self) -> Option<u64> {
        if self.state == PowerState::Idle
            && self.suspend_allowed()
            && let Some(off) = self.off_since_us
        {
            return Some(off + u64::from(self.idle_ms) * 1000);
        }
        None
    }

    /// The deep-idle window may have elapsed: if so, move to `Suspended` and ask to suspend.
    pub fn on_timeout(&mut self, now_us: u64) -> Option<PowerAction> {
        let deadline = self.next_deadline()?;
        if now_us >= deadline {
            self.state = PowerState::Suspended;
            Some(PowerAction::Suspend)
        } else {
            None
        }
    }

    /// The system resumed (input arrived after a suspend). Re-arm: if the screen is still off, go
    /// back to Idle and restart the deep-idle window from now; otherwise go Active.
    pub fn resumed(&mut self, now_us: u64) {
        if self.state != PowerState::Suspended {
            return;
        }
        if self.screen_off && self.power_saver {
            self.off_since_us = Some(now_us);
            self.state = PowerState::Idle;
        } else {
            self.off_since_us = None;
            self.state = PowerState::Active;
        }
    }
}

/// One side effect of the energy policy, passed to the injected runner so the recording back end
/// can log it without touching the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op<'a> {
    /// Write `value` to the sysfs file `path`.
    Write { path: &'a Path, value: &'a str },
    /// Read the sysfs file `path` (brightness baseline).
    Read { path: &'a Path },
    /// Request suspend-to-RAM: write `mem` to `/sys/power/state`. The recording back end records
    /// this and returns at once; it never writes the real file.
    Suspend { state: &'a Path },
    /// Run `program args...` on `PATH` (the cpufreq helper).
    Run {
        program: &'a str,
        args: &'a [&'a str],
    },
}

/// Performs the side effects. `Err` means the file/helper could not be touched at all (absent);
/// `Ok(String)` is the read value (empty for writes/runs). A write/run that "failed" (e.g. the
/// kernel rejected the value) is the back end's concern and still returns `Ok`.
pub type Runner = Box<dyn FnMut(&Op) -> io::Result<String> + Send>;

/// Drives the documented power hooks. Created with the real [`Power::system`] back end or the
/// [`Power::recording`] one (`--mock`, tests).
pub struct Power {
    run: Runner,
    paths: PowerPaths,
    /// Whether glassd currently holds [`WAKELOCK`] (its view, not a re-query).
    locked: bool,
    /// The last cpufreq profile applied, so it is run only on a change.
    cpu: Option<CpuProfile>,
    /// The brightness read at [`Power::start`], restored on wake; `None` disables brightness.
    active_brightness: Option<u32>,
    /// The brightness last written, so it is written only on a change.
    brightness_now: Option<u32>,
}

impl std::fmt::Debug for Power {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Power")
            .field("locked", &self.locked)
            .field("cpu", &self.cpu)
            .field("active_brightness", &self.active_brightness)
            .finish()
    }
}

/// The real back end: writes sysfs, suspends, and spawns the cpufreq helper.
fn run_system(op: &Op) -> io::Result<String> {
    use std::io::{Read, Write};
    match *op {
        Op::Write { path, value } => {
            // Truncate, as `echo value > file` does: a sysfs attribute ignores it, and a plain
            // file (tests) is not left with a longer previous value's tail.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)?;
            f.write_all(value.as_bytes())?;
            Ok(String::new())
        }
        Op::Read { path } => {
            let mut s = String::new();
            std::fs::File::open(path)?.read_to_string(&mut s)?;
            Ok(s.trim().to_string())
        }
        Op::Suspend { state } => {
            // The Android wakelock path: request suspend-to-RAM. The kernel suspends once no
            // wakelock is held (glassd released its own just before this). Writing `mem` returns
            // without blocking in the deferred model; the resume path runs on a wake source.
            let mut f = std::fs::OpenOptions::new().write(true).open(state)?;
            f.write_all(b"mem\n")?;
            Ok(String::new())
        }
        Op::Run { program, args } => {
            let ok = std::process::Command::new(program)
                .args(args)
                .status()
                .map(|s| s.success())?;
            Ok(if ok { String::new() } else { "non-zero".into() })
        }
    }
}

impl Power {
    /// The real back end driving `paths`.
    pub fn system(paths: PowerPaths) -> Power {
        Power::with_runner(paths, Box::new(run_system))
    }

    /// A back end that only records its operations (never touches the host); for `--mock` and
    /// tests. Every operation "succeeds" with empty output, so a suspend is recorded, not taken.
    pub fn recording(
        paths: PowerPaths,
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Power {
        Power::with_runner(
            paths,
            Box::new(move |op: &Op| {
                log.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(describe(op));
                Ok(String::new())
            }),
        )
    }

    /// A back end with a custom runner.
    pub fn with_runner(paths: PowerPaths, run: Runner) -> Power {
        Power {
            run,
            paths,
            locked: false,
            cpu: None,
            active_brightness: None,
            brightness_now: None,
        }
    }

    /// Runs one operation; logs and returns `None` if the file/helper is absent, else its output.
    fn op(&mut self, op: Op) -> Option<String> {
        match (self.run)(&op) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("power: {} unavailable ({e})", describe(&op));
                None
            }
        }
    }

    fn write(&mut self, path: PathBuf, value: &str) -> bool {
        self.op(Op::Write { path: &path, value }).is_some()
    }

    /// Takes [`WAKELOCK`], so the system will not suspend while glassd wants it awake.
    pub fn lock(&mut self) {
        if self.locked {
            return;
        }
        let p = self.paths.wake_lock.clone();
        if self.write(p, WAKELOCK) {
            self.locked = true;
        }
    }

    /// Releases [`WAKELOCK`].
    fn unlock(&mut self) {
        let p = self.paths.wake_unlock.clone();
        if self.write(p, WAKELOCK) {
            self.locked = false;
        }
    }

    /// Arms the configured `power/wakeup` sources (`enabled`), so they resume the system.
    fn arm_wakeups(&mut self) {
        for dir in self.paths.wakeup.clone() {
            self.write(dir.join("power/wakeup"), "enabled");
        }
    }

    /// Sets `power/control = auto` on the configured peripherals (runtime-PM autosuspend).
    fn runtime_pm_auto(&mut self) {
        for ctl in self.paths.runtime_pm.clone() {
            self.write(ctl, "auto");
        }
    }

    /// Applies the cpufreq profile (only on a change), through the `omg-tune.sh` helper.
    pub fn set_cpu_profile(&mut self, profile: CpuProfile) {
        if self.cpu == Some(profile) {
            return;
        }
        let tune = self.paths.cpu_tune.clone();
        let arg = profile.arg();
        if self
            .op(Op::Run {
                program: &tune,
                args: &[arg],
            })
            .is_some()
        {
            self.cpu = Some(profile);
            info!("power: cpufreq profile {arg}");
        }
    }

    /// Reads the brightness baseline; brightness control is enabled only when it reads a positive
    /// value (so the policy never writes a guessed level and never blacks out a working panel).
    fn read_active_brightness(&mut self) {
        let Some(path) = self.paths.brightness.clone() else {
            return;
        };
        if let Some(s) = self.op(Op::Read { path: &path }) {
            match s.parse::<u32>() {
                Ok(v) if v > 0 => {
                    self.active_brightness = Some(v);
                    self.brightness_now = Some(v);
                    info!("power: brightness baseline {v}");
                }
                _ => info!("power: brightness {s:?} not positive; brightness control off"),
            }
        }
    }

    /// Dims or restores the panel brightness to match the screen state (only when a baseline was
    /// read). `SCREEN_DIM` and `SCREEN_OFF` drop it; `SCREEN_ON` restores the baseline. The panel
    /// is still blanked by `FBIOBLANK` in [`crate::display`]; this only lowers the backlight.
    pub fn set_brightness_for(&mut self, screen: Screen) {
        let Some(active) = self.active_brightness else {
            return;
        };
        let target = match screen {
            Screen::On => active,
            // A low, non-zero level while dim; the FBIOBLANK POWERDOWN already darkens OFF, so a
            // small floor keeps the backlight path warm without being bright.
            Screen::Dim => (active / 4).max(1),
            Screen::Off => (active / 8).max(1),
            Screen::Unknown => return,
        };
        if self.brightness_now == Some(target) {
            return;
        }
        let Some(path) = self.paths.brightness.clone() else {
            return;
        };
        if self.write(path, &target.to_string()) {
            self.brightness_now = Some(target);
        }
    }

    /// Start of day: read the brightness baseline, set peripherals to runtime-PM auto, arm the
    /// wake sources, and take the wakelock. All best effort.
    pub fn start(&mut self, initial: CpuProfile) {
        self.read_active_brightness();
        self.runtime_pm_auto();
        self.arm_wakeups();
        self.lock();
        self.set_cpu_profile(initial);
    }

    /// Requests a suspend cycle: re-arm the wake sources, release the wakelock, and write `mem`.
    /// Returns once requested (non-blocking); the system suspends when no wakelock is held and a
    /// wake source resumes it. [`Power::resume`] must run when activity next arrives.
    pub fn suspend(&mut self) {
        self.arm_wakeups();
        info!("power: suspend requested (releasing {WAKELOCK}, writing mem)");
        self.unlock();
        let state = self.paths.state.clone();
        self.op(Op::Suspend { state: &state });
    }

    /// The system resumed: re-take the wakelock so it does not immediately suspend again.
    pub fn resume(&mut self) {
        info!("power: resumed (re-taking {WAKELOCK})");
        self.lock();
    }

    pub fn holds_wakelock(&self) -> bool {
        self.locked
    }
}

/// A human-readable one-liner for an [`Op`] (logs and the recording back end).
fn describe(op: &Op) -> String {
    match *op {
        Op::Write { path, value } => format!("write {}={value}", path.display()),
        Op::Read { path } => format!("read {}", path.display()),
        Op::Suspend { state } => format!("suspend mem -> {}", state.display()),
        Op::Run { program, args } => format!("run {program} {}", args.join(" ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const S: u64 = 1_000_000;

    fn policy() -> PowerPolicy {
        // Policy on, deep sleep on, 60 s idle window.
        PowerPolicy::new(true, true, 60_000)
    }

    #[test]
    fn idle_then_suspend_after_the_window() {
        let mut p = policy();
        // Screen on: nothing to do.
        assert_eq!(p.state(), PowerState::Active);
        assert_eq!(p.next_deadline(), None);
        assert_eq!(p.cpu_profile(), CpuProfile::Balanced);
        // Screen off at 10 s: idle, eco, suspend armed for 10 + 60 s.
        assert!(p.screen_changed(10 * S, true));
        assert_eq!(p.state(), PowerState::Idle);
        assert_eq!(p.cpu_profile(), CpuProfile::Eco);
        assert_eq!(p.next_deadline(), Some(70 * S));
        // Before the window: nothing.
        assert_eq!(p.on_timeout(69 * S), None);
        // At the window: suspend.
        assert_eq!(p.on_timeout(70 * S), Some(PowerAction::Suspend));
        assert_eq!(p.state(), PowerState::Suspended);
        assert_eq!(p.next_deadline(), None);
    }

    #[test]
    fn resume_rearms_while_still_off() {
        let mut p = policy();
        p.screen_changed(0, true);
        p.on_timeout(60 * S);
        assert!(p.is_suspended());
        // Resume at 100 s, screen still off: back to Idle, window restarts.
        p.resumed(100 * S);
        assert_eq!(p.state(), PowerState::Idle);
        assert_eq!(p.next_deadline(), Some(160 * S));
        // A wake (screen on) then goes Active.
        assert!(p.screen_changed(101 * S, false));
        assert_eq!(p.state(), PowerState::Active);
        assert_eq!(p.next_deadline(), None);
    }

    #[test]
    fn deep_sleep_off_never_suspends_but_still_eco() {
        let mut p = PowerPolicy::new(true, false, 60_000);
        p.screen_changed(0, true);
        assert_eq!(p.state(), PowerState::Idle);
        assert_eq!(p.cpu_profile(), CpuProfile::Eco); // still saves via cpufreq
        assert_eq!(p.next_deadline(), None); // but no suspend
        assert_eq!(p.on_timeout(10_000 * S), None);
    }

    #[test]
    fn power_saver_off_stays_active() {
        let mut p = PowerPolicy::new(false, true, 60_000);
        p.screen_changed(0, true);
        assert_eq!(p.state(), PowerState::Active);
        assert_eq!(p.next_deadline(), None);
        // Turning it on while already off moves to Idle.
        p.apply_settings(true, true, 60_000);
        assert_eq!(p.state(), PowerState::Idle);
        assert_eq!(p.next_deadline(), Some(60 * S));
    }

    fn recording() -> (Power, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let paths = PowerPaths {
            brightness: None,
            ..Default::default()
        };
        (Power::recording(paths, log.clone()), log)
    }

    fn cmds(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    #[test]
    fn recording_back_end_records_a_suspend_and_never_writes_real_state() {
        // A state path that does not exist: the recording back end must not create or write it.
        let dir = std::env::temp_dir().join(format!("glassd-power-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("state");
        let log = Arc::new(Mutex::new(Vec::new()));
        let paths = PowerPaths {
            state: state.clone(),
            wake_lock: dir.join("wake_lock"),
            wake_unlock: dir.join("wake_unlock"),
            brightness: None,
            ..Default::default()
        };
        let mut power = Power::recording(paths, log.clone());
        power.start(CpuProfile::Balanced);
        assert!(power.holds_wakelock());
        power.suspend();
        power.resume();
        let c = cmds(&log);
        // The suspend was recorded...
        assert!(c.iter().any(|l| l.starts_with("suspend mem -> ")), "{c:?}");
        // ...but nothing was ever written to the host's power files.
        assert!(
            !state.exists(),
            "the recording back end wrote the state file"
        );
        assert!(!dir.join("wake_lock").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cpu_profile_is_applied_only_on_a_change() {
        let (mut p, log) = recording();
        p.set_cpu_profile(CpuProfile::Eco);
        p.set_cpu_profile(CpuProfile::Eco);
        let c = cmds(&log);
        assert_eq!(
            c.iter().filter(|l| l.contains("omg-tune.sh")).count(),
            1,
            "{c:?}"
        );
        p.set_cpu_profile(CpuProfile::Balanced);
        assert!(cmds(&log).iter().any(|l| l.contains("balanced")));
    }

    #[test]
    fn suspend_releases_and_resume_retakes_the_wakelock() {
        let (mut p, log) = recording();
        p.lock();
        assert!(p.holds_wakelock());
        let _ = cmds(&log);
        p.suspend();
        assert!(!p.holds_wakelock());
        let c = cmds(&log);
        assert!(c.iter().any(|l| l.contains("wake_unlock")));
        assert!(c.iter().any(|l| l.starts_with("suspend mem")));
        p.resume();
        assert!(p.holds_wakelock());
        assert!(cmds(&log).iter().any(|l| l.contains("wake_lock")));
    }

    #[test]
    fn system_back_end_writes_wake_files_but_only_a_temp_state() {
        // Exercises the REAL writer against temp files only (never the host's /sys). Proves the
        // real path writes wake_lock / mem; the state path here is a throwaway temp file.
        let dir = std::env::temp_dir().join(format!("glassd-power-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["wake_lock", "wake_unlock", "state", "brightness"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        std::fs::write(dir.join("brightness"), "200\n").unwrap();
        let paths = PowerPaths {
            state: dir.join("state"),
            wake_lock: dir.join("wake_lock"),
            wake_unlock: dir.join("wake_unlock"),
            brightness: Some(dir.join("brightness")),
            runtime_pm: Vec::new(),
            wakeup: Vec::new(),
            cpu_tune: "true".into(), // a harmless helper that exists on CI
        };
        let mut power = Power::system(paths);
        power.start(CpuProfile::Eco);
        assert_eq!(power.active_brightness, Some(200));
        assert_eq!(
            std::fs::read_to_string(dir.join("wake_lock")).unwrap(),
            WAKELOCK
        );
        power.set_brightness_for(Screen::Off);
        assert_eq!(
            std::fs::read_to_string(dir.join("brightness")).unwrap(),
            "25"
        );
        power.set_brightness_for(Screen::On);
        assert_eq!(
            std::fs::read_to_string(dir.join("brightness")).unwrap(),
            "200"
        );
        power.suspend();
        assert_eq!(std::fs::read_to_string(dir.join("state")).unwrap(), "mem\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("wake_unlock")).unwrap(),
            WAKELOCK
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn brightness_off_without_a_baseline_does_nothing() {
        let (mut p, log) = recording();
        // No brightness path configured: set_brightness_for is a no-op.
        p.set_brightness_for(Screen::Off);
        assert!(cmds(&log).is_empty());
    }
}
