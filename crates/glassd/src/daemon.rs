//! The `poll` loop: listening socket, clients, the touchpad, and (with `--mock`) standard input.
//! Single-threaded; blocks in `poll` until a descriptor is ready or the next deadline
//! (long press, screen timer, a full client queue, a mock `wait`).

use crate::buttons::{ButtonTracker, LONG_PRESS_MS};
use crate::display::{Display, TouchpadSleep};
use crate::mockcmd::{self, Command};
use crate::queue::OutQueue;
use crate::screen::{Change, Now, ScreenPolicy};
use crate::settings_store::SettingsStore;
use crate::touch::{ScreenOffInput, Step, TouchGate};
use crate::{info, warn};
use glass_api::hub::settings_rules;
use glass_api::proto::{
    Button, ButtonAction, ButtonId, ClientFrame, HeadGesture, HeadGestureKind, ScreenState,
    ServiceFrame, Settings, Wear, WearSource, Wink, service_frame,
};
use glass_api::{ClientId, Effect, FrameDecoder, Hub};
use glass_evdev::Device;
use glass_gesture::synth::{Direction, Synth};
use glass_gesture::{AxisRange, Config, Output, RawEvent, Recognizer};
use glass_head::{Context as HeadContext, HeadService, Imu, Sample};
use glass_hub::{GlassHub, Thresholds, Update, WearMachine};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

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
    wear: WearMachine,
    camera: ButtonTracker,
    power: ButtonTracker,
    camera_dev: Option<Device>,
    power_dev: Option<Device>,
    touch_sleep: TouchpadSleep,
    store: Option<SettingsStore>,
    imu: Option<Imu>,
    head: HeadService,
    /// `HeadRequest` wishes per client: (relative look-up, nudge).
    head_requests: BTreeMap<ClientId, (bool, bool)>,
    samples: Vec<Sample>,
}

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

/// What was last written to the glasshub, so each value is written only on a change.
#[derive(Debug, Default)]
struct HubWritten {
    detection: Option<bool>,
    wink: Option<bool>,
    pause: Option<bool>,
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
            info!("no glasshub at {}", root.display());
            return None;
        }
        Err(e) => {
            warn!("cannot open the glasshub at {}: {e}", root.display());
            return None;
        }
    };
    info!(
        "glasshub {} (firmware {})",
        root.display(),
        hub.version().unwrap_or_else(|e| format!("unreadable: {e}"))
    );
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

