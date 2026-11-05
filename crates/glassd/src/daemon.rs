//! The `poll` loop: listening socket, clients, the touchpad, and (with `--mock`) standard input.
//! Single-threaded; blocks in `poll` until a descriptor is ready or the next deadline
//! (long press, screen timer, a full client queue, a mock `wait`).

use crate::buttons::{ButtonTracker, LONG_PRESS_MS};
use crate::companion::{self, Companion};
use crate::display::{Display, TouchpadSleep};
use crate::mockcmd::{self, Command, CompanionCommand};
use crate::offhead::OffHeadTimer;
use crate::peers::PeerPolicy;
use crate::power::{Power, PowerAction, PowerPaths, PowerPolicy};
use crate::queue::OutQueue;
use crate::radio::Radio;
use crate::screen::{Change, Now, ScreenPolicy};
use crate::settings_store::SettingsStore;
use crate::touch::{ScreenOffInput, Step, TouchGate};
use crate::{info, warn};
use glass_api::hub::settings_rules;
use glass_api::proto::{
    Button, ButtonAction, ButtonId, ClientFrame, CompanionNotification,
    CompanionNotificationActionInfo, CompanionNotificationRemoved, CompanionRegister,
    CompanionStatus, DictationState, ErrorCode, HeadGesture, HeadGestureKind, Hello, Locale,
    Screen, ScreenState, ServiceError, ServiceFrame, Settings, Transcript, Wear, WearSource, Wink,
    client_frame, dictation_state, service_frame,
};
use glass_api::{ClientId, Effect, FrameDecoder, Hub};
use glass_evdev::Device;
use glass_gesture::synth::{Direction, Synth};
use glass_gesture::{AxisRange, Config, Output, RawEvent, Recognizer};
use glass_head::{Context as HeadContext, HeadService, Imu, Sample};
use glass_hub::{GlassHub, Thresholds, Update, WearMachine};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How far `BOOTTIME` must pull ahead of `MONOTONIC` to count as a resume; the two are read
/// apart, so a small difference means nothing.
const RESUME_MIN_US: u64 = 100_000;

/// The time spent suspended since boot.
fn suspend_offset(t: Now) -> u64 {
    t.boot_us.saturating_sub(t.mono_us)
}

/// How often `don_doff` is read while on-head detection is armed.
const DON_DOFF_RECHECK_MS: u64 = 2000;
/// The wait after each step of waking the hub's sensing (1 s is what was tested on hardware).
const HUB_WAKE_STEP: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_secs(1)
};

/// Name of the touchpad input device (docs/specs/kernel-interfaces.md §1.2).
pub const TOUCHPAD_NAME: &str = "sensor00fn11";
/// Input device of the camera button, `KEY_CAMERA` (kernel-interfaces.md §5).
pub const CAMERA_NAME: &str = "gpio-keys";
pub const KEY_CAMERA: u16 = 212;
/// Input device of the power button, `KEY_POWER`.
pub const POWER_NAME: &str = "twl6030_pwrbutton";
pub const KEY_POWER: u16 = 116;

const EV_KEY: u16 = 1;

/// Bytes read from one client per loop iteration.
const READ_BUDGET: usize = 64 * 1024;

const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_PRESSURE: u16 = 0x3a;

/// Command line options.
#[derive(Debug, Clone)]
pub struct Options {
    pub socket: PathBuf,
    pub mock: bool,
    pub device: Option<PathBuf>,
    pub device_name: String,
    /// "Forward" is decreasing raw X (the stock pad has forward = +X).
    pub invert_x: bool,
    /// "Down" is decreasing raw Y.
    pub invert_y: bool,
    /// Gesture-recognizer threshold overrides for on-device fine-tuning (all in touchpad device
    /// units; `None` keeps the [`glass_gesture::Config`] default). See the recognizer config for
    /// what each means; exposed as flags so the tap/swipe feel can be re-tuned on the real pad
    /// without rebuilding.
    pub tap_slop: Option<f32>,
    pub swipe_min_travel: Option<f32>,
    pub scroll_min_step: Option<f32>,
    pub screen_off_input: ScreenOffInput,
    /// The glasshub sysfs directory (`None`: no glasshub).
    pub glasshub: Option<PathBuf>,
    /// A stock gaze parameter file to take the hub thresholds from, if it exists.
    pub gaze_params: Option<PathBuf>,
    pub camera: Source,
    pub power: Source,
    /// The touchpad's F11 sysfs directory, for `sleepmode` (`None`: not written).
    pub touchpad_sysfs: Option<PathBuf>,
    /// Where the settings are stored (`None`: in memory only).
    pub state_dir: Option<PathBuf>,
    /// Where IIO devices are listed, to find the IMU (`None`: no IMU).
    pub iio: Option<PathBuf>,
    /// The IMU's character device (default `/dev/iio:deviceN`).
    pub iio_dev: Option<PathBuf>,
    /// Which peers may connect (docs/api.md, "Who may connect").
    pub peers: PeerPolicy,
    /// Grab the touchpad (`EVIOCGRAB`), so Android's input reader does not also turn its
    /// touches into navigation keys (docs/api.md, "Android glue").
    pub grab_touchpad: bool,
    /// Control the Wi-Fi and Bluetooth radios (Model B / plain Linux). Off on Android, where
    /// the framework owns the radios.
    pub radio: bool,
    /// The Wi-Fi interface to control when `radio` is on (default `wlan0`).
    pub wifi_iface: String,
    /// Run the companion link (Model B holds the phone link in glassd). Off on Android (the
    /// Glass app is the provider) and with `--mock` (the simulated provider is used instead).
    pub companion: bool,
    /// TCP accept port for the companion link (`None` disables the TCP fallback path).
    pub companion_tcp_port: Option<u16>,
    /// Advertise and accept the Bluetooth RFCOMM companion link (needs the `linux-bt` feature).
    pub companion_bt: bool,
    /// Where the companion link learns that the random pool is seeded (`/dev/random`).
    pub companion_rng_device: PathBuf,
    /// Where to read the battery for the companion `BatteryStatus`/`Status`.
    pub power_supply: PathBuf,
    /// Run the Model B energy policy (idle -> blank/brightness/cpufreq/runtime-PM and, when
    /// `deep_sleep_enabled`, suspend-to-RAM). Off on Android, where the framework owns power.
    /// Named `energy` to avoid clashing with the `power` power-button device source.
    pub energy: bool,
    /// The sysfs paths and cpufreq helper the energy policy drives (`--power-*`).
    pub power_paths: PowerPaths,
}

/// Where an input device comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Found by its input device name.
    Auto,
    Path(PathBuf),
    /// Not used.
    Off,
}

/// Reads both clocks.
pub fn now() -> Now {
    let mono_us = glass_api::monotonic_us();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let boot_us = {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec.
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
        (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let boot_us = mono_us;
    Now { mono_us, boot_us }
}

struct Conn {
    stream: UnixStream,
    decoder: FrameDecoder,
    queue: OutQueue,
    closing: bool,
}

/// The simulated companion link provider of `--mock` (docs/api.md, "Mock"): a hub client
/// without a connection, whose frames the daemon answers itself.
struct MockCompanion {
    id: ClientId,
    /// Open requests, oldest first: (link id, is a reply).
    links: Vec<(u32, bool)>,
}

struct MockInput {
    buf: Vec<u8>,
    commands: VecDeque<Command>,
    resume_at: Option<u64>,
    eof: bool,
}

pub struct Daemon {
    hub: Hub,
    listener: UnixListener,
    socket_path: PathBuf,
    /// Whether glassd bound the socket itself (and removes it on exit); false for a socket
    /// passed by Android's init.
    owns_socket: bool,
    peers: PeerPolicy,
    mock_companion: Option<MockCompanion>,
    conns: BTreeMap<ClientId, Conn>,
    device: Option<Device>,
    rec: Recognizer,
    gate: TouchGate,
    policy: ScreenPolicy,
    display: Box<dyn Display>,
    mock: Option<MockInput>,
    events: Vec<glass_evdev::Event>,
    glasshub: Option<GlassHub>,
    hub_written: HubWritten,
    /// While on-head detection is armed: when to read `don_doff` again.
    don_doff_recheck_us: Option<u64>,
    /// The last `don_doff` value read, to log only its changes.
    last_don_doff: Option<bool>,
    wear: WearMachine,
    camera: ButtonTracker,
    power: ButtonTracker,
    camera_dev: Option<Device>,
    power_dev: Option<Device>,
    touch_sleep: TouchpadSleep,
    store: Option<SettingsStore>,
    /// Wi-Fi / Bluetooth control (`None`: this build does not manage the radios, e.g. Android).
    radio: Option<Radio>,
    /// The idle -> suspend decision (always present; inert when the back end is absent).
    power_policy: PowerPolicy,
    /// How long off the head, for the wake alarm and the power-off.
    off_head: OffHeadTimer,
    /// `BOOTTIME - MONOTONIC` when the last suspend was requested, to recognise a resume.
    suspend_offset_us: Option<u64>,
    /// The energy-policy side-effect back end (`None`: not managing power, e.g. Android/mock-off).
    /// Named `power_io` to avoid clashing with the `power` power-button tracker.
    power_io: Option<Power>,
    imu: Option<Imu>,
    head: HeadService,
    /// `HeadRequest` wishes per client: (relative look-up, nudge).
    head_requests: BTreeMap<ClientId, (bool, bool)>,
    samples: Vec<Sample>,
    /// IMU read batches so far (for the `OMGB-SENSOR:` liveness diagnostic).
    imu_reads: u64,
    /// The companion link (Model B holds the phone link in glassd); `None` on Android/`--mock`.
    companion: Option<Companion>,
    /// The internal hub client the companion link registers as (the provider), so the launcher
    /// sees the link through the `companion` capability.
    companion_provider: Option<ClientId>,
    /// Where the battery is read for the companion status.
    power_supply: PathBuf,
    /// The last battery `(percent, charging)` pushed to the companion.
    battery_last: Option<(u8, bool)>,
    /// When to poll the battery next (`CLOCK_MONOTONIC` us).
    battery_deadline: Option<u64>,
}

/// How often glassd re-reads the battery to push changes to the companion.
const BATTERY_POLL_US: u64 = 30_000_000;

/// Writes `v` with `write` unless it is the value last written; true if it was written.
fn sync_attr(
    cur: &mut Option<bool>,
    v: bool,
    name: &str,
    write: impl FnOnce(bool) -> io::Result<()>,
) -> bool {
    if *cur == Some(v) {
        return false;
    }
    match write(v) {
        Ok(()) => {
            *cur = Some(v);
            true
        }
        Err(e) => {
            warn!("glasshub {name}: {e}");
            false
        }
    }
}

/// The effective `(wifi, bluetooth)` radio states for the settings: airplane mode forces both
/// off, keeping the per-radio wishes so they are restored when it is turned off.
/// Maps the companion's locale to the services `Locale` enum (both cover the same it/en/fr/es
/// set; see `omg_protocol::Locale` and the `Locale` proto enum).
/// The settings as the companion sees them. Radios have their own frames.
fn companion_settings(s: &Settings) -> omg_protocol::Settings {
    omg_protocol::Settings {
        brightness: None,
        head_wake: s.head_wake_enabled,
        head_wake_angle_deg: s.head_wake_angle_deg.map(|a| a.round() as u8),
        on_head_detection: s.on_head_detection_enabled,
        audio_volume: s.audio_volume.map(|v| v.min(100) as u8),
        time_24h: s.time_24h,
        timezone_id: s.timezone_id,
        power_saver: s.power_saver,
        deep_sleep: s.deep_sleep_enabled,
        deep_sleep_idle_s: s.deep_sleep_idle_ms.map(|ms| ms / 1000),
        off_head_power_off_min: s.off_head_power_off_min,
        wink: s.wink_enabled,
        screen_timeout_s: s.screen_timeout_ms.map(|ms| ms / 1000),
        nudge: s.nudge_sleep_enabled,
        locale: s.locale.map(|l| companion_locale(Some(l))),
        radios: None,
    }
}

/// A change from the companion, as glassd settings. `settings_rules::merge` clamps it.
fn settings_from_companion(s: &omg_protocol::Settings) -> Settings {
    Settings {
        head_wake_enabled: s.head_wake,
        head_wake_angle_deg: s.head_wake_angle_deg.map(f32::from),
        on_head_detection_enabled: s.on_head_detection,
        audio_volume: s.audio_volume.map(u32::from),
        time_24h: s.time_24h,
        timezone_id: s.timezone_id,
        power_saver: s.power_saver,
        deep_sleep_enabled: s.deep_sleep,
        deep_sleep_idle_ms: s.deep_sleep_idle_s.map(|v| v.saturating_mul(1000)),
        off_head_power_off_min: s.off_head_power_off_min,
        wink_enabled: s.wink,
        screen_timeout_ms: s.screen_timeout_s.map(|v| v.saturating_mul(1000)),
        nudge_sleep_enabled: s.nudge,
        locale: s.locale.map(|l| proto_locale(l) as i32),
        ..Default::default()
    }
}

fn proto_locale(l: omg_protocol::Locale) -> Locale {
    match l {
        omg_protocol::Locale::It => Locale::It,
        omg_protocol::Locale::En => Locale::En,
        omg_protocol::Locale::Fr => Locale::Fr,
        omg_protocol::Locale::Es => Locale::Es,
    }
}

/// Maps a stored services `Settings.locale` value back to the companion's locale, falling back
/// to `omg_protocol::Locale::DEFAULT` (en) for unset/unknown values.
fn companion_locale(v: Option<i32>) -> omg_protocol::Locale {
    match v.and_then(|v| Locale::try_from(v).ok()) {
        Some(Locale::It) => omg_protocol::Locale::It,
        Some(Locale::Fr) => omg_protocol::Locale::Fr,
        Some(Locale::Es) => omg_protocol::Locale::Es,
        _ => omg_protocol::Locale::En,
    }
}

fn effective_radio(s: &Settings) -> (bool, bool) {
    let airplane = s.airplane_mode.unwrap_or(false);
    (
        !airplane && s.wifi_enabled.unwrap_or(false),
        !airplane && s.bluetooth_enabled.unwrap_or(false),
    )
}

/// What was last written to the glasshub, so each value is written only on a change.
#[derive(Debug, Default)]
struct HubWritten {
    detection: Option<bool>,
    wink: Option<bool>,
    pause: Option<bool>,
}

/// The environment variable through which Android's init passes a socket named `name`.
pub fn init_socket_env(name: &str) -> String {
    format!("ANDROID_SOCKET_{name}")
}

/// The listening socket: the one Android's init created for the socket's file name
/// (`ANDROID_SOCKET_<name>`, a file descriptor number), else [`bind_socket`]. Returns it and
/// whether glassd owns the file.
pub fn listen(path: &Path) -> io::Result<(UnixListener, bool)> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if let Ok(v) = std::env::var(init_socket_env(name)) {
        let fd: RawFd = v.trim().parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}={v:?} is not a file descriptor", init_socket_env(name)),
            )
        })?;
        // init only binds the socket; the daemon listens (as vold and installd do). On a
        // socket that already listens this only changes the backlog.
        // SAFETY: listen on a descriptor number; an invalid one only returns an error.
        if unsafe { libc::listen(fd, 16) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: init passes the descriptor of a Unix stream socket and nothing else in
        // this process uses it.
        let l = unsafe { UnixListener::from_raw_fd(fd) };
        l.set_nonblocking(true)?;
        return Ok((l, false));
    }
    Ok((bind_socket(path)?, true))
}

