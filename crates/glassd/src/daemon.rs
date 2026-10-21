//! The `poll` loop: listening socket, clients, the touchpad, and (with `--mock`) standard input.
//! Single-threaded; blocks in `poll` until a descriptor is ready or the next deadline
//! (long press, screen timer, a full client queue, a mock `wait`).

use crate::display::Display;
use crate::mockcmd::{self, Command};
use crate::queue::OutQueue;
use crate::screen::{Change, Now, ScreenPolicy};
use crate::touch::{ScreenOffInput, Step, TouchGate};
use crate::{info, warn};
use glass_api::hub::settings_rules;
use glass_api::proto::{
    Button, ButtonAction, ButtonId, ClientFrame, HeadGesture, HeadGestureKind, ScreenState,
    ServiceFrame, Settings, Wear, WearSource, WearState, Wink, service_frame,
};
use glass_api::{ClientId, Effect, FrameDecoder, Hub};
use glass_evdev::Device;
use glass_gesture::synth::{Direction, Synth};
use glass_gesture::{AxisRange, Config, Output, RawEvent, Recognizer};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Name of the touchpad input device (docs/specs/kernel-interfaces.md §1.2).
pub const TOUCHPAD_NAME: &str = "sensor00fn11";

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
        let settings = settings_rules::defaults();
        let policy = ScreenPolicy::new(t, settings.screen_timeout_ms.unwrap_or(15_000));
        let mut caps: Vec<&str> = vec!["screen"];
        if opts.mock {
            caps = vec![
                "mock",
                "touchpad",
                "screen",
                "button-camera",
                "button-power",
                "hinge",
                "head",
                "wear",
                "wink",
            ];
        } else if device.is_some() {
            caps.insert(0, "touchpad");
        }
        let mut hub = Hub::new(env!("CARGO_PKG_VERSION"), &caps);
        hub.set_screen_source(policy.state());
        hub.set_settings_source(settings);
        let mock = opts.mock.then(|| {
            let wear = WearState {
                wear: Wear::OnHead as i32,
                snapshot: false,
                timestamp_us: t.mono_us,
                source: WearSource::Forced as i32,
            };
            hub.set_wear_source(wear);
            MockInput {
                buf: Vec::new(),
                commands: VecDeque::new(),
                resume_at: None,
                eof: false,
            }
        });
        info!(
            "listening on {} ({})",
            opts.socket.display(),
            caps.join(", ")
        );
        Ok(Daemon {
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
        })
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
                Effect::SettingsChanged(s) => {
                    if let Some(ms) = s.screen_timeout_ms {
                        self.policy.set_timeout(ms);
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
            Command::Camera { long } => {
                let screen_off = self.policy.is_off();
                if let Some(ch) = self.policy.wake(t, "button", false) {
                    self.apply_change(ch);
                }
                let actions: &[ButtonAction] = if long {
                    &[ButtonAction::LongPress, ButtonAction::Release]
                } else {
                    &[ButtonAction::Press]
                };
                for &a in actions {
                    self.publish(service_frame::Body::Button(Button {
                        button: ButtonId::Camera as i32,
                        action: a as i32,
                        screen_off,
                        timestamp_us: t.mono_us,
                    }));
                }
            }
            Command::Power { long } => {
                let screen_off = self.policy.is_off();
                let actions: &[ButtonAction] = if long {
                    &[ButtonAction::LongPress, ButtonAction::Release]
                } else {
                    &[ButtonAction::Press]
                };
                for &a in actions {
                    self.publish(service_frame::Body::Button(Button {
                        button: ButtonId::Power as i32,
                        action: a as i32,
                        screen_off,
                        timestamp_us: t.mono_us,
                    }));
                }
                if !long {
                    let on = !self.policy.on_head();
                    self.mock_wear(on, WearSource::PowerKey);
                }
            }
            Command::Hinge { open } => self.mock_wear(open, WearSource::Hinge),
            Command::Wear { on } => self.mock_wear(on, WearSource::Sensor),
            Command::LookUp | Command::LookDown | Command::RelativeLookUp | Command::Nudge => {
                let s = self.settings();
                let angle = s.head_wake_angle_deg.unwrap_or(30.0);
                let (kind, pitch) = match c {
                    Command::LookUp => (HeadGestureKind::LookUp, angle + 5.0),
                    Command::LookDown => (HeadGestureKind::LookDown, angle - 10.0),
                    Command::RelativeLookUp => (HeadGestureKind::RelativeLookUp, angle),
                    _ => (HeadGestureKind::Nudge, 0.0),
                };
                self.publish(service_frame::Body::HeadGesture(HeadGesture {
                    kind: kind as i32,
                    pitch_deg: pitch,
                    timestamp_us: t.mono_us,
                }));
                let change = match kind {
                    HeadGestureKind::LookUp
                        if self.policy.is_off()
                            && self.policy.on_head()
                            && s.head_wake_enabled.unwrap_or(true) =>
                    {
                        self.policy.wake(t, "look-up", true)
                    }
                    HeadGestureKind::Nudge
                        if s.nudge_sleep_enabled.unwrap_or(false)
                            && !self.policy.is_off()
                            && !self.gate.touching() =>
                    {
                        self.policy.sleep(t, "nudge")
                    }
                    _ => None,
                };
                if let Some(ch) = change {
                    self.apply_change(ch);
                }
            }
            Command::Wink => {
                if self.settings().wink_enabled.unwrap_or(false) {
                    self.publish(service_frame::Body::Wink(Wink {
                        timestamp_us: t.mono_us,
                    }));
                } else {
                    info!("mock: wink ignored (wink_enabled is false)");
                }
            }
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

    fn mock_wear(&mut self, on: bool, source: WearSource) {
        let t = now();
        let wear = WearState {
            wear: if on { Wear::OnHead } else { Wear::OffHead } as i32,
            snapshot: false,
            timestamp_us: t.mono_us,
            source: source as i32,
        };
        self.publish(service_frame::Body::WearState(wear));
        if let Some(ch) = self.policy.set_on_head(t, on) {
            self.apply_change(ch);
        }
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