fn open_imu(opts: &Options) -> Option<Imu> {
    let root = opts.iio.as_ref()?;
    let dir = match glass_head::iio::find(root, glass_head::iio::IMU_NAME) {
        Ok(Some(d)) => d,
        Ok(None) => {
            info!("no IIO device named {:?}", glass_head::iio::IMU_NAME);
            return None;
        }
        Err(e) => {
            info!("cannot list {}: {e}", root.display());
            return None;
        }
    };
    match Imu::open(&dir, opts.iio_dev.clone()) {
        Ok(imu) => {
            info!("IMU {} ({:?})", dir.display(), imu.calibration());
            Some(imu)
        }
        Err(e) => {
            warn!("cannot open the IMU at {}: {e}", dir.display());
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
fn peer_cred(fd: RawFd) -> String {
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
    if r == 0 {
        format!("uid {} pid {}", c.uid, c.pid)
    } else {
        "unknown peer".into()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_cred(_fd: RawFd) -> String {
    "peer".into()
}

impl Daemon {
    pub fn new(opts: &Options, display: Box<dyn Display>) -> io::Result<Daemon> {
        let listener = bind_socket(&opts.socket)?;
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
        let policy = ScreenPolicy::new(t, settings.screen_timeout_ms.unwrap_or(15_000));
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
        let caps: Vec<&str> = if opts.mock {
            vec![
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
            ]
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
            c.extend(["dictation", "companion"]);
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
            wear,
            camera: ButtonTracker::new(ButtonId::Camera),
            power: ButtonTracker::new(ButtonId::Power),
            camera_dev,
            power_dev,
            touch_sleep: TouchpadSleep::new(touchpad_sysfs),
            store,
            imu,
            head: HeadService::default(),
            head_requests: BTreeMap::new(),
            samples: Vec::new(),
        };
        // A previous run may have left the touchpad asleep.
        if d.mock.is_none() {
            d.touch_sleep.set(false);
        }
        d.sync_glasshub();
        d.read_don_doff();
        d.update_head();
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
            self.camera.next_deadline(),
            self.power.next_deadline(),
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
            Client(ClientId),
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
        for (pfd, src) in fds.iter().zip(srcs) {
            if pfd.revents == 0 {
                continue;
            }
            match src {
                Src::Listener => self.accept(),
                Src::Touchpad => self.read_touchpad(),
                Src::Stdin => self.read_stdin(),
                Src::Key(id) => self.read_key(id),
                Src::DonDoff => self.read_don_doff(),
                Src::WinkAttr => self.read_wink(),
                Src::Imu => self.read_imu(),
                Src::Client(id) => {
                    if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                        self.read_client(id);
                    }
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
        for id in [ButtonId::Camera, ButtonId::Power] {
            if let Some(b) = self.tracker(id).on_timeout(t.mono_us) {
                self.button_frames(id, vec![b]);
            }
        }
        if let Some(m) = &mut self.mock
            && m.resume_at.is_some_and(|r| t.mono_us >= r)
        {
            m.resume_at = None;
        }
        self.run_mock_commands();
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let id = self.hub.add_client();
                    info!("client {id} connected ({})", peer_cred(stream.as_raw_fd()));
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
            }
        }
    }

    fn publish(&mut self, body: service_frame::Body) {
        let fx = self.hub.publish(ServiceFrame { body: Some(body) });
        self.effects(fx);
    }

    fn apply_change(&mut self, c: Change) {
        info!("screen {:?} ({})", c.screen, c.reason);
        self.display.set(c.screen);
        let state: ScreenState = c.to_state();
        self.publish(service_frame::Body::ScreenState(state));
    }

    fn touch_outputs(&mut self, outs: &[Output], t: Now) {
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
        let keys: Vec<(i32, u64)> = self
            .events
            .iter()
            .filter(|e| e.type_ == EV_KEY && e.code == code)
            .map(|e| (e.value, e.time_us))
            .collect();
        for (value, at) in keys {
            self.on_key(id, value, at);
        }
    }

    /// One key event of a button. The camera button wakes the screen on key down (not off
    /// the head); the frames are published; a short power press toggles a manual off-head.
    fn on_key(&mut self, id: ButtonId, value: i32, at_us: u64) {
        let screen_off = self.policy.is_off();
        let (down, frames) = self.tracker(id).key(value, at_us, screen_off);
        if down
            && id == ButtonId::Camera
            && let Some(ch) = self.policy.wake(now(), "button", false)
        {
            self.apply_change(ch);
        }
        self.button_frames(id, frames);
    }

    fn button_frames(&mut self, id: ButtonId, frames: Vec<Button>) {
        for b in frames {
            info!("button {id:?} {:?}", ButtonAction::try_from(b.action));
            let press = b.action == ButtonAction::Press as i32;
            self.publish(service_frame::Body::Button(b));
            if id == ButtonId::Power && press {
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
            if u.wear_changed {
                let t = now();
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
            Ok(_) => self.wink(now().mono_us),
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
        if let Some(st) = &self.store
            && let Err(e) = st.save(s)
        {
            warn!("cannot store the settings in {}: {e}", st.path().display());
        }
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
        info!("head {kind:?} at {:.1} deg", g.pitch_deg);
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

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("glassd-bind-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_hub_daemon(name: &str) -> (Daemon, PathBuf) {
        let d = dir(name);
        let hub = d.join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        for (f, v) in [("version", "1.14\n"), ("don_doff", "1\n"), ("wink", "0\n")] {
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
            screen_off_input: ScreenOffInput::Touches,
            glasshub: Some(hub.clone()),
            gaze_params: None,
            camera: Source::Off,
            power: Source::Off,
            touchpad_sysfs: None,
            state_dir: None,
            iio: None,
            iio_dev: None,
        };
        let daemon = Daemon::new(&opts, Box::new(crate::display::LoggingDisplay)).unwrap();
        (daemon, hub)
    }

    fn detection_on(d: &mut Daemon) {
        let mut s = d.settings();
        s.on_head_detection_enabled = Some(true);
        d.hub.set_settings_source(s);
        d.settings_changed(&s);
    }

    #[test]
    fn don_doff_is_read_again_when_detection_is_armed() {
        let (mut d, hub) = fake_hub_daemon("rearm");
        assert!(d.wear.on_head());
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