/// Binds the socket: removes a stale socket file, refuses to start if another daemon answers,
/// creates the parent directory, and sets mode 0660.
pub fn bind_socket(path: &Path) -> io::Result<UnixListener> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{} is served by another process", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

/// Opens a button's input device (by name, or the given path) and checks that it has the key.
fn open_key(source: &Source, name: &str, code: u16) -> Option<Device> {
    let path = match source {
        Source::Off => return None,
        Source::Path(p) => p.clone(),
        Source::Auto => match glass_evdev::find_by_name(name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                info!("no input device named {name:?}");
                return None;
            }
            Err(e) => {
                warn!("cannot scan /dev/input for {name:?}: {e}");
                return None;
            }
        },
    };
    let mut dev = match Device::open(&path) {
        Ok(d) => d,
        Err(e) => {
            warn!("cannot open {}: {e}", path.display());
            return None;
        }
    };
    if dev.has_code(EV_KEY, code).is_ok_and(|has| !has) {
        warn!("{} ({name:?}) has no key {code}", path.display());
        return None;
    }
    if let Err(e) = dev.set_clock_monotonic() {
        info!("{}: EVIOCSCLOCKID failed ({e})", path.display());
    }
    info!("{name:?} on {}", path.display());
    Some(dev)
}

/// Opens the glasshub and writes its thresholds (with on-head detection off while doing so).
fn open_glasshub(opts: &Options) -> Option<GlassHub> {
    let root = opts.glasshub.as_ref()?;
    let hub = match GlassHub::open(root) {
        Ok(Some(h)) => h,
        Ok(None) => {
            info!("OMGB-SENSOR: no glasshub at {}", root.display());
            return None;
        }
        Err(e) => {
            warn!(
                "OMGB-SENSOR: cannot open the glasshub at {}: {e}",
                root.display()
            );
            return None;
        }
    };
    info!(
        "OMGB-SENSOR: glasshub opened {} (firmware {})",
        root.display(),
        hub.version().unwrap_or_else(|e| format!("unreadable: {e}"))
    );
    // The hub's sensing stays off (proxraw 65535, don_doff stuck at 0) until it sees a pause
    // and a resume, and the driver writes the register only when the state changes. Stock
    // goes through this at start-up; Model B has to do it itself (seen on the device: one
    // toggle took proxraw from 65535 to 322).
    if let Err(e) = wake_hub_sensing(&hub) {
        warn!("glasshub wake: {e}");
    }
    let thresholds = opts
        .gaze_params
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| Thresholds::parse_params(&t))
        .unwrap_or_default();
    if let Err(e) = hub
        .set_on_head_detection(false)
        .and_then(|()| hub.apply_thresholds(&thresholds))
    {
        warn!("glasshub thresholds: {e}");
    }
    Some(hub)
}

fn wake_hub_sensing(hub: &GlassHub) -> io::Result<()> {
    hub.set_pause(true)?;
    std::thread::sleep(HUB_WAKE_STEP);
    hub.set_pause(false)?;
    std::thread::sleep(HUB_WAKE_STEP);
    Ok(())
}

fn open_imu(opts: &Options) -> Option<Imu> {
    let root = opts.iio.as_ref()?;
    let dir = match glass_head::iio::find(root, glass_head::iio::IMU_NAME) {
        Ok(Some(d)) => d,
        Ok(None) => {
            info!(
                "OMGB-SENSOR: no IIO device named {:?}",
                glass_head::iio::IMU_NAME
            );
            return None;
        }
        Err(e) => {
            info!("cannot list {}: {e}", root.display());
            return None;
        }
    };
    match Imu::open(&dir, opts.iio_dev.clone()) {
        Ok(imu) => {
            info!(
                "OMGB-SENSOR: IMU opened {} ({:?})",
                dir.display(),
                imu.calibration()
            );
            Some(imu)
        }
        Err(e) => {
            warn!("OMGB-SENSOR: cannot open the IMU at {}: {e}", dir.display());
            None
        }
    }
}

fn open_touchpad(opts: &Options) -> Option<(Device, Config)> {
    let path = match &opts.device {
        Some(p) => p.clone(),
        None => match glass_evdev::find_by_name(&opts.device_name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                warn!("no input device named {:?}", opts.device_name);
                return None;
            }
            Err(e) => {
                warn!("cannot scan /dev/input: {e}");
                return None;
            }
        },
    };
    let mut dev = match Device::open(&path) {
        Ok(d) => d,
        Err(e) => {
            warn!("cannot open {}: {e}", path.display());
            return None;
        }
    };
    if let Err(e) = dev.set_clock_monotonic() {
        warn!("EVIOCSCLOCKID failed ({e}); stamping events when read");
    }
    if opts.grab_touchpad
        && let Err(e) = dev.grab(true)
    {
        warn!("EVIOCGRAB failed ({e}); other readers also get the touchpad");
    }
    let axis = |code, fallback: AxisRange| match dev.abs_info(code) {
        Ok(a) if a.maximum > a.minimum => AxisRange::new(a.minimum, a.maximum),
        _ => fallback,
    };
    let d = Config::default();
    let mut cfg = Config::with_axes(axis(ABS_MT_POSITION_X, d.x), axis(ABS_MT_POSITION_Y, d.y));
    if let Ok(p) = dev.abs_info(ABS_MT_PRESSURE)
        && p.maximum > 0
    {
        cfg.pressure_max = p.maximum;
    }
    info!(
        "touchpad {} ({:?}): X {}..{}, Y {}..{}",
        path.display(),
        dev.name().unwrap_or_default(),
        cfg.x.min,
        cfg.x.max,
        cfg.y.min,
        cfg.y.max
    );
    Some((dev, cfg))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_ucred(fd: RawFd) -> Option<(u32, i32)> {
    let mut c = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `c` and `len` are valid for getsockopt(SO_PEERCRED).
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut c as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some((c.uid, c.pid))
}

/// macOS and the BSDs (host development): the uid only.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_ucred(fd: RawFd) -> Option<(u32, i32)> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: valid out-pointers for getpeereid.
    let r = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    (r == 0).then_some((uid, -1))
}

