//! Synthetic touchpad event streams in the `sensor00fn11` format (MT protocol A), for tests
//! and for `glassd --mock`.
//!
//! Positions are given in device units relative to the pad centre, in the Glass frame:
//! `x` positive forward, `y` positive down. The generator applies the orientation flags of
//! the [`Config`], so the recognizer sees the same raw stream a real pad would send.

use crate::config::Config;
use crate::event::{RawEvent, abs, ev, syn};

/// Direction of a synthetic stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
    Down,
    Up,
}

impl Direction {
    fn unit(self) -> (f32, f32) {
        match self {
            Direction::Forward => (1.0, 0.0),
            Direction::Backward => (-1.0, 0.0),
            Direction::Down => (0.0, 1.0),
            Direction::Up => (0.0, -1.0),
        }
    }
}

/// Builds an event stream frame by frame. Time advances only with [`advance_ms`](Self::advance_ms).
#[derive(Debug, Clone)]
pub struct Synth {
    cfg: Config,
    t_us: u64,
    events: Vec<RawEvent>,
    /// Report interval used by the high-level helpers, ms.
    pub frame_ms: u64,
}

/// Horizontal spacing of the fingers of a multi-finger touch, device units.
const FINGER_SPACING: f32 = 60.0;

impl Synth {
    pub fn new(cfg: &Config, t0_us: u64) -> Self {
        Synth {
            cfg: cfg.clone(),
            t_us: t0_us,
            events: Vec::new(),
            frame_ms: 10,
        }
    }

    /// Current time of the stream, µs.
    pub fn time_us(&self) -> u64 {
        self.t_us
    }

    pub fn advance_ms(&mut self, ms: u64) {
        self.t_us += ms * 1000;
    }

    fn push(&mut self, type_: u16, code: u16, value: i32) {
        self.events.push(RawEvent {
            time_us: self.t_us,
            type_,
            code,
            value,
        });
    }

    fn raw(&self, x: f32, y: f32) -> (i32, i32) {
        let cx = (self.cfg.x.min as f32 + self.cfg.x.max as f32) / 2.0;
        let cy = (self.cfg.y.min as f32 + self.cfg.y.max as f32) / 2.0;
        let rx = if self.cfg.forward_is_positive_x {
            cx + x
        } else {
            cx - x
        };
        let ry = if self.cfg.down_is_positive_y {
            cy + y
        } else {
            cy - y
        };
        (rx.round() as i32, ry.round() as i32)
    }

    /// One frame with these contacts, `(x, y)` relative to the centre, tracking ids from 0.
    pub fn frame(&mut self, contacts: &[(f32, f32)]) {
        for (i, &(x, y)) in contacts.iter().enumerate() {
            let (rx, ry) = self.raw(x, y);
            self.push(ev::ABS, abs::MT_TRACKING_ID, i as i32);
            self.push(ev::ABS, abs::MT_TOOL_TYPE, 0);
            self.push(ev::ABS, abs::MT_PRESSURE, 60);
            self.push(ev::ABS, abs::MT_TOUCH_MAJOR, 5);
            self.push(ev::ABS, abs::MT_TOUCH_MINOR, 4);
            self.push(ev::ABS, abs::MT_ORIENTATION, 1);
            self.push(ev::ABS, abs::MT_POSITION_X, rx);
            self.push(ev::ABS, abs::MT_POSITION_Y, ry);
            self.push(ev::SYN, syn::MT_REPORT, 0);
        }
        if contacts.is_empty() {
            self.push(ev::SYN, syn::MT_REPORT, 0);
        }
        self.push(ev::SYN, syn::REPORT, 0);
    }

    /// The all-up frame.
    pub fn lift(&mut self) {
        self.frame(&[]);
    }

    fn encoded_contact(&mut self, dx: i32, dy: i32) {
        self.push(ev::ABS, abs::MT_PRESSURE, 1);
        self.push(ev::ABS, abs::MT_TOUCH_MAJOR, 15);
        self.push(ev::ABS, abs::MT_TOUCH_MINOR, 15);
        self.push(ev::ABS, abs::MT_POSITION_X, 100 + dx);
        self.push(ev::ABS, abs::MT_POSITION_Y, 100 + dy);
        self.push(ev::SYN, syn::MT_REPORT, 0);
    }

