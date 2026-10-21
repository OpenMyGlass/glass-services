//! Touchpad gesture recognizer for Glass. Pure: evdev-like events in, glass-api events out.
//!
//! Behaviour follows `docs/specs/touchpad-gestures.md` §4 (stock recognizer) with the
//! delivery rules of `docs/api.md` ("Touchpad gestures"):
//!
//! - Frames are built from multitouch events. MT protocol A (`SYN_MT_REPORT` per contact,
//!   used by `sensor00fn11`) and protocol B (`ABS_MT_SLOT`) are both accepted; the protocol
//!   is detected from the stream. A frame with no contact means all fingers are up.
//! - `TAP` when every finger lifts without leaving the tap radius; `LONG_PRESS` when the hold
//!   time passes inside it (the daemon drives the timer through [`Recognizer::next_deadline`]
//!   and [`Recognizer::on_timeout`]); after it the touch produces nothing else.
//! - Leaving the tap radius starts a scroll whose axis is fixed by the move that left it.
//!   Horizontal scrolls send `Scroll` frames. On lift, a one-finger scroll is classified from
//!   its entry point (angle limits and release velocity); a two-finger scroll needs both lifts
//!   within the release window; three fingers never swipe.
//! - While screen-off decoding is on, contacts with the markers of the kernel's encoded
//!   screen-off frames (touchpad-gestures.md §5.2) produce [`Output::ScreenOff`] and are kept
//!   out of touch tracking; real contacts around them are recognized as usual.
//!
//! Positions are oriented first (forward and down positive, see [`Config`]), so all the logic
//! works in the Glass frame.

pub mod config;
pub mod event;
pub mod synth;

pub use config::{AxisRange, Config};
pub use event::RawEvent;

use event::{abs, ev, syn};
use glass_api::proto::{FingerCount, Gesture, GestureKind, Scroll, ServiceFrame, service_frame};

/// A gesture decoded from a screen-off encoded frame (touchpad-gestures.md §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenOffGesture {
    Tap,
    LongPress,
    SwipeForward,
    SwipeBackward,
}

/// One recognizer output.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Gesture(Gesture),
    Scroll(Scroll),
    FingerCount(FingerCount),
    /// An encoded screen-off frame; only produced while screen-off decoding is on. Not an
    /// interface event: the screen policy decides what it does.
    ScreenOff {
        gesture: ScreenOffGesture,
        timestamp_us: u64,
    },
}

impl Output {
    /// The interface frame for this output (`None` for [`Output::ScreenOff`]).
    pub fn to_frame(&self) -> Option<ServiceFrame> {
        let body = match self {
            Output::Gesture(g) => service_frame::Body::Gesture(*g),
            Output::Scroll(s) => service_frame::Body::Scroll(*s),
            Output::FingerCount(f) => service_frame::Body::FingerCount(*f),
            Output::ScreenOff { .. } => return None,
        };
        Some(ServiceFrame { body: Some(body) })
    }
}

/// One finger in a frame, raw device units.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Contact {
    id: i32,
    x: i32,
    y: i32,
    pressure: i32,
    major: i32,
    minor: i32,
}

#[derive(Debug, Clone, Copy, Default)]
struct Partial {
    id: Option<i32>,
    x: Option<i32>,
    y: Option<i32>,
    pressure: Option<i32>,
    major: Option<i32>,
    minor: Option<i32>,
    any: bool,
}