impl Daemon {
    pub fn new(opts: &Options, display: Box<dyn Display>) -> io::Result<Daemon> {
        let (listener, owns_socket) = listen(&opts.socket)?;
        let t = now();
        let (device, mut cfg) = if opts.mock {
            (None, Config::default())
        } else {
            match open_touchpad(opts) {
                Some((d, c)) => (Some(d), c),
                None => (None, Config::default()),
            }
        };
        cfg.forward_is_positive_x = !opts.invert_x;
        cfg.down_is_positive_y = !opts.invert_y;
        if let Some(v) = opts.tap_slop {
            cfg.touch_slop = v;
        }
        if let Some(v) = opts.swipe_min_travel {
            cfg.swipe_min_travel = v;
        }
        if let Some(v) = opts.scroll_min_step {
            cfg.scroll_min_step = v;
        }
        let store = opts.state_dir.as_ref().map(SettingsStore::new);
        let settings = match &store {
            Some(st) => st.load().unwrap_or_else(|e| {
                warn!(
                    "cannot read {}: {e}; using the defaults",
                    st.path().display()
                );
                settings_rules::defaults()
            }),
            None => settings_rules::defaults(),
        };
        let policy = ScreenPolicy::new(t, settings.screen_timeout_ms.unwrap_or(120_000));
        let (glasshub, camera_dev, power_dev, imu) = if opts.mock {
            (None, None, None, None)
        } else {
            (
                open_glasshub(opts),
                open_key(&opts.camera, CAMERA_NAME, KEY_CAMERA),
                open_key(&opts.power, POWER_NAME, KEY_POWER),
                open_imu(opts),
            )
        };
        let hub_present = glasshub.is_some() || opts.mock;
        let detection = settings.on_head_detection_enabled.unwrap_or(false) && hub_present;
        let wear = WearMachine::new(t.mono_us, detection);
        // The radio controller: recorded (no real commands) with --mock, real otherwise when
        // --radio is on. Android leaves the radios to the framework (--radio off).
        let radio = if opts.mock {
            opts.radio.then(|| {
                Radio::recording(
                    opts.wifi_iface.clone(),
                    std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                )
            })
        } else {
            opts.radio.then(|| {
                let radio = Radio::system(opts.wifi_iface.clone());
                match &opts.state_dir {
                    Some(dir) => radio.with_wifi_conf(dir.join(crate::radio::WIFI_CONF_FILE)),
                    None => radio,
                }
            })
        };
        // The energy-policy back end: recorded with --mock (never touches the host's power
        // sysfs), real otherwise when --power is on. Android leaves power to the framework.
        let power = if !opts.energy {
            None
        } else if opts.mock {
            Some(Power::recording(
                opts.power_paths.clone(),
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            ))
        } else {
            // Expand the runtime-PM device list from the live buses (real back end only).
            Some(Power::system(
                opts.power_paths.clone().with_default_runtime_pm(),
            ))
        };
        // The policy is inert without a back end: force power_saver off so it never arms a
        // suspend it cannot carry out (Android / --power off).
        let off_head = OffHeadTimer::new(settings.off_head_power_off_min.unwrap_or(30));
        let power_policy = PowerPolicy::new(
            settings.power_saver.unwrap_or(true) && power.is_some(),
            settings.deep_sleep_enabled.unwrap_or(false),
            settings.deep_sleep_idle_ms.unwrap_or(60_000),
        );
        // The companion link: on Model B glassd holds the phone link itself (it is the hub's
        // companion provider). Off on Android (the Glass app is the provider) and with --mock
        // (the simulated provider is used instead).
        let companion = if opts.companion && !opts.mock {
            let state_dir = opts
                .state_dir
                .clone()
                .unwrap_or_else(crate::settings_store::default_state_dir);
            // Report the persisted UI language to the companion from the start.
            let mut device = companion::link::DeviceState {
                locale: companion_locale(settings.locale),
                ..Default::default()
            };
            if let Some((percent, charging)) = companion::battery::read(&opts.power_supply) {
                device.battery_percent = Some(percent);
                device.charging = Some(charging);
            }
            match Companion::with_rng_device(
                &state_dir,
                device,
                opts.companion_tcp_port,
                opts.companion_bt,
                &opts.companion_rng_device,
            ) {
                Ok(c) => Some(c),
                Err(e) => {
                    warn!("companion: disabled ({e})");
                    None
                }
            }
        } else {
            None
        };
        let caps: Vec<&str> = if opts.mock {
            let mut c = vec![
                "mock",
                "touchpad",
                "screen",
                "button-camera",
                "button-power",
                "hinge",
                "head",
                "wear",
                "wink",
                "dictation",
                "companion",
            ];
            if radio.is_some() {
                c.push("radio");
            }
            if power.is_some() {
                c.push("power");
            }
            c
        } else {
            let mut c = Vec::new();
            if device.is_some() {
                c.push("touchpad");
            }
            if imu.is_some() {
                c.push("head");
            }
            c.extend(["screen", "wear"]);
            if glasshub.is_some() {
                c.push("wink");
            }
            if camera_dev.is_some() {
                c.push("button-camera");
            }
            if power_dev.is_some() {
                c.push("button-power");
            }
            if radio.is_some() {
                c.push("radio");
            }
            if power.is_some() {
                c.push("power");
            }
            c.extend(["dictation", "companion"]);
            // glassd holds the phone link itself (not an external provider).
            if let Some(link) = &companion {
                c.push("companion-link");
                if link.notes_available() {
                    c.push("notes");
                }
            }
            c
        };
        let mut hub = Hub::new(env!("CARGO_PKG_VERSION"), &caps);
        hub.set_screen_source(policy.state());
        hub.set_settings_source(settings);
        hub.set_wear_source(wear.state());
        let mock = opts.mock.then(|| MockInput {
            buf: Vec::new(),
            commands: VecDeque::new(),
            resume_at: None,
            eof: false,
        });
        let touchpad_sysfs = opts.touchpad_sysfs.clone().filter(|d| d.is_dir());
        info!(
            "listening on {} ({})",
            opts.socket.display(),
            caps.join(", ")
        );
        let mut d = Daemon {
            hub,
            listener,
            socket_path: opts.socket.clone(),
            owns_socket,
            peers: opts.peers.clone(),
            mock_companion: None,
            conns: BTreeMap::new(),
            device,
            rec: Recognizer::new(cfg),
            gate: TouchGate::new(opts.screen_off_input),
            policy,
            display,
            mock,
            events: Vec::new(),
            glasshub,
            hub_written: HubWritten::default(),
            don_doff_recheck_us: None,
            last_don_doff: None,
            wear,
            camera: ButtonTracker::new(ButtonId::Camera),
            power: ButtonTracker::new(ButtonId::Power),
            camera_dev,
            power_dev,
            touch_sleep: TouchpadSleep::new(touchpad_sysfs),
            store,
            radio,
            power_policy,
            off_head,
            suspend_offset_us: None,
            power_io: power,
            imu,
            head: HeadService::default(),
            head_requests: BTreeMap::new(),
            samples: Vec::new(),
            imu_reads: 0,
            companion,
            companion_provider: None,
            power_supply: opts.power_supply.clone(),
            battery_last: None,
            battery_deadline: None,
        };
        if d.companion.is_some() {
            d.battery_deadline = Some(now().mono_us);
        }
        // A previous run may have left the touchpad asleep.
        if d.mock.is_none() {
            d.touch_sleep.set(false);
        }
        d.sync_glasshub();
        d.read_don_doff();
        d.update_head();
        // Sensor bring-up summary (grep OMGB-SENSOR in the kmsg log; needs -v).
        info!(
            "OMGB-SENSOR: bring-up: imu={} glasshub={} head_wake_enabled={} wink_enabled={} on_head_detection={} wear.on_head={} screen={:?}",
            d.imu.is_some(),
            d.glasshub.is_some(),
            d.settings().head_wake_enabled.unwrap_or(false),
            d.settings().wink_enabled.unwrap_or(false),
            d.settings().on_head_detection_enabled.unwrap_or(false),
            d.wear.on_head(),
            d.policy.screen(),
        );
        // Bring the radios to the stored (or default: both off) state at start, so Wi-Fi is
        // off by default and a prior run's state does not leak in.
        let s = d.settings();
        d.apply_radio(&s);
        // Start the energy policy: runtime-PM auto, arm wake sources, take the wakelock, and set
        // the active (screen-on) cpufreq profile. Best effort; inert without a back end.
        if let Some(power) = &mut d.power_io {
            power.start(crate::power::CpuProfile::Balanced);
        }
        Ok(d)
    }

    /// Runs forever (until a fatal error).
    pub fn run(&mut self) -> io::Result<()> {
        loop {
            self.step()?;
        }
    }

    fn next_deadline(&self) -> Option<u64> {
        let mut d = [
            self.rec.next_deadline(),
            self.policy.next_deadline(),
            self.mock.as_ref().and_then(|m| m.resume_at),
            self.wear.next_deadline(),
            self.don_doff_recheck_us,
            self.camera.next_deadline(),
            self.power.next_deadline(),
            self.power_io
                .as_ref()
                .and_then(|_| self.power_policy.next_deadline()),
            // Awake only: asleep, the alarm covers it. BOOTTIME to the monotonic base.
            self.power_io
                .as_ref()
                .and(self.off_head.power_off_at())
                .map(|at| {
                    let t = now();
                    t.mono_us + at.saturating_sub(t.boot_us)
                }),
            self.hub.next_deadline(),
            self.battery_deadline,
            self.companion.as_ref().and_then(|c| {
                // Wall clock to the monotonic base, and never a busy loop.
                let wait_ms = c.notes_next_deadline()?.saturating_sub(companion::now_ms());
                Some(now().mono_us + wait_ms.max(companion::notes::MIN_WAIT_MS) * 1000)
            }),
            self.companion.as_ref().and_then(|c| {
                let wait_ms = c.rng_next_deadline()?.saturating_sub(companion::now_ms());
                Some(now().mono_us + wait_ms * 1000)
            }),
        ]
        .into_iter()
        .flatten()
        .min();
        for c in self.conns.values() {
            if let Some(q) = c.queue.deadline() {
                d = Some(d.map_or(q, |x| x.min(q)));
            }
        }
        d
    }