    /// One screen-off encoded frame at raw (100 + dx, 100 + dy), as the driver sends it in
    /// early suspend (kernel-interfaces.md §1.5): one contact with pressure 1, size 15 and no
    /// tracking id. No all-up frame follows; real contacts may keep arriving around it.
    pub fn encoded(&mut self, dx: i32, dy: i32) {
        self.encoded_contact(dx, dy);
        self.push(ev::SYN, syn::REPORT, 0);
    }

    /// A `SYN_REPORT` with nothing before it, as the driver sends after an encoded frame
    /// (kernel-interfaces.md §1.5).
    pub fn bare_report(&mut self) {
        self.push(ev::SYN, syn::REPORT, 0);
    }

    /// One frame with real contacts and an encoded contact after them.
    pub fn frame_with_encoded(&mut self, contacts: &[(f32, f32)], dx: i32, dy: i32) {
        let mut real = self.clone();
        real.events.clear();
        real.frame(contacts);
        // Drop the SYN_REPORT of the real contacts, then add the encoded one.
        real.events.pop();
        self.events.extend(real.events);
        self.encoded(dx, dy);
    }

    /// The events so far, emptying the buffer.
    pub fn take(&mut self) -> Vec<RawEvent> {
        std::mem::take(&mut self.events)
    }

    fn fingers_at(n: u32, x: f32, y: f32) -> Vec<(f32, f32)> {
        let off = (n.max(1) - 1) as f32 * FINGER_SPACING / 2.0;
        (0..n.max(1))
            .map(|i| (x + i as f32 * FINGER_SPACING - off, y))
            .collect()
    }

    /// Touch with `fingers`, hold still `hold_ms`, lift. Time ends 10 ms after the lift.
    pub fn tap(&mut self, fingers: u32, hold_ms: u64) {
        let pos = Self::fingers_at(fingers, 0.0, 0.0);
        self.frame(&pos);
        let mut held = 0;
        while held + self.frame_ms < hold_ms {
            self.advance_ms(self.frame_ms);
            held += self.frame_ms;
            self.frame(&pos);
        }
        self.advance_ms(hold_ms - held);
        self.lift();
        self.advance_ms(10);
    }

    /// A straight stroke of `distance` device units in `duration_ms`, then lift.
    pub fn swipe(&mut self, dir: Direction, fingers: u32, distance: f32, duration_ms: u64) {
        let (ux, uy) = dir.unit();
        let (sx, sy) = (-ux * distance / 2.0, -uy * distance / 2.0);
        self.stroke(
            fingers,
            (sx, sy),
            (ux * distance, uy * distance),
            duration_ms,
        );
        self.lift();
        self.advance_ms(10);
    }

    /// Fingers down at `start`, moving by `delta` over `duration_ms` (one frame per
    /// `frame_ms`), fingers still down at the end.
    pub fn stroke(&mut self, fingers: u32, start: (f32, f32), delta: (f32, f32), duration_ms: u64) {
        self.frame(&Self::fingers_at(fingers, start.0, start.1));
        let steps = (duration_ms / self.frame_ms).max(1);
        for i in 1..=steps {
            self.advance_ms(self.frame_ms);
            let f = i as f32 / steps as f32;
            self.frame(&Self::fingers_at(
                fingers,
                start.0 + delta.0 * f,
                start.1 + delta.1 * f,
            ));
        }
    }

    /// A slow horizontal drag of `distance` device units (positive forward) over `duration_ms`,
    /// then the finger rests 100 ms (so the release velocity is low: no swipe) and lifts.
    pub fn drag(&mut self, fingers: u32, distance: f32, duration_ms: u64) {
        self.stroke(
            fingers,
            (-distance / 2.0, 0.0),
            (distance, 0.0),
            duration_ms,
        );
        let end = Self::fingers_at(fingers, distance / 2.0, 0.0);
        for _ in 0..(100 / self.frame_ms) {
            self.advance_ms(self.frame_ms);
            self.frame(&end);
        }
        self.lift();
        self.advance_ms(10);
    }
}