impl Partial {
    fn contact(&self, default_id: i32) -> Option<Contact> {
        Some(Contact {
            id: self.id.unwrap_or(default_id),
            x: self.x?,
            y: self.y?,
            pressure: self.pressure.unwrap_or(0),
            major: self.major.unwrap_or(0),
            minor: self.minor.unwrap_or(0),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Unknown,
    A,
    B,
}

const MAX_SLOTS: usize = 16;

/// Assembles evdev events into frames of contacts.
#[derive(Debug)]
struct FrameBuilder {
    protocol: Protocol,
    /// Protocol A: the contact being reported, and the contacts of this frame.
    pending: Partial,
    contacts: Vec<Contact>,
    /// A `SYN_MT_REPORT` was seen in the current frame.
    mt_report: bool,
    /// Protocol B (also filled while the protocol is unknown).
    slot: usize,
    slots: [Partial; MAX_SLOTS],
    dropping: bool,
}

enum FrameEvent {
    Frame(Vec<Contact>),
    Dropped,
}

impl FrameBuilder {
    fn new() -> Self {
        FrameBuilder {
            protocol: Protocol::Unknown,
            pending: Partial::default(),
            contacts: Vec::new(),
            mt_report: false,
            slot: 0,
            slots: [Partial::default(); MAX_SLOTS],
            dropping: false,
        }
    }

    fn push(&mut self, e: &RawEvent) -> Option<FrameEvent> {
        match (e.type_, e.code) {
            (ev::SYN, syn::DROPPED) => {
                self.dropping = true;
                self.mt_report = false;
                self.pending = Partial::default();
                self.contacts.clear();
                self.slots = [Partial::default(); MAX_SLOTS];
                None
            }
            (ev::SYN, syn::REPORT) if self.dropping => {
                self.dropping = false;
                Some(FrameEvent::Dropped)
            }
            _ if self.dropping => None,
            (ev::SYN, syn::MT_REPORT) => {
                self.protocol = Protocol::A;
                self.mt_report = true;
                let p = std::mem::take(&mut self.pending);
                if let Some(c) = p.contact(self.contacts.len() as i32) {
                    self.contacts.push(c);
                }
                None
            }
            (ev::SYN, syn::REPORT) => {
                if self.protocol == Protocol::Unknown && self.slots.iter().any(|s| s.any) {
                    self.protocol = Protocol::B;
                }
                if self.protocol == Protocol::A && !std::mem::take(&mut self.mt_report) {
                    // No contact data at all (a bare SYN_REPORT after an encoded frame, or a
                    // switch event): not a frame of contacts, in particular not an all-up.
                    self.pending = Partial::default();
                    return None;
                }
                Some(FrameEvent::Frame(match self.protocol {
                    Protocol::A => std::mem::take(&mut self.contacts),
                    Protocol::B => self
                        .slots
                        .iter()
                        .filter(|s| s.id.is_some_and(|id| id >= 0))
                        .filter_map(|s| s.contact(0))
                        .collect(),
                    Protocol::Unknown => Vec::new(),
                }))
            }
            (ev::ABS, abs::MT_SLOT) => {
                self.protocol = Protocol::B;
                self.slot = (e.value.max(0) as usize).min(MAX_SLOTS - 1);
                None
            }
            (ev::ABS, code) => {
                let set = |p: &mut Partial| {
                    let v = Some(e.value);
                    match code {
                        abs::MT_TRACKING_ID => p.id = v,
                        abs::MT_POSITION_X => p.x = v,
                        abs::MT_POSITION_Y => p.y = v,
                        abs::MT_PRESSURE => p.pressure = v,
                        abs::MT_TOUCH_MAJOR => p.major = v,
                        abs::MT_TOUCH_MINOR => p.minor = v,
                        _ => return,
                    }
                    p.any = true;
                };
                if self.protocol != Protocol::B {
                    set(&mut self.pending);
                }
                if self.protocol != Protocol::A {
                    let slot = &mut self.slots[self.slot];
                    if code == abs::MT_TRACKING_ID && e.value < 0 {
                        // Slot released: forget its values.
                        *slot = Partial {
                            any: true,
                            ..Partial::default()
                        };
                    } else {
                        set(slot);
                    }
                }
                None
            }
            _ => None,
        }
    }
}

/// Oriented contact position, device units, forward and down positive.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    id: i32,
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Forward,
    Backward,
    Down,
    Up,
}

impl Dir {
    fn horizontal(self) -> bool {
        matches!(self, Dir::Forward | Dir::Backward)
    }

    fn kind(self) -> GestureKind {
        match self {
            Dir::Forward => GestureKind::SwipeForward,
            Dir::Backward => GestureKind::SwipeBackward,
            Dir::Down => GestureKind::SwipeDown,
            Dir::Up => GestureKind::SwipeUp,
        }
    }

    /// Dominant axis of a displacement; `None` if neither dominates.
    fn of(dx: f32, dy: f32) -> Option<Dir> {
        if dx.abs() > dy.abs() {
            Some(if dx > 0.0 {
                Dir::Forward
            } else {
                Dir::Backward
            })
        } else if dy.abs() > dx.abs() {
            Some(if dy > 0.0 { Dir::Down } else { Dir::Up })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// No finger on the pad.
    Idle,
    /// Fingers down, still inside the tap radius of the reference frame.
    Tap,
    /// Left the tap radius; `dir` fixed at entry (`None` if no axis dominated).
    Scroll { dir: Option<Dir> },
    /// The touch produces no more gestures (long press fired, four fingers, three fingers
    /// moved, events dropped); finger counts are still reported until all fingers are up.
    Dead,
}

/// The recognizer. Feed it events with [`push`](Self::push) or [`feed`](Self::feed).
#[derive(Debug)]
pub struct Recognizer {
    cfg: Config,
    frames: FrameBuilder,
    screen_off_decoding: bool,
    phase: Phase,
    /// Previous frame, oriented.
    prev: Vec<Point>,
    /// Frame at the last finger down or up (tap radius reference).
    reference: Vec<Point>,
    /// Mean displacement since touch down, accumulated over matched contacts.
    cum: (f32, f32),
    /// `cum` when the scroll started.
    entry: (f32, f32),
    /// Smoothed velocity, device units per ms.
    vel: (f32, f32),
    last_t: u64,
    /// Largest finger count of the touch.
    max_fingers: u32,
    /// Largest finger count since the scroll started.
    scroll_fingers: u32,
    long_press_at: Option<u64>,
    /// The last `Scroll` sent in this touch.
    last_scroll: Option<Scroll>,
    /// Two-finger scroll: time of the first lift, and whether its direction matched.
    first_lift: Option<(u64, bool)>,
    /// The previous frame carried an encoded screen-off contact.
    encoded_seen: bool,
}

impl Recognizer {
    pub fn new(cfg: Config) -> Self {
        Recognizer {
            cfg,
            frames: FrameBuilder::new(),
            screen_off_decoding: false,
            phase: Phase::Idle,
            prev: Vec::new(),
            reference: Vec::new(),
            cum: (0.0, 0.0),
            entry: (0.0, 0.0),
            vel: (0.0, 0.0),
            last_t: 0,
            max_fingers: 0,
            scroll_fingers: 0,
            long_press_at: None,
            last_scroll: None,
            first_lift: None,
            encoded_seen: false,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Turns decoding of encoded screen-off frames on (screen off) or off.
    pub fn set_screen_off_decoding(&mut self, on: bool) {
        self.screen_off_decoding = on;
    }

    /// Whether fingers are on the pad.
    pub fn touching(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// When [`on_timeout`](Self::on_timeout) must be called next (`CLOCK_MONOTONIC` µs).
    pub fn next_deadline(&self) -> Option<u64> {
        self.long_press_at
    }

    /// Fires the long press if its time has come.
    pub fn on_timeout(&mut self, now_us: u64, out: &mut Vec<Output>) {
        if let Some(at) = self.long_press_at
            && now_us >= at
        {
            self.long_press_at = None;
            if self.phase == Phase::Tap {
                self.cancel_scroll(out);
                out.push(Output::Gesture(Gesture {
                    kind: GestureKind::LongPress as i32,
                    fingers: self.max_fingers.min(3),
                    velocity: 0.0,
                    timestamp_us: at,
                }));
                self.phase = Phase::Dead;
            }
        }
    }

    /// Ends the current touch now, without a gesture (device lost, for example).
    pub fn cancel(&mut self, now_us: u64, out: &mut Vec<Output>) {
        self.cancel_touch(now_us, out);
        self.frames = FrameBuilder::new();
        self.encoded_seen = false;
    }

    /// Feeds a batch of events, returning the outputs.
    pub fn feed(&mut self, events: &[RawEvent]) -> Vec<Output> {
        let mut out = Vec::new();
        for e in events {
            self.push(e, &mut out);
        }
        out
    }

    /// Feeds one event.
    pub fn push(&mut self, e: &RawEvent, out: &mut Vec<Output>) {
        match self.frames.push(e) {
            None => {}
            Some(FrameEvent::Dropped) => self.dropped(out),
            Some(FrameEvent::Frame(contacts)) => self.frame(e.time_us, contacts, out),
        }
    }

    fn orient(&self, c: &Contact) -> Point {
        let (x, y) = (i64::from(c.x), i64::from(c.y));
        let x = if self.cfg.forward_is_positive_x {
            x - i64::from(self.cfg.x.min)
        } else {
            i64::from(self.cfg.x.max) - x
        };
        let y = if self.cfg.down_is_positive_y {
            y - i64::from(self.cfg.y.min)
        } else {
            i64::from(self.cfg.y.max) - y
        };
        Point {
            id: c.id,
            x: x as f32,
            y: y as f32,
        }
    }

    /// Whether a contact carries the markers of an encoded screen-off frame
    /// (touchpad-gestures.md §5.2: pressure ≤ 0.01, touch major and minor ≥ 14.8).
    fn is_encoded(&self, c: &Contact) -> bool {
        let p = c.pressure as f32 / self.cfg.pressure_max.max(1) as f32;
        p <= self.cfg.encoded_max_pressure
            && c.major as f32 >= self.cfg.encoded_min_size
            && c.minor as f32 >= self.cfg.encoded_min_size
    }

    /// The gesture encoded at (100 + dx, 100 + dy), if any.
    fn decode_encoded(&self, c: &Contact) -> Option<ScreenOffGesture> {
        let dx = i64::from(c.x) - 100;
        let dy = i64::from(c.y) - 100;
        let forward = (dx > 0) == self.cfg.forward_is_positive_x;
        match (dx, dy) {
            (0, 0) | (-1, -1) => Some(ScreenOffGesture::Tap),
            (1, 1) => Some(ScreenOffGesture::LongPress),
            _ if dx.abs() >= i64::from(self.cfg.encoded_min_flick) && dx.abs() > dy.abs() => {
                Some(if forward {
                    ScreenOffGesture::SwipeForward
                } else {
                    ScreenOffGesture::SwipeBackward
                })
            }
            _ => None,
        }
    }

    fn frame(&mut self, t: u64, mut contacts: Vec<Contact>, out: &mut Vec<Output>) {
        if self.screen_off_decoding {
            // Encoded contacts are interleaved with real ones: report them, keep them out of
            // the touch, and do not take a frame that only carried one for an all-up.
            let (encoded, real): (Vec<Contact>, Vec<Contact>) =
                contacts.into_iter().partition(|c| self.is_encoded(c));
            let had = !encoded.is_empty();
            if !self.encoded_seen
                && let Some(gesture) = encoded.first().and_then(|c| self.decode_encoded(c))
            {
                out.push(Output::ScreenOff {
                    gesture,
                    timestamp_us: t,
                });
            }
            let just_after = std::mem::replace(&mut self.encoded_seen, had);
            if had && real.is_empty() {
                return;
            }
            // The driver may close a real-finger frame early around an encoded one, so the
            // frame right after it can be partial: do not read fewer fingers from it. A real
            // all-up (an empty frame with SYN_MT_REPORT) still ends the touch.
            if just_after
                && !had
                && self.phase != Phase::Idle
                && !real.is_empty()
                && real.len() < self.prev.len()
            {
                return;
            }
            contacts = real;
        } else {
            self.encoded_seen = false;
        }
        if self.phase == Phase::Idle && contacts.is_empty() {
            return;
        }
        // A frame later than the hold time fires the long press first.
        self.on_timeout(t, out);

        let points: Vec<Point> = contacts.iter().map(|c| self.orient(c)).collect();
        let n = points.len() as u32;

        if self.phase == Phase::Idle {
            self.start_touch(t, points, out);
            return;
        }

        let prev_n = self.prev.len() as u32;
        self.track_motion(t, &points);

        if n == 0 {
            self.end_touch(t, prev_n, out);
            return;
        }

        if n != prev_n {
            out.push(Output::FingerCount(FingerCount {
                previous: prev_n,
                current: n,
                timestamp_us: t,
            }));
            self.max_fingers = self.max_fingers.max(n);
            self.fingers_changed(t, prev_n, n, &points, out);
        }

        match self.phase {
            Phase::Tap => {
                let factor = if n >= 3 {
                    self.cfg.three_finger_slop_factor
                } else {
                    1.0
                };
                if self.beyond_slop(&points, self.cfg.touch_slop * factor) {
                    self.long_press_at = None;
                    if n >= 3 {
                        self.phase = Phase::Dead;
                    } else {
                        let (dx, dy) = mean_delta(&self.reference, &points);
                        self.entry = self.cum;
                        self.scroll_fingers = n;
                        self.phase = Phase::Scroll {
                            dir: Dir::of(dx, dy),
                        };
                        self.maybe_scroll(t, n, out);
                    }
                }
            }
            Phase::Scroll { .. } => self.maybe_scroll(t, n, out),
            Phase::Dead | Phase::Idle => {}
        }
        self.prev = points;
        self.last_t = t;
    }

    fn start_touch(&mut self, t: u64, points: Vec<Point>, out: &mut Vec<Output>) {
        let n = points.len() as u32;
        out.push(Output::FingerCount(FingerCount {
            previous: 0,
            current: n,
            timestamp_us: t,
        }));
        self.cum = (0.0, 0.0);
        self.entry = (0.0, 0.0);
        self.vel = (0.0, 0.0);
        self.last_t = t;
        self.max_fingers = n;
        self.scroll_fingers = 0;
        self.last_scroll = None;
        self.first_lift = None;
        self.reference = points.clone();
        self.prev = points;
        if n >= 4 {
            self.phase = Phase::Dead;
            self.long_press_at = None;
        } else {
            self.phase = Phase::Tap;
            self.long_press_at = Some(t + self.cfg.long_press_ms * 1000);
        }
    }

    fn fingers_changed(
        &mut self,
        t: u64,
        prev_n: u32,
        n: u32,
        points: &[Point],
        out: &mut Vec<Output>,
    ) {
        if n >= 4 {
            self.long_press_at = None;
            self.cancel_scroll(out);
            self.phase = Phase::Dead;
            return;
        }
        self.reference = points.to_vec();
        match self.phase {
            Phase::Tap if n > prev_n => {
                self.long_press_at = Some(t + self.cfg.long_press_ms * 1000);
            }
            Phase::Scroll { dir } => {
                self.scroll_fingers = self.scroll_fingers.max(n);
                if prev_n >= 2 && n < prev_n && self.first_lift.is_none() {
                    let (dx, dy) = (self.cum.0 - self.entry.0, self.cum.1 - self.entry.1);
                    self.first_lift = Some((t, dir.is_some() && Dir::of(dx, dy) == dir));
                }
            }
            _ => {}
        }
    }

    /// Updates the accumulated displacement and the smoothed velocity from the contacts
    /// present in both the previous and this frame (touchpad-gestures.md §4.2).
    fn track_motion(&mut self, t: u64, points: &[Point]) {
        let (dx, dy) = mean_delta(&self.prev, points);
        self.cum.0 += dx;
        self.cum.1 += dy;
        if t > self.last_t && !points.is_empty() {
            let dt = (t - self.last_t) as f32 / 1000.0;
            let alpha = dt / (self.cfg.velocity_rc_ms + dt);
            let smooth = |prev: f32, raw: f32| {
                if prev.abs() < 0.001 {
                    raw
                } else {
                    (1.0 - alpha) * prev + alpha * raw
                }
            };
            self.vel = (smooth(self.vel.0, dx / dt), smooth(self.vel.1, dy / dt));
        }
    }

    /// Any contact farther than `slop` from the same contact in the reference frame
    /// (integer-truncated deltas, squared distance, as in touchpad-gestures.md §4.2).
    fn beyond_slop(&self, points: &[Point], slop: f32) -> bool {
        points.iter().any(|p| {
            self.reference.iter().any(|q| {
                if q.id != p.id {
                    return false;
                }
                let dx = (p.x - q.x).trunc();
                let dy = (p.y - q.y).trunc();
                dx * dx + dy * dy > slop * slop
            })
        })
    }

    fn width(&self) -> f32 {
        self.cfg.x.span()
    }

    fn height(&self) -> f32 {
        self.cfg.y.span()
    }

    fn maybe_scroll(&mut self, t: u64, n: u32, out: &mut Vec<Output>) {
        let Phase::Scroll { dir: Some(dir) } = self.phase else {
            return;
        };
        if !dir.horizontal() {
            return;
        }
        let displacement = self.cum.0 / self.width();
        let last = self.last_scroll.map_or(0.0, |s| s.displacement);
        let delta = displacement - last;
        if delta == 0.0 {
            return;
        }
        let s = Scroll {
            displacement,
            delta,
            velocity: self.vel.0 * 1000.0 / self.width(),
            fingers: n,
            ended: false,
            cancelled: false,
            timestamp_us: t,
        };
        self.last_scroll = Some(s);
        out.push(Output::Scroll(s));
    }

    /// Sends the closing `Scroll{ended, cancelled}` if a scroll is open.
    fn cancel_scroll(&mut self, out: &mut Vec<Output>) {
        if let Some(last) = self.last_scroll.take() {
            out.push(Output::Scroll(Scroll {
                delta: 0.0,
                velocity: 0.0,
                ended: true,
                cancelled: true,
                ..last
            }));
        }
    }

    /// Events were lost (`SYN_DROPPED`): the touch produces nothing more until all fingers are
    /// up.
    fn dropped(&mut self, out: &mut Vec<Output>) {
        if self.phase != Phase::Idle {
            self.cancel_scroll(out);
            self.long_press_at = None;
            self.phase = Phase::Dead;
        }
    }

    fn cancel_touch(&mut self, t: u64, out: &mut Vec<Output>) {
        if self.phase == Phase::Idle {
            self.reset();
            return;
        }
        self.cancel_scroll(out);
        let prev = self.prev.len() as u32;
        if prev > 0 {
            out.push(Output::FingerCount(FingerCount {
                previous: prev,
                current: 0,
                timestamp_us: t,
            }));
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.phase = Phase::Idle;
        self.prev.clear();
        self.reference.clear();
        self.long_press_at = None;
        self.last_scroll = None;
        self.first_lift = None;
    }

    fn end_touch(&mut self, t: u64, prev_n: u32, out: &mut Vec<Output>) {
        match self.phase {
            Phase::Tap => out.push(Output::Gesture(Gesture {
                kind: GestureKind::Tap as i32,
                fingers: self.max_fingers.min(3),
                velocity: 0.0,
                timestamp_us: t,
            })),
            Phase::Scroll { dir } => {
                if let Some(last) = self.last_scroll.take() {
                    out.push(Output::Scroll(Scroll {
                        delta: 0.0,
                        velocity: self.vel.0 * 1000.0 / self.width(),
                        ended: true,
                        cancelled: false,
                        timestamp_us: t,
                        ..last
                    }));
                }
                if let Some(d) = self.classify(t, dir) {
                    let velocity = if d.horizontal() {
                        self.vel.0 * 1000.0 / self.width()
                    } else {
                        self.vel.1 * 1000.0 / self.height()
                    };
                    out.push(Output::Gesture(Gesture {
                        kind: d.kind() as i32,
                        fingers: self.scroll_fingers,
                        velocity,
                        timestamp_us: t,
                    }));
                }
            }
            Phase::Dead | Phase::Idle => {}
        }
        if prev_n > 0 {
            out.push(Output::FingerCount(FingerCount {
                previous: prev_n,
                current: 0,
                timestamp_us: t,
            }));
        }
        self.reset();
    }

    /// The swipe at the end of a scroll (touchpad-gestures.md §4.3), if any.
    fn classify(&self, t: u64, dir: Option<Dir>) -> Option<Dir> {
        let (dx, dy) = (self.cum.0 - self.entry.0, self.cum.1 - self.entry.1);
        match self.scroll_fingers {
            1 => {}
            2 => {
                // The last lift; if both lifted together it is also the first.
                let (first_t, matched) = self
                    .first_lift
                    .unwrap_or((t, dir.is_some() && Dir::of(dx, dy) == dir));
                let window = self.cfg.two_finger_release_window_ms * 1000;
                return dir.filter(|_| matched && t.saturating_sub(first_t) <= window);
            }
            _ => return None,
        }
        let started_horizontal = dir.is_some_and(Dir::horizontal);
        let vertical = || {
            if started_horizontal {
                // A scroll that started horizontal never becomes a vertical swipe.
                None
            } else if dy > 0.0 {
                Some(Dir::Down)
            } else if dy < 0.0 {
                Some(Dir::Up)
            } else {
                None
            }
        };
        if dx == 0.0 {
            return vertical();
        }
        let angle = (dy.abs() / dx.abs()).atan().to_degrees();
        let limit = if dx > 0.0 {
            self.cfg.forward_max_angle_deg
        } else {
            self.cfg.backward_max_angle_deg
        };
        if angle > limit {
            return vertical();
        }
        let vx = self.vel.0;
        if vx > self.cfg.forward_min_velocity {
            Some(Dir::Forward)
        } else if vx < -self.cfg.backward_min_velocity {
            Some(Dir::Backward)
        } else {
            None
        }
    }
}

/// Mean displacement of the contacts present in both frames (matched by id).
fn mean_delta(from: &[Point], to: &[Point]) -> (f32, f32) {
    let mut sum = (0.0, 0.0);
    let mut k = 0;
    for p in to {
        if let Some(q) = from.iter().find(|q| q.id == p.id) {
            sum.0 += p.x - q.x;
            sum.1 += p.y - q.y;
            k += 1;
        }
    }
    if k == 0 {
        (0.0, 0.0)
    } else {
        (sum.0 / k as f32, sum.1 / k as f32)
    }
}