    /// One iteration: timers, writes, `poll`, reads.
    pub fn step(&mut self) -> io::Result<()> {
        self.timers(now());
        self.update_head();
        self.flush(now());

        let timeout = match self.next_deadline() {
            None => -1,
            Some(d) => {
                let t = now().mono_us;
                if d <= t {
                    0
                } else {
                    ((d - t).div_ceil(1000)).min(i32::MAX as u64) as i32
                }
            }
        };
        #[derive(Clone, Copy)]
        enum Src {
            Listener,
            Touchpad,
            Stdin,
            Key(ButtonId),
            DonDoff,
            WinkAttr,
            Imu,
            Alarm,
            Client(ClientId),
            CompanionRng,
            CompanionListener(companion::transport::Transport),
            CompanionConn(companion::ConnId),
        }
        let mut fds = Vec::new();
        let mut srcs = Vec::new();
        let mut add = |fd: RawFd, events: libc::c_short, src: Src| {
            fds.push(libc::pollfd {
                fd,
                events,
                revents: 0,
            });
            srcs.push(src);
        };
        add(self.listener.as_raw_fd(), libc::POLLIN, Src::Listener);
        if let Some(d) = &self.device {
            add(d.as_raw_fd(), libc::POLLIN, Src::Touchpad);
        }
        if let Some(d) = &self.camera_dev {
            add(d.as_raw_fd(), libc::POLLIN, Src::Key(ButtonId::Camera));
        }
        if let Some(d) = &self.power_dev {
            add(d.as_raw_fd(), libc::POLLIN, Src::Key(ButtonId::Power));
        }
        if let Some(fd) = self.imu.as_ref().and_then(|i| i.fd()) {
            add(fd, libc::POLLIN, Src::Imu);
        }
        if let Some(h) = &self.glasshub {
            add(h.don_doff_fd(), glass_hub::sysfs::POLL_EVENTS, Src::DonDoff);
            add(h.wink_fd(), glass_hub::sysfs::POLL_EVENTS, Src::WinkAttr);
        }
        if let Some(fd) = self.power_io.as_ref().and_then(Power::alarm_fd) {
            add(fd, libc::POLLIN, Src::Alarm);
        }
        if let Some(m) = &self.mock
            && !m.eof
        {
            add(0, libc::POLLIN, Src::Stdin);
        }
        for (&id, c) in &self.conns {
            let mut ev = 0;
            if !c.closing {
                ev |= libc::POLLIN;
            }
            if !c.queue.is_empty() {
                ev |= libc::POLLOUT;
            }
            add(c.stream.as_raw_fd(), ev, Src::Client(id));
        }
        if let Some(comp) = &self.companion {
            if let Some(fd) = comp.rng_fd() {
                add(fd, libc::POLLIN, Src::CompanionRng);
            }
            for (fd, transport) in comp.listener_fds() {
                add(fd, libc::POLLIN, Src::CompanionListener(transport));
            }
            for (id, fd, want_write) in comp.conn_fds() {
                let mut ev = libc::POLLIN;
                if want_write {
                    ev |= libc::POLLOUT;
                }
                add(fd, ev, Src::CompanionConn(id));
            }
        }
        // SAFETY: `fds` is a valid array of pollfd of the given length.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(e)
            };
        }
        // `poll` also returns before the kernel has suspended (a timer, a client), and that must
        // not undo the request. A resume shows as BOOTTIME pulling ahead of MONOTONIC; with a
        // cable the kernel never suspends and the alarm is what tells.
        if self.power_policy.is_suspended() {
            let t = now();
            let slept = self
                .suspend_offset_us
                .is_some_and(|o| suspend_offset(t) > o + RESUME_MIN_US);
            let alarm = fds
                .iter()
                .zip(&srcs)
                .any(|(p, s)| matches!(s, Src::Alarm) && p.revents != 0);
            if slept || alarm {
                self.resume_from_suspend(t);
            }
        }
        for (pfd, src) in fds.iter().zip(srcs) {
            if pfd.revents == 0 {
                continue;
            }
            match src {
                Src::Listener => self.accept(),
                Src::Touchpad => self.read_touchpad(),
                Src::Stdin => self.read_stdin(),
                Src::Key(id) => self.read_key(id),
                Src::DonDoff => {
                    info!("OMGB-SENSOR: glasshub don_doff notification");
                    self.read_don_doff();
                }
                Src::WinkAttr => self.read_wink(),
                Src::Imu => self.read_imu(),
                Src::Alarm => {
                    if let Some(power) = &mut self.power_io {
                        power.acknowledge_alarm();
                    }
                }
                Src::Client(id) => {
                    if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                        self.read_client(id);
                    }
                }
                Src::CompanionRng => {
                    let events = match &mut self.companion {
                        Some(c) => c.on_rng_event(),
                        None => Vec::new(),
                    };
                    self.companion_events(events);
                }
                Src::CompanionListener(transport) => {
                    let events = match &mut self.companion {
                        Some(c) => c.accept(transport),
                        None => Vec::new(),
                    };
                    self.companion_events(events);
                }
                Src::CompanionConn(id) => {
                    let mut events = Vec::new();
                    if let Some(c) = &mut self.companion {
                        if pfd.revents & libc::POLLOUT != 0 {
                            events.extend(c.on_writable(id));
                        }
                        if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                            events.extend(c.on_readable(id));
                        }
                    }
                    self.companion_events(events);
                }
            }
        }
        Ok(())
    }

    fn timers(&mut self, t: Now) {
        let mut out = Vec::new();
        self.rec.on_timeout(t.mono_us, &mut out);
        self.touch_outputs(&out, t);
        if let Some(c) = self.policy.on_timeout(t) {
            self.apply_change(c);
        }
        let u = self.wear.on_timeout(t.mono_us);
        self.wear_update(u);
        if self.don_doff_recheck_us.is_some_and(|d| t.mono_us >= d) {
            self.don_doff_recheck_us = None;
            self.read_don_doff();
            self.sync_don_doff_recheck(t.mono_us);
        }
        for id in [ButtonId::Camera, ButtonId::Power] {
            if let Some(b) = self.tracker(id).on_timeout(t.mono_us) {
                self.button_frames(id, vec![b]);
            }
        }
        if self.hub.next_deadline().is_some_and(|d| t.mono_us >= d) {
            let fx = self.hub.on_timeout(t.mono_us);
            self.effects(fx);
        }
        if let Some(m) = &mut self.mock
            && m.resume_at.is_some_and(|r| t.mono_us >= r)
        {
            m.resume_at = None;
        }
        if self.battery_deadline.is_some_and(|d| t.mono_us >= d) {
            self.poll_battery(t);
        }
        let events = self
            .companion
            .as_mut()
            .map(|c| c.notes_tick())
            .unwrap_or_default();
        self.companion_events(events);
        let events = self
            .companion
            .as_mut()
            .map(|c| c.rng_tick())
            .unwrap_or_default();
        self.companion_events(events);
        // Energy policy: the deep-idle window may have elapsed (screen off long enough).
        if self.power_io.is_some()
            && let Some(PowerAction::Suspend) = self.power_policy.on_timeout(t.mono_us)
            && let Some(power) = &mut self.power_io
        {
            power.set_alarm(self.off_head.next_alarm_s(t.boot_us));
            power.suspend();
            self.suspend_offset_us = Some(suspend_offset(t));
        }
        if self.power_io.is_some() && self.off_head.power_off_due(t.boot_us) {
            self.power_off_if_unplugged(t);
        }
        self.run_mock_commands();
    }

    /// The off-head time is up. On a cable the Glass stays on, so a charging Glass left on the
    /// desk is not found off: the wait starts again. So it does if powering off failed.
    fn power_off_if_unplugged(&mut self, t: Now) {
        let cable = companion::battery::cable_connected(&self.power_supply);
        if !cable && let Some(power) = &mut self.power_io {
            power.power_off();
        }
        self.off_head.activity(t.boot_us);
    }

    /// Reads the battery and pushes it to the companion when it changed.
    fn poll_battery(&mut self, t: Now) {
        self.battery_deadline = Some(t.mono_us + BATTERY_POLL_US);
        if self.companion.is_none() {
            return;
        }
        let Some(reading) = companion::battery::read(&self.power_supply) else {
            return;
        };
        if self.battery_last == Some(reading) {
            return;
        }
        self.battery_last = Some(reading);
        let (percent, charging) = reading;
        let events = match &mut self.companion {
            Some(c) => c.battery_changed(percent, charging),
            None => Vec::new(),
        };
        self.companion_events(events);
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    let cred = peer_ucred(stream.as_raw_fd());
                    let who = cred.map_or("unknown peer".to_string(), |(uid, pid)| {
                        format!("uid {uid} pid {pid}")
                    });
                    if self.peers.enabled() && !cred.is_some_and(|(uid, _)| self.peers.allows(uid))
                    {
                        warn!("refused a connection from {who}: not allowed on the socket");
                        deny(&mut stream);
                        continue;
                    }
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let id = self.hub.add_client();
                    info!("client {id} connected ({who})");
                    self.conns.insert(
                        id,
                        Conn {
                            stream,
                            decoder: FrameDecoder::new(),
                            queue: OutQueue::new(),
                            closing: false,
                        },
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    warn!("accept: {e}");
                    return;
                }
            }
        }
    }

    fn read_client(&mut self, id: ClientId) {
        let mut buf = [0u8; 8192];
        let mut frames = Vec::new();
        let mut gone = false;
        let mut bad = None;
        if let Some(c) = self.conns.get_mut(&id) {
            // At most READ_BUDGET bytes per pass, so one busy client cannot starve the others
            // (poll reports the rest next time); frames are parsed after each read.
            let mut budget = READ_BUDGET;
            'read: while budget > 0 {
                let want = buf.len().min(budget);
                match c.stream.read(&mut buf[..want]) {
                    Ok(0) => {
                        gone = true;
                        break;
                    }
                    Ok(n) => {
                        budget -= n;
                        c.decoder.push(&buf[..n]);
                        loop {
                            match c.decoder.next_message::<ClientFrame>() {
                                Ok(Some(f)) => frames.push(f),
                                Ok(None) => break,
                                Err(e) => {
                                    bad = Some(e.to_string());
                                    break 'read;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        gone = true;
                        break;
                    }
                }
            }
        }
        for f in frames {
            if !self.conns.contains_key(&id) || self.conns[&id].closing {
                break;
            }
            let fx = self.hub.on_frame(id, f);
            self.effects(fx);
        }
        if let Some(e) = bad
            && self.conns.get(&id).is_some_and(|c| !c.closing)
        {
            warn!("client {id}: {e}");
            let fx = self.hub.bad_frame(id, &e);
            self.effects(fx);
        }
        if gone {
            self.remove(id);
        }
    }

    fn remove(&mut self, id: ClientId) {
        if self.conns.remove(&id).is_some() {
            info!("client {id} disconnected");
        }
        let fx = self.hub.remove_client(id);
        self.policy.client_gone(now(), id);
        self.head_requests.remove(&id);
        self.effects(fx);
    }

    fn flush(&mut self, t: Now) {
        let mut drop_ids = Vec::new();
        for (&id, c) in &mut self.conns {
            if c.queue.flush_to(&mut c.stream, t.mono_us).is_err() {
                drop_ids.push(id);
            } else if c.queue.expired(t.mono_us) {
                warn!("client {id}: queue full for 2 s, disconnecting");
                drop_ids.push(id);
            } else if c.closing && c.queue.is_empty() {
                drop_ids.push(id);
            }
        }
        for id in drop_ids {
            self.remove(id);
        }
    }

    fn effects(&mut self, fx: Vec<Effect>) {
        let t = now();
        for e in fx {
            match e {
                Effect::Send(id, f) if self.mock_companion.as_ref().is_some_and(|m| m.id == id) => {
                    self.mock_companion_frame(f)
                }
                Effect::Send(id, f) if self.companion_provider == Some(id) => {
                    self.companion_provider_frame(f)
                }
                Effect::Send(id, f) => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.queue.push(&f, t.mono_us);
                    }
                }
                Effect::Close(id) => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        c.closing = true;
                    }
                    self.policy.client_gone(t, id);
                    self.head_requests.remove(&id);
                }
                Effect::Screen {
                    client,
                    action,
                    duration_ms,
                } => {
                    if let Some(c) = self.policy.request(t, client, action, duration_ms) {
                        self.apply_change(c);
                    }
                }
                Effect::SettingsChanged(s) => self.settings_changed(&s),
                Effect::Head { client, request } => {
                    self.head_requests
                        .insert(client, (request.relative_look_up, request.nudge));
                    if let Some(ms) = request.inhibit_look_up_ms {
                        self.head.inhibit_look_up(t.mono_us, ms);
                    }
                }
                Effect::CompanionNotificationAction { key, action_index } => {
                    use omg_protocol::NotificationChoice;
                    let choice = match action_index {
                        Some(i) => NotificationChoice::Action(i),
                        None => NotificationChoice::Dismiss,
                    };
                    let events = match &mut self.companion {
                        Some(c) => c.notification_action(key, choice),
                        None => Vec::new(),
                    };
                    self.companion_events(events);
                }
                Effect::NoteAdd(add) => {
                    let events = match &mut self.companion {
                        Some(c) => c.note_add(add),
                        None => companion::notes::refused_add(&add),
                    };
                    self.companion_events(events);
                }
                Effect::NoteSpeak(speak) => {
                    let events = match &mut self.companion {
                        Some(c) => c.note_speak(speak),
                        None => companion::notes::refused_speak(&speak),
                    };
                    self.companion_events(events);
                }
            }
        }
    }

    fn publish(&mut self, body: service_frame::Body) {
        let fx = self.hub.publish(ServiceFrame { body: Some(body) });
        self.effects(fx);
    }

    fn apply_change(&mut self, c: Change) {
        info!("screen {:?} ({})", c.screen, c.reason);
        if c.screen == Screen::Off {
            // The head is still where it was at the sleep: no look-up wake for a while.
            self.head.screen_went_off(c.at_us);
        }
        self.display.set(c.screen);
        self.drive_power(c.screen);
        let state: ScreenState = c.to_state();
        self.publish(service_frame::Body::ScreenState(state));
    }

    /// Follows a screen change through the energy policy: tell the policy whether the panel is
    /// off (dim counts as on), then apply the cpufreq profile and the panel brightness. The
    /// screen-off blanking itself is `FBIOBLANK` in [`crate::display`] and is not changed here.
    fn drive_power(&mut self, screen: Screen) {
        let off = screen == Screen::Off;
        self.power_policy.screen_changed(now().mono_us, off);
        if !off && let Some(power) = &mut self.power_io {
            power.stay_awake();
        }
        let profile = self.power_policy.cpu_profile();
        // With the policy off, keep the panel at full brightness whatever the screen state.
        let bright = if self.power_policy.power_saver() {
            screen
        } else {
            Screen::On
        };
        if let Some(power) = &mut self.power_io {
            power.set_cpu_profile(profile);
            power.set_brightness_for(bright);
        }
    }

    /// The system resumed after a suspend: re-arm the policy, re-take the wakelock, force the
    /// display to be re-driven on the next wake (so the panel bring-up is never skipped), and
    /// read the on-head sensor, whose reading is what most of these wakes are for.
    fn resume_from_suspend(&mut self, t: Now) {
        let slept = self
            .suspend_offset_us
            .take()
            .map_or(0, |o| suspend_offset(t).saturating_sub(o));
        info!("power: resumed after {} ms asleep", slept / 1000);
        self.power_policy.resumed(t.mono_us);
        if let Some(power) = &mut self.power_io {
            power.resume();
        }
        self.display.invalidate();
        self.read_don_doff();
    }

    fn touch_outputs(&mut self, outs: &[Output], t: Now) {
        if !outs.is_empty() {
            let u = self.wear.user_input(t.mono_us);
            self.wear_update(u);
        }
        for o in outs {
            for step in self.gate.handle(o, &mut self.policy, t) {
                match step {
                    Step::Deliver(f) => {
                        let fx = self.hub.publish(f);
                        self.effects(fx);
                    }
                    Step::Screen(c) => self.apply_change(c),
                }
            }
        }
    }

    fn feed_touch(&mut self, events: &[RawEvent]) {
        let mut out = Vec::new();
        for e in events {
            self.rec
                .set_screen_off_decoding(self.gate.decoding(&self.policy));
            self.rec.push(e, &mut out);
            if !out.is_empty() {
                let o = std::mem::take(&mut out);
                self.touch_outputs(&o, now());
            }
        }
    }

    fn read_touchpad(&mut self) {
        let Some(dev) = &mut self.device else { return };
        self.events.clear();
        match dev.read_events(&mut self.events) {
            Ok(_) => {
                let evs: Vec<RawEvent> = self
                    .events
                    .iter()
                    .map(|e| RawEvent {
                        time_us: e.time_us,
                        type_: e.type_,
                        code: e.code,
                        value: e.value,
                    })
                    .collect();
                self.feed_touch(&evs);
            }
            Err(e) => {
                warn!("touchpad read failed ({e}); touchpad disabled");
                self.device = None;
                let mut out = Vec::new();
                self.rec.cancel(now().mono_us, &mut out);
                self.touch_outputs(&out, now());
            }
        }
    }

    fn read_stdin(&mut self) {
        let Some(m) = &mut self.mock else { return };
        let mut buf = [0u8; 4096];
        // SAFETY: `buf` is valid for its length; fd 0 is standard input.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            if n == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                m.eof = true;
                info!("standard input closed; mock keeps serving");
            }
            return;
        }
        m.buf.extend_from_slice(&buf[..n as usize]);
        while let Some(i) = m.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = m.buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line);
            match mockcmd::parse(line.trim_end()) {
                Ok(Some(c)) => m.commands.push_back(c),
                Ok(None) => {}
                Err(e) => warn!("mock: {e}"),
            }
        }
        self.run_mock_commands();
    }

    fn run_mock_commands(&mut self) {
        loop {
            let Some(m) = &mut self.mock else { return };
            if m.resume_at.is_some() {
                return;
            }
            let Some(c) = m.commands.pop_front() else {
                return;
            };
            self.mock_command(c);
        }
    }

    fn settings(&self) -> Settings {
        self.hub
            .settings()
            .cloned()
            .unwrap_or_else(settings_rules::defaults)
    }

    /// Synthetic touch events, shifted so the last event is now.
    fn synth_touch(&mut self, build: impl FnOnce(&mut Synth, &Config)) {
        let cfg = self.rec.config().clone();
        let mut s = Synth::new(&cfg, 0);
        build(&mut s, &cfg);
        let mut evs = s.take();
        let last = evs.last().map_or(0, |e| e.time_us);
        let shift = now().mono_us.saturating_sub(last);
        for e in &mut evs {
            e.time_us += shift;
        }
        self.feed_touch(&evs);
    }

    fn mock_command(&mut self, c: Command) {
        let t = now();
        info!("mock: {c:?}");
        match c {
            Command::Tap { fingers } => self.synth_touch(|s, _| s.tap(fingers, 60)),
            Command::LongPress => self.synth_touch(|s, cfg| s.tap(1, cfg.long_press_ms + 100)),
            Command::Swipe { dir, fingers } => self.synth_touch(|s, cfg| match dir {
                Direction::Forward | Direction::Backward => {
                    s.swipe(dir, fingers, cfg.x.span() * 0.4, 100)
                }
                Direction::Down | Direction::Up => s.swipe(dir, fingers, cfg.y.span() * 0.5, 150),
            }),
            Command::Scroll { displacement } => {
                self.synth_touch(|s, cfg| s.drag(1, displacement * cfg.x.span(), 600))
            }
            Command::Camera { long } => self.mock_button(ButtonId::Camera, long),
            Command::Power { long } => self.mock_button(ButtonId::Power, long),
            Command::Hinge { open } => {
                let u = self.wear.hinge(t.mono_us, open);
                self.wear_update(u);
            }
            Command::Wear { on } => {
                if !self.wear.detection() {
                    info!(
                        "mock: on_head_detection_enabled is false: the sensor reading is kept, not used"
                    );
                }
                let u = self.wear.sensor(t.mono_us, on);
                self.wear_update(u);
            }
            Command::LookUp | Command::LookDown | Command::RelativeLookUp | Command::Nudge => {
                let kind = match c {
                    Command::LookUp => HeadGestureKind::LookUp,
                    Command::LookDown => HeadGestureKind::LookDown,
                    Command::RelativeLookUp => HeadGestureKind::RelativeLookUp,
                    _ => HeadGestureKind::Nudge,
                };
                self.update_head();
                match self.head.inject(kind, t.mono_us) {
                    Some(g) => self.head_gesture(g),
                    None => info!(
                        "mock: {kind:?} not delivered (its detector does not run now, or it is inhibited)"
                    ),
                }
            }
            Command::Wink => self.wink(t.mono_us),
            Command::Dim => {
                if let Some(ch) = self.policy.force_dim(t) {
                    self.apply_change(ch);
                }
            }
            Command::Wait { ms } => {
                if let Some(m) = &mut self.mock {
                    m.resume_at = Some(t.mono_us + ms * 1000);
                }
            }
            Command::Companion(c) => self.mock_companion_command(c),
            Command::CompanionQr(payload) => self.companion_qr(&payload),
        }
    }

    /// A frame from the simulated provider to the hub.
    fn mock_companion_says(&mut self, body: client_frame::Body) -> Vec<Effect> {
        let Some(m) = &self.mock_companion else {
            return Vec::new();
        };
        let id = m.id;
        let fx = self.hub.on_frame(
            id,
            ClientFrame {
                seq: 0,
                body: Some(body),
            },
        );
        for e in &fx {
            if let Effect::Send(to, f) = e
                && *to == id
                && let Some(service_frame::Body::Error(err)) = &f.body
            {
                warn!("mock companion: {}", err.message);
            }
        }
        fx
    }

    fn mock_companion_command(&mut self, c: CompanionCommand) {
        use dictation_state::State as St;
        let newest = self
            .mock_companion
            .as_ref()
            .and_then(|m| m.links.last().copied());
        let state = |link: u32, st: St, error: &str| {
            client_frame::Body::ProviderDictationState(DictationState {
                request_id: link,
                state: st as i32,
                error: error.into(),
            })
        };
        let fx = match c {
            CompanionCommand::On { capabilities } => {
                if self.mock_companion.is_none() {
                    let id = self.hub.add_client();
                    self.mock_companion = Some(MockCompanion {
                        id,
                        links: Vec::new(),
                    });
                    let mut fx = self.mock_companion_says(client_frame::Body::Hello(Hello {
                        api_version: glass_api::API_VERSION,
                        client_name: "mock-companion".into(),
                    }));
                    fx.extend(
                        self.mock_companion_says(client_frame::Body::CompanionRegister(
                            CompanionRegister {
                                name: "mock-companion".into(),
                            },
                        )),
                    );
                    if self.hub.relay().provider() != Some(id) {
                        warn!("mock companion: a real provider is registered");
                        self.mock_companion = None;
                        fx.extend(self.hub.remove_client(id));
                        self.effects(fx);
                        return;
                    }
                    self.effects(fx);
                }
                self.mock_companion_says(client_frame::Body::CompanionStatus(CompanionStatus {
                    phone_connected: true,
                    phone_capabilities: capabilities,
                    phone_name: "Mock phone".into(),
                    ..Default::default()
                }))
            }
            CompanionCommand::Off => {
                if let Some(m) = &mut self.mock_companion {
                    m.links.clear();
                }
                self.mock_companion_says(client_frame::Body::CompanionStatus(CompanionStatus {
                    phone_connected: false,
                    ..Default::default()
                }))
            }
            CompanionCommand::Gone => match self.mock_companion.take() {
                Some(m) => self.hub.remove_client(m.id),
                None => Vec::new(),
            },
            CompanionCommand::Transcript { text, last } => match newest {
                Some((link, reply)) => {
                    if last
                        && !reply
                        && let Some(m) = &mut self.mock_companion
                    {
                        m.links.retain(|l| l.0 != link);
                    }
                    self.mock_companion_says(client_frame::Body::ProviderTranscript(Transcript {
                        request_id: link,
                        text,
                        r#final: last,
                    }))
                }
                None => {
                    warn!("mock companion: no open dictation request");
                    Vec::new()
                }
            },
            CompanionCommand::Error { error } => match newest {
                Some((link, _)) => {
                    if let Some(m) = &mut self.mock_companion {
                        m.links.retain(|l| l.0 != link);
                    }
                    self.mock_companion_says(state(link, St::Failed, &error))
                }
                None => {
                    warn!("mock companion: no open dictation request");
                    Vec::new()
                }
            },
            CompanionCommand::ReplyResult { ok } => match newest {
                Some((link, true)) => {
                    if let Some(m) = &mut self.mock_companion {
                        m.links.retain(|l| l.0 != link);
                    }
                    let st = if ok { St::Sent } else { St::Failed };
                    self.mock_companion_says(state(link, st, if ok { "" } else { "send-failed" }))
                }
                _ => {
                    warn!("mock companion: no open reply");
                    Vec::new()
                }
            },
        };
        self.effects(fx);
    }

    /// A frame glassd sent to the simulated provider.
    fn mock_companion_frame(&mut self, f: ServiceFrame) {
        let Some(m) = &mut self.mock_companion else {
            return;
        };
        let fx = match f.body {
            Some(service_frame::Body::ProviderDictationStart(d)) => {
                info!("mock companion: dictation {} ({})", d.request_id, d.purpose);
                m.links.push((d.request_id, d.purpose != "free"));
                self.mock_companion_says(client_frame::Body::ProviderDictationState(
                    DictationState {
                        request_id: d.request_id,
                        state: dictation_state::State::Listening as i32,
                        error: String::new(),
                    },
                ))
            }
            Some(service_frame::Body::ProviderDictationCancel(c)) => {
                m.links.retain(|l| l.0 != c.request_id);
                Vec::new()
            }
            Some(service_frame::Body::ProviderDictationConfirm(c)) => {
                if !c.send {
                    m.links.retain(|l| l.0 != c.request_id);
                }
                Vec::new()
            }
            _ => Vec::new(),
        };
        self.effects(fx);
    }

    /// Carries out the companion link's events: link state to the launcher (via the internal
    /// provider), notifications to the timeline, locale, and radio/Wi-Fi requests.
    fn companion_events(&mut self, events: Vec<companion::Event>) {
        use companion::Event as E;
        for e in events {
            match e {
                E::Connected {
                    name,
                    capabilities,
                    transport,
                } => self.companion_connected(name, capabilities, transport),
                E::Disconnected => self.companion_disconnected(),
                // The link already acked LocaleChanged. Persist the chosen language in Settings
                // and tell TOPIC_SETTINGS subscribers, so the launcher i18n picks it up the same
                // way as any other setting (it reads Settings.locale from the snapshot).
                E::ApplyLocale(l) => {
                    info!("companion: locale -> {}", l.code());
                    self.apply_settings_change(Settings {
                        locale: Some(proto_locale(l) as i32),
                        ..Default::default()
                    });
                }
                E::SettingsGet => {
                    let s = companion_settings(&self.settings());
                    let events = match &mut self.companion {
                        Some(c) => c.send_settings(s),
                        None => Vec::new(),
                    };
                    self.companion_events(events);
                }
                // settings_changed pushes the result back to the companion.
                E::SettingsSet(s) => self.apply_settings_change(settings_from_companion(&s)),
                E::Note(event) => self.publish(service_frame::Body::NoteEvent(event)),
                E::Notification(n) => self.publish(service_frame::Body::CompanionNotification(
                    to_proto_notification(n),
                )),
                E::RemoveNotification(key) => {
                    self.publish(service_frame::Body::CompanionNotificationRemoved(
                        CompanionNotificationRemoved { key },
                    ))
                }
                E::SetRadio { radio, on } => self.companion_set_radio(radio, on),
                E::SetAirplane(on) => {
                    self.apply_settings_change(Settings {
                        airplane_mode: Some(on),
                        ..Default::default()
                    });
                    self.report_radio_state();
                }
                E::WifiBringUp(_) => {
                    self.apply_settings_change(Settings {
                        wifi_enabled: Some(true),
                        ..Default::default()
                    });
                    self.report_radio_state();
                }
                E::WifiTearDown => {
                    self.apply_settings_change(Settings {
                        wifi_enabled: Some(false),
                        ..Default::default()
                    });
                    self.report_radio_state();
                }
            }
        }
    }

    fn companion_set_radio(&mut self, radio: omg_protocol::Radio, on: bool) {
        let change = match radio {
            omg_protocol::Radio::Wifi => Settings {
                wifi_enabled: Some(on),
                ..Default::default()
            },
            omg_protocol::Radio::Bt => Settings {
                bluetooth_enabled: Some(on),
                ..Default::default()
            },
        };
        self.apply_settings_change(change);
        self.report_radio_state();
    }

    /// Merges a settings change (e.g. from the companion), applies it, persists it, and tells
    /// the launcher's `Settings` subscribers, as a `SetSettings` from a client would.
    fn apply_settings_change(&mut self, change: Settings) {
        let mut s = self.settings();
        settings_rules::merge(&mut s, &change);
        self.settings_changed(&s);
        self.publish(service_frame::Body::Settings(s));
    }

    /// Reports the current radio state to the companion.
    fn report_radio_state(&mut self) {
        let Some((wifi, bt)) = self.radio.as_ref().map(|r| r.state()) else {
            return;
        };
        let wifi_ip = self.radio.as_mut().and_then(|r| r.wifi_ip());
        let s = self.settings();
        let radios = omg_protocol::RadioState {
            wifi_on: wifi.unwrap_or(false),
            bt_on: bt.unwrap_or(false),
            airplane_mode: s.airplane_mode.unwrap_or(false),
            wifi_ssid: None,
            wifi_ip,
        };
        let events = match &mut self.companion {
            Some(c) => c.radio_state_changed(radios),
            None => Vec::new(),
        };
        self.companion_events(events);
    }

    /// A companion authenticated: register as the hub's companion provider (if not already) and
    /// report the link to the launcher.
    fn companion_connected(
        &mut self,
        name: String,
        capabilities: Vec<String>,
        transport: companion::transport::Transport,
    ) {
        if self.companion_provider.is_none() {
            let id = self.hub.add_client();
            self.companion_provider = Some(id);
            let mut fx = self.companion_provider_says(client_frame::Body::Hello(Hello {
                api_version: glass_api::API_VERSION,
                client_name: "companion-link".into(),
            }));
            fx.extend(
                self.companion_provider_says(client_frame::Body::CompanionRegister(
                    CompanionRegister {
                        name: "companion-link".into(),
                    },
                )),
            );
            if self.hub.relay().provider() != Some(id) {
                warn!("companion: another provider is registered; not surfacing the link");
                self.companion_provider = None;
                fx.extend(self.hub.remove_client(id));
                self.effects(fx);
                return;
            }
            self.effects(fx);
        }
        let paired = self.companion.as_ref().is_some_and(|c| c.paired());
        let fx =
            self.companion_provider_says(client_frame::Body::CompanionStatus(CompanionStatus {
                phone_connected: true,
                phone_capabilities: capabilities,
                phone_name: name,
                paired,
                transport: transport.as_str().into(),
            }));
        self.effects(fx);
    }

    fn companion_disconnected(&mut self) {
        let paired = self.companion.as_ref().is_some_and(|c| c.paired());
        let fx =
            self.companion_provider_says(client_frame::Body::CompanionStatus(CompanionStatus {
                phone_connected: false,
                phone_capabilities: Vec::new(),
                phone_name: String::new(),
                paired,
                transport: String::new(),
            }));
        self.effects(fx);
    }

    fn companion_provider_says(&mut self, body: client_frame::Body) -> Vec<Effect> {
        let Some(id) = self.companion_provider else {
            return Vec::new();
        };
        self.hub.on_frame(
            id,
            ClientFrame {
                seq: 0,
                body: Some(body),
            },
        )
    }

    /// A frame the hub routed to the companion provider. glassd's link does not relay dictation
    /// yet, so a relayed `DictationStart` is failed with `unsupported` rather than left hanging.
    fn companion_provider_frame(&mut self, f: ServiceFrame) {
        let fx = match f.body {
            Some(service_frame::Body::ProviderDictationStart(d)) => self.companion_provider_says(
                client_frame::Body::ProviderDictationState(DictationState {
                    request_id: d.request_id,
                    state: dictation_state::State::Failed as i32,
                    error: "unsupported".into(),
                }),
            ),
            _ => Vec::new(),
        };
        self.effects(fx);
    }

    /// Feeds a decoded Wi-Fi QR bootstrap (`OMG1:` payload): join the network with the merged
    /// radio control, then dial the companion if the QR carried an address (item 6). The
    /// camera-side scan is a later phase; this is the decode+join entry point.
    pub fn companion_qr(&mut self, payload: &str) {
        let boot = match companion::bootstrap::decode(payload) {
            Ok(b) => b,
            Err(e) => {
                warn!("companion: bad QR bootstrap ({e:?})");
                return;
            }
        };
        info!(
            "companion: QR ssid {:?} addr {:?}",
            boot.ssid, boot.companion_addr
        );
        if let Some(radio) = &mut self.radio {
            let conf = companion::bootstrap::wpa_supplicant_conf(
                &boot.ssid,
                boot.auth,
                boot.psk.as_deref(),
            );
            radio.join_wifi(&conf, Path::new("/var/run/wpa_supplicant_glassd.conf"));
            // Or the next settings change would turn it off again.
            self.apply_settings_change(Settings {
                wifi_enabled: Some(true),
                ..Default::default()
            });
        }
        if let Some(addr) = &boot.companion_addr {
            match &mut self.companion {
                Some(c) => match c.connect_tcp(addr) {
                    Ok(events) => self.companion_events(events),
                    Err(e) => warn!("companion: cannot dial {addr} ({e})"),
                },
                None => info!("companion: --companion off; not dialing {addr}"),
            }
        }
    }

    /// A press of a button from the mock: down, then (long) the hold time, then up.
    fn mock_button(&mut self, id: ButtonId, long: bool) {
        let t = now().mono_us;
        self.on_key(id, crate::buttons::KEY_DOWN, t);
        let mut up = t;
        if long {
            up = t + LONG_PRESS_MS * 1000;
            if let Some(b) = self.tracker(id).on_timeout(up) {
                self.button_frames(id, vec![b]);
            }
        }
        self.on_key(id, crate::buttons::KEY_UP, up);
    }

    fn tracker(&mut self, id: ButtonId) -> &mut ButtonTracker {
        match id {
            ButtonId::Power => &mut self.power,
            _ => &mut self.camera,
        }
    }

    fn read_key(&mut self, id: ButtonId) {
        let dev = match id {
            ButtonId::Power => &mut self.power_dev,
            _ => &mut self.camera_dev,
        };
        let Some(d) = dev else { return };
        self.events.clear();
        if let Err(e) = d.read_events(&mut self.events) {
            warn!("{id:?} button read failed ({e}); button disabled");
            *dev = None;
            self.tracker(id).cancel();
            return;
        }
        let code = match id {
            ButtonId::Power => KEY_POWER,
            _ => KEY_CAMERA,
        };
        let keys = key_events(&self.events, code);
        if !keys.is_empty() {
            self.off_head.activity(now().boot_us);
        }
        for (value, at) in keys {
            self.on_key(id, value, at);
        }
    }

    /// One key event of a button. The camera button wakes the screen on key down (off the head
    /// only for the quick timeout); the frames are published; a short power press toggles a manual off-head.
    fn on_key(&mut self, id: ButtonId, value: i32, at_us: u64) {
        let screen_off = self.policy.is_off();
        let (down, frames) = self.tracker(id).key(value, at_us, screen_off);
        if down
            && id == ButtonId::Camera
            && let Some(ch) = self.policy.wake_key(now(), "button")
        {
            self.apply_change(ch);
        }
        self.button_frames(id, frames);
    }

    fn button_frames(&mut self, id: ButtonId, frames: Vec<Button>) {
        // A press that takes back an assumed off-head has done its job: on the power key it must
        // not also force the Glass off the head.
        let mut took_back = false;
        if !frames.is_empty() {
            let u = self.wear.user_input(now().mono_us);
            took_back = u.is_some();
            self.wear_update(u);
        }
        for b in frames {
            info!("button {id:?} {:?}", ButtonAction::try_from(b.action));
            let press = b.action == ButtonAction::Press as i32;
            self.publish(service_frame::Body::Button(b));
            if id == ButtonId::Power && press && !took_back {
                let u = self.wear.power_key(now().mono_us);
                self.wear_update(u);
            }
        }
    }

    /// Publishes a wear change and, when the wear itself changed, applies it: the screen
    /// (off, or a quick wake on don), the touchpad's sleep mode, and a touch in progress is
    /// cancelled off the head.
    fn wear_update(&mut self, u: Option<Update>) {
        if let Some(u) = u {
            let on = u.state.wear == Wear::OnHead as i32;
            info!(
                "wear {:?} ({:?})",
                Wear::try_from(u.state.wear),
                WearSource::try_from(u.state.source)
            );
            self.publish(service_frame::Body::WearState(u.state));
            self.off_head.set_watching(!self.wear.hub_paused());
            if u.wear_changed {
                let t = now();
                self.power_policy.set_off_head(t.mono_us, !on);
                self.off_head.set_off_head(t.boot_us, !on);
                // Now, not only at the next request: a suspend may already be pending.
                if let Some(power) = &mut self.power_io {
                    power.set_alarm(self.off_head.next_alarm_s(t.boot_us));
                }
                self.touch_sleep.set(!on);
                if let Some(ch) = self.policy.set_on_head(t, on) {
                    self.apply_change(ch);
                }
                if !on {
                    let mut out = Vec::new();
                    self.rec.cancel(t.mono_us, &mut out);
                    self.touch_outputs(&out, t);
                }
            }
        }
        self.sync_glasshub();
    }

    /// A glasshub (or the mock) can sense the wear.
    fn hub_present(&self) -> bool {
        self.glasshub.is_some() || self.mock.is_some()
    }

    /// Writes the glasshub enables that differ from what it has: on-head detection, the wink
    /// interrupt, and the pause while forced off the head.
    fn sync_glasshub(&mut self) {
        let wink = self.settings().wink_enabled.unwrap_or(false);
        let detection = self.wear.detection();
        let pause = self.wear.hub_paused();
        let Some(h) = &self.glasshub else { return };
        let w = &mut self.hub_written;
        let armed = sync_attr(&mut w.detection, detection, "on-head detection", |v| {
            h.set_on_head_detection(v)
        }) && detection;
        sync_attr(&mut w.wink, wink, "wink", |v| h.set_wink(v));
        let resumed = sync_attr(&mut w.pause, pause, "pause", |v| h.set_pause(v)) && !pause;
        // The state may have changed while detection was off or the hub paused, without a
        // notification: read it again. (This ends: a second pass writes nothing.)
        if armed || resumed {
            self.read_don_doff();
        }
        self.sync_don_doff_recheck(now().mono_us);
    }

    /// While on-head detection is armed, `don_doff` is also read on a timer. On Model B no
    /// notification arrived after arming, though the sensor reads correctly (2026-11-03: 690
    /// worn, 327 off, with `don_doff` 1 and 0), so a state change could go unseen.
    fn sync_don_doff_recheck(&mut self, now_us: u64) {
        let wanted = self.glasshub.is_some() && self.wear.detection();
        if !wanted {
            self.don_doff_recheck_us = None;
        } else if self.don_doff_recheck_us.is_none() {
            self.don_doff_recheck_us = Some(now_us + DON_DOFF_RECHECK_MS * 1000);
        }
    }

    /// The glasshub failed: stop using it. Without it there is no on-head sensing (forced on
    /// the head) and no wink.
    fn drop_glasshub(&mut self, why: &str) {
        warn!("glasshub read failed ({why}); glasshub disabled");
        self.glasshub = None;
        let u = self.wear.set_detection(now().mono_us, false);
        self.wear_update(u);
    }

    /// Re-reads `don_doff` after a notification (or at start).
    fn read_don_doff(&mut self) {
        let Some(h) = &mut self.glasshub else { return };
        match h.read_don_doff() {
            Ok(Some(on)) => {
                if self.last_don_doff != Some(on) {
                    self.last_don_doff = Some(on);
                    info!("OMGB-SENSOR: glasshub don_doff read: on_head={on}");
                    if !on && !self.wear.sensor_trusted() {
                        info!(
                            "glasshub don_doff 0 ignored: the sensor has not reported on-head yet"
                        );
                    }
                }
                let u = self.wear.sensor(now().mono_us, on);
                self.wear_update(u);
            }
            Ok(None) => warn!("glasshub don_doff: unexpected value"),
            Err(e) => self.drop_glasshub(&e.to_string()),
        }
    }

    /// Acknowledges a wink interrupt and reports it.
    fn read_wink(&mut self) {
        let Some(h) = &mut self.glasshub else { return };
        match h.ack_wink() {
            // 0: no wink pending (a spurious wake, or already acknowledged).
            Ok(0) => {}
            Ok(_) => {
                info!("OMGB-SENSOR: glasshub wink interrupt acknowledged");
                self.wink(now().mono_us)
            }
            Err(e) => self.drop_glasshub(&e.to_string()),
        }
    }

    /// A wink is reported when `wink_enabled` is true and the Glass is on the head.
    fn wink(&mut self, at_us: u64) {
        if !self.settings().wink_enabled.unwrap_or(false) {
            info!("wink ignored (wink_enabled is false)");
        } else if !self.wear.on_head() {
            info!("wink ignored (off the head)");
        } else {
            self.publish(service_frame::Body::Wink(Wink {
                timestamp_us: at_us,
            }));
        }
    }

    /// Applies and stores changed settings.
    fn settings_changed(&mut self, s: &Settings) {
        if let Some(ms) = s.screen_timeout_ms {
            self.policy.set_timeout(ms);
        }
        let detection = s.on_head_detection_enabled.unwrap_or(false) && self.hub_present();
        let u = self.wear.set_detection(now().mono_us, detection);
        self.wear_update(u);
        self.apply_radio(s);
        self.apply_power(s);
        // Keep the companion's reported UI language in step (idempotent; pushes LocaleChanged to
        // the phone only when the value actually changed and it was not the companion's own set).
        if s.locale.is_some() {
            let events = match &mut self.companion {
                Some(c) => c.locale_changed(companion_locale(s.locale)),
                None => Vec::new(),
            };
            self.companion_events(events);
        }
        let events = match &mut self.companion {
            Some(c) => c.send_settings(companion_settings(s)),
            None => Vec::new(),
        };
        self.companion_events(events);
        if let Some(st) = &self.store
            && let Err(e) = st.save(s)
        {
            warn!("cannot store the settings in {}: {e}", st.path().display());
        }
    }

    /// Drives the radios to match the settings. Airplane mode forces both off while keeping the
    /// per-radio wishes, which are restored when it is turned off.
    fn apply_radio(&mut self, s: &Settings) {
        let Some(radio) = self.radio.as_mut() else {
            return;
        };
        let (wifi, bluetooth) = effective_radio(s);
        radio.apply(wifi, bluetooth);
    }

    /// Applies the energy-policy settings: update the idle -> suspend decision and re-apply the
    /// cpufreq profile and brightness for the current screen (so toggling `power_saver` restores
    /// a full-brightness, responsive state at once). Inert without a back end.
    fn apply_power(&mut self, s: &Settings) {
        // No back end: the policy stays inert (power_saver forced off).
        let enabled = self.power_io.is_some();
        self.power_policy.apply_settings(
            s.power_saver.unwrap_or(true) && enabled,
            s.deep_sleep_enabled.unwrap_or(false),
            s.deep_sleep_idle_ms.unwrap_or(60_000),
        );
        self.off_head
            .set_power_off_min(s.off_head_power_off_min.unwrap_or(30));
        self.drive_power(self.policy.screen());
    }

    /// The head detectors' context from the screen, the wear, the settings and the requests
    /// of clients subscribed to `TOPIC_HEAD`.
    fn head_context(&self) -> HeadContext {
        let s = self.settings();
        let (mut relative, mut nudge) = (false, false);
        for (&id, &(r, n)) in &self.head_requests {
            if self.hub.is_subscribed(id, glass_api::proto::Topic::Head) {
                relative |= r;
                nudge |= n;
            }
        }
        HeadContext {
            screen_off: self.policy.is_off(),
            on_head: self.wear.on_head(),
            head_wake_enabled: s.head_wake_enabled.unwrap_or(true),
            wake_angle_deg: s.head_wake_angle_deg.unwrap_or(30.0),
            nudge_wanted: nudge || s.nudge_sleep_enabled.unwrap_or(false),
            relative_wanted: relative,
        }
    }

    /// Brings the detectors and the IMU stream in line with the current state.
    fn update_head(&mut self) {
        let ctx = self.head_context();
        if ctx != self.head.context() {
            self.head.set_context(ctx);
        }
        let want = self.head.channels();
        if let Some(imu) = &mut self.imu
            && imu.channels() != want
        {
            info!("IMU -> accel {} gyro {}", want.accel, want.gyro);
            if let Err(e) = imu.set_channels(want) {
                warn!("IMU setup failed ({e}); head gestures disabled");
                self.imu = None;
            }
        }
    }

    fn read_imu(&mut self) {
        let Some(imu) = &mut self.imu else { return };
        self.samples.clear();
        let t = now();
        if let Err(e) = imu.read((t.mono_us * 1000) as i64, &mut self.samples) {
            warn!("IMU read failed ({e}); head gestures disabled");
            self.imu = None;
            return;
        }
        let samples = std::mem::take(&mut self.samples);
        // Liveness: confirm samples actually flow from the IMU character device, and show the
        // current pitch, every ~2 s (100 batches at 20 ms). Reveals a silent/stalled IMU.
        self.imu_reads += 1;
        if self.imu_reads <= 3 || self.imu_reads.is_multiple_of(100) {
            let last = samples.last();
            info!(
                "OMGB-SENSOR: IMU read #{} ({} samples) accel={:?} gyro={:?}",
                self.imu_reads,
                samples.len(),
                last.and_then(|s| s.accel),
                last.and_then(|s| s.gyro),
            );
        }
        for s in &samples {
            for g in self.head.push(s, t.mono_us) {
                self.head_gesture(g);
            }
        }
        self.samples = samples;
    }

    /// Publishes a head gesture and applies it: a look-up wakes the screen with the quick
    /// timeout; a nudge turns it off when `nudge_sleep_enabled`, unless a touch is in progress.
    fn head_gesture(&mut self, g: HeadGesture) {
        let t = now();
        let kind = HeadGestureKind::try_from(g.kind).unwrap_or(HeadGestureKind::Unspecified);
        info!(
            "OMGB-SENSOR: head gesture {kind:?} at {:.1} deg",
            g.pitch_deg
        );
        self.publish(service_frame::Body::HeadGesture(g));
        let change = match kind {
            HeadGestureKind::LookUp => self.policy.wake(t, "look-up", true),
            HeadGestureKind::Nudge
                if self.settings().nudge_sleep_enabled.unwrap_or(false)
                    && !self.gate.touching() =>
            {
                self.policy.sleep(t, "nudge")
            }
            _ => None,
        };
        if let Some(ch) = change {
            self.apply_change(ch);
        }
        self.update_head();
    }
}

/// Maps the companion-link `Notification` to the launcher `CompanionNotification` timeline card.
fn to_proto_notification(n: omg_protocol::Notification) -> CompanionNotification {
    CompanionNotification {
        key: n.key,
        app_name: n.app_name,
        title: n.title,
        text: n.text,
        posted_at_ms: n.posted_at_ms,
        actions: n
            .actions
            .into_iter()
            .map(|a| CompanionNotificationActionInfo {
                index: a.index,
                title: a.title,
                reply: a.reply,
            })
            .collect(),
        wake: n.wake,
    }
}

/// Tells a refused peer why, then drops the connection.
fn deny(stream: &mut UnixStream) {
    use std::io::Write;
    let f = ServiceFrame {
        body: Some(service_frame::Body::Error(ServiceError {
            code: ErrorCode::Denied as i32,
            message: "not allowed on this socket".into(),
            seq: 0,
        })),
    };
    if let Ok(bytes) = glass_api::frame_bytes(&f) {
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(100)));
        let _ = stream.write_all(&bytes);
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.owns_socket {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

/// The state changes of `code` among `events`, as (value, time). Readable does not mean pressed:
/// the kernel's `input_reset_device` sends a lone `SYN_REPORT` on every resume.
fn key_events(events: &[glass_evdev::Event], code: u16) -> Vec<(i32, u64)> {
    events
        .iter()
        .filter(|e| e.type_ == EV_KEY && e.code == code)
        .map(|e| (e.value, e.time_us))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_sync_of_a_resume_is_not_a_key_press() {
        let event = |type_, code, value| glass_evdev::Event {
            time_us: 7,
            type_,
            code,
            value,
        };
        assert_eq!(key_events(&[event(0, 0, 1)], KEY_CAMERA), vec![]);
        let press = [event(EV_KEY, KEY_CAMERA, 1), event(0, 0, 0)];
        assert_eq!(key_events(&press, KEY_CAMERA), vec![(1, 7)]);
        assert_eq!(key_events(&press, KEY_POWER), vec![]);
    }

    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("glassd-bind-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_hub_daemon(name: &str) -> (Daemon, PathBuf) {
        fake_hub_daemon_with(name, "1\n")
    }

    /// A daemon with a fake glasshub whose `don_doff` starts at `don_doff`.
    fn fake_hub_daemon_with(name: &str, don_doff: &str) -> (Daemon, PathBuf) {
        let d = dir(name);
        let hub = d.join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        for (f, v) in [
            ("version", "1.14\n"),
            ("don_doff", don_doff),
            ("wink", "0\n"),
        ] {
            std::fs::write(hub.join(f), v).unwrap();
        }
        for f in glass_hub::sysfs::WRITABLE {
            std::fs::write(hub.join(f), "").unwrap();
        }
        let opts = Options {
            socket: d.join("glassd.sock"),
            mock: false,
            device: Some(d.join("no-touchpad")),
            device_name: TOUCHPAD_NAME.into(),
            invert_x: false,
            invert_y: false,
            tap_slop: None,
            swipe_min_travel: None,
            scroll_min_step: None,
            screen_off_input: ScreenOffInput::Touches,
            glasshub: Some(hub.clone()),
            gaze_params: None,
            camera: Source::Off,
            power: Source::Off,
            touchpad_sysfs: None,
            state_dir: None,
            iio: None,
            iio_dev: None,
            peers: PeerPolicy::open(),
            grab_touchpad: false,
            radio: false,
            wifi_iface: crate::radio::DEFAULT_WIFI_IFACE.into(),
            companion: false,
            companion_tcp_port: None,
            companion_bt: false,
            companion_rng_device: crate::companion::entropy::RANDOM_DEVICE.into(),
            power_supply: crate::companion::battery::DEFAULT_ROOT.into(),
            energy: false,
            power_paths: PowerPaths::default(),
        };
        let mut daemon = Daemon::new(&opts, Box::new(crate::display::LoggingDisplay)).unwrap();
        // The tests below start from detection off and turn it on themselves.
        set_detection(&mut daemon, false);
        (daemon, hub)
    }

    fn detection_on(d: &mut Daemon) {
        set_detection(d, true);
    }

    fn set_detection(d: &mut Daemon, on: bool) {
        let mut s = d.settings();
        s.on_head_detection_enabled = Some(on);
        d.hub.set_settings_source(s);
        d.settings_changed(&s);
    }

    /// The boot this exists for: the random pool is not seeded and no identity is stored.
    #[test]
    fn an_unseeded_random_pool_holds_up_the_link_not_the_daemon() {
        use crate::companion::entropy::testing::FakePool;
        let d = dir("unseeded");
        let state = d.join("state");
        let mut pool = FakePool::new(&d);
        let socket = d.join("glassd.sock");
        let opts = Options {
            socket: socket.clone(),
            mock: false,
            device: Some(d.join("no-touchpad")),
            device_name: TOUCHPAD_NAME.into(),
            invert_x: false,
            invert_y: false,
            tap_slop: None,
            swipe_min_travel: None,
            scroll_min_step: None,
            screen_off_input: ScreenOffInput::Touches,
            glasshub: None,
            gaze_params: None,
            camera: Source::Off,
            power: Source::Off,
            touchpad_sysfs: None,
            state_dir: Some(state.clone()),
            iio: None,
            iio_dev: None,
            peers: PeerPolicy::open(),
            grab_touchpad: false,
            radio: false,
            wifi_iface: crate::radio::DEFAULT_WIFI_IFACE.into(),
            companion: true,
            companion_tcp_port: Some(0),
            companion_bt: false,
            companion_rng_device: pool.path.clone(),
            power_supply: d.join("no-power-supply"),
            energy: false,
            power_paths: PowerPaths::default(),
        };
        let mut daemon = Daemon::new(&opts, Box::new(crate::display::LoggingDisplay)).unwrap();
        let identity = companion::state_file(&state);
        assert!(!identity.exists(), "a key from an unseeded pool");
        assert!(daemon.companion.as_ref().unwrap().listener_fds().is_empty());

        // A client is served meanwhile: the handshake runs on this same thread.
        let client = std::thread::spawn(move || {
            glass_api::Client::connect_with_timeout(&socket, "test", Duration::from_secs(10))
                .unwrap()
                .welcome()
                .capabilities
                .clone()
        });
        while !client.is_finished() {
            daemon.step().unwrap();
        }
        let caps = client.join().unwrap();
        assert!(caps.iter().any(|c| c == "companion-link"), "{caps:?}");
        assert!(caps.iter().any(|c| c == "notes"), "{caps:?}");
        assert!(!identity.exists());

        pool.seed();
        daemon.step().unwrap();
        assert!(identity.exists());
        assert_eq!(daemon.companion.as_ref().unwrap().listener_fds().len(), 1);
    }

    #[test]
    fn every_setting_round_trips_through_the_companion_mirror() {
        let mut s = settings_rules::defaults();
        s.head_wake_enabled = Some(true);
        s.head_wake_angle_deg = Some(25.0);
        s.on_head_detection_enabled = Some(false);
        s.audio_volume = Some(40);
        s.time_24h = Some(false);
        s.timezone_id = Some(5);
        s.power_saver = Some(false);
        s.deep_sleep_enabled = Some(true);
        s.deep_sleep_idle_ms = Some(90_000);
        s.wink_enabled = Some(true);
        s.screen_timeout_ms = Some(30_000);
        s.nudge_sleep_enabled = Some(true);
        s.locale = Some(Locale::It as i32);
        s.off_head_power_off_min = Some(60);
        let back = settings_from_companion(&companion_settings(&s));
        let mut merged = settings_rules::defaults();
        settings_rules::merge(&mut merged, &back);
        assert_eq!(merged, s);
        // Untouched fields stay untouched.
        let only = settings_from_companion(&omg_protocol::Settings {
            audio_volume: Some(10),
            ..Default::default()
        });
        assert_eq!(only.audio_volume, Some(10));
        assert_eq!(only.time_24h, None);
    }

    #[test]
    fn the_companion_names_the_same_time_zones_as_the_table() {
        let names: Vec<&str> = settings_rules::TIME_ZONES.iter().map(|z| z.0).collect();
        assert_eq!(names, omg_protocol::TIME_ZONE_NAMES);
    }

    #[test]
    fn locale_maps_round_trip_between_the_proto_and_the_companion() {
        for l in [Locale::It, Locale::En, Locale::Fr, Locale::Es] {
            let round = proto_locale(companion_locale(Some(l as i32)));
            assert_eq!(round, l);
        }
        // Unset / unknown fall back to the companion default (en).
        assert_eq!(companion_locale(None), omg_protocol::Locale::En);
        assert_eq!(
            companion_locale(Some(Locale::Unspecified as i32)),
            omg_protocol::Locale::En
        );
        assert_eq!(companion_locale(Some(12345)), omg_protocol::Locale::En);
    }

    #[test]
    fn don_doff_is_read_again_when_detection_is_armed() {
        let (mut d, hub) = fake_hub_daemon("rearm");
        assert!(d.wear.on_head());
        // The sensor reported on the head once (the start-up read of "1"): trusted.
        assert!(d.wear.sensor_trusted());
        // Changed while detection was off: no notification came.
        std::fs::write(hub.join("don_doff"), "0\n").unwrap();
        detection_on(&mut d);
        assert_eq!(
            std::fs::read_to_string(hub.join("don_doff_enable")).unwrap(),
            "1\n"
        );
        assert!(!d.wear.on_head(), "read again after don_doff_enable = 1");
    }

    #[test]
    fn opening_the_hub_goes_through_a_pause_and_a_resume() {
        let (_d, hub) = fake_hub_daemon("wake");
        assert_eq!(std::fs::read_to_string(hub.join("pause")).unwrap(), "0\n");
    }

    #[test]
    fn don_doff_changes_are_seen_without_a_notification() {
        // Start off the head: a 0 before any 1 is ignored, so the Glass stays on the head.
        let (mut d, hub) = fake_hub_daemon_with("recheck", "0\n");
        assert_eq!(d.don_doff_recheck_us, None, "detection off: no timer");
        detection_on(&mut d);
        let due = d.don_doff_recheck_us.expect("armed: timer set");
        // Put on, then taken off: no notification, only the timer sees them.
        std::fs::write(hub.join("don_doff"), "1\n").unwrap();
        d.timers(Now::at(due));
        assert!(d.wear.sensor_trusted());
        let due = d.don_doff_recheck_us.expect("the timer keeps running");
        std::fs::write(hub.join("don_doff"), "0\n").unwrap();
        d.timers(Now::at(due));
        assert!(!d.wear.on_head());
        // Detection off: the timer stops.
        let mut s = d.settings();
        s.on_head_detection_enabled = Some(false);
        d.hub.set_settings_source(s);
        d.settings_changed(&s);
        assert_eq!(d.don_doff_recheck_us, None);
    }

    #[test]
    fn a_sensor_stuck_at_0_does_not_take_the_glass_off_the_head() {
        // Model B glasshub 1.22: don_doff reads 0 from boot while the Glass is worn.
        let (mut d, hub) = fake_hub_daemon_with("stuck", "0\n");
        assert!(!d.wear.sensor_trusted());
        detection_on(&mut d);
        assert!(d.wear.on_head(), "a 0 before any 1 is not the sensor state");
        assert_eq!(d.wear.on_timeout(now().mono_us + 31_000_000), None);
        assert!(d.wear.on_head());
        // Once it reports 1, a later 0 counts.
        std::fs::write(hub.join("don_doff"), "1\n").unwrap();
        d.read_don_doff();
        assert!(d.wear.sensor_trusted());
        std::fs::write(hub.join("don_doff"), "0\n").unwrap();
        d.read_don_doff();
        assert!(!d.wear.on_head());
    }

    #[test]
    fn don_doff_is_read_again_when_the_hub_resumes() {
        let (mut d, hub) = fake_hub_daemon("resume");
        detection_on(&mut d);
        let u = d.wear.power_key(now().mono_us); // forced off: hub paused
        d.wear_update(u);
        assert_eq!(std::fs::read_to_string(hub.join("pause")).unwrap(), "1\n");
        std::fs::write(hub.join("don_doff"), "0\n").unwrap();
        // Power key again: timed on-head (detection on), hub resumed and read again; the
        // timed period still reports on the head, but the reading is kept for the sensor.
        let u = d.wear.power_key(now().mono_us);
        d.wear_update(u);
        assert_eq!(std::fs::read_to_string(hub.join("pause")).unwrap(), "0\n");
        let u = d.wear.on_timeout(now().mono_us + 31_000_000);
        assert_eq!(u.map(|u| u.state.wear), Some(Wear::OffHead as i32));
    }

    #[test]
    fn a_failed_glasshub_forces_on_head_and_drops_wink() {
        let (mut d, _hub) = fake_hub_daemon("drop");
        detection_on(&mut d);
        assert!(d.wear.detection());
        d.drop_glasshub("test");
        assert!(d.glasshub.is_none());
        assert!(!d.wear.detection());
        assert!(d.wear.on_head());
        assert_eq!(d.wear.state().source, WearSource::Forced as i32);
        // Turning detection on again has no effect without a hub.
        detection_on(&mut d);
        assert!(!d.wear.detection());
    }

    #[test]
    fn airplane_mode_forces_both_radios_off_but_keeps_the_wishes() {
        let mut s = settings_rules::defaults();
        s.wifi_enabled = Some(true);
        s.bluetooth_enabled = Some(true);
        s.airplane_mode = Some(false);
        assert_eq!(effective_radio(&s), (true, true));
        // Airplane on: both off, the wishes are untouched (so they restore on the way back).
        s.airplane_mode = Some(true);
        assert_eq!(effective_radio(&s), (false, false));
        assert_eq!(s.wifi_enabled, Some(true));
        assert_eq!(s.bluetooth_enabled, Some(true));
        // Airplane off again: back to the wishes.
        s.airplane_mode = Some(false);
        assert_eq!(effective_radio(&s), (true, true));
        // Defaults: Wi-Fi off.
        assert_eq!(effective_radio(&settings_rules::defaults()), (false, false));
    }

    #[test]
    fn bind_refuses_to_unlink_a_regular_file() {
        let d = dir("file");
        let p = d.join("glassd.sock");
        std::fs::write(&p, b"keep me").unwrap();
        assert!(bind_socket(&p).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"keep me");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn bind_replaces_a_stale_socket_and_refuses_a_live_one() {
        let d = dir("sock");
        let p = d.join("glassd.sock");
        drop(UnixListener::bind(&p).unwrap()); // stale: nobody listens
        let live = bind_socket(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660);
        assert_eq!(
            bind_socket(&p).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
        drop(live);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
