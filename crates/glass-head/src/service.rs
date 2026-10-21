//! Which detectors run, and the forwarding rules (docs/api.md "Head gestures",
//! docs/specs/head-gestures.md §3.1, §4.2-4.3, §5), as a pure state machine.
//!
//! Detectors run only on the head (the sensor is stopped otherwise, for battery):
//! - look-up / look-down: while `head_wake_enabled`, nobody asked for relative look-up, and
//!   the screen is off, or a look-up fired and the head has not come back down yet (so the
//!   look-down that re-arms it is seen); accelerometer only;
//! - nudge: while the screen is on and nudges are wanted (`nudge_sleep_enabled`, or a client
//!   asked for them); gyroscope;
//! - relative look-up: while a client asked for it; gyroscope. It replaces the look-up.
//!
//! Forwarding:
//! - `LOOK_UP` only with the screen off and outside the look-up inhibition; it starts the
//!   1000 ms nudge inhibition;
//! - `LOOK_DOWN` always (while the detector runs);
//! - `NUDGE` only with the screen on and outside the nudge inhibition; it sets the look-up
//!   inhibition to at least 500 ms and restarts the nudge inhibition;
//! - `RELATIVE_LOOK_UP` always; it starts the nudge inhibition;
//! - a client can set the look-up inhibition for N ms (0 clears it).
//!
//! Times: the inhibitions use the caller's `CLOCK_MONOTONIC` (µs); the detectors use the
//! sensor's own timestamps for their time steps.

use crate::iio::{Channels, Sample};
use crate::lookup::{Gravity, Look, LookUp};
use crate::nudge::{Nudge, NudgeConfig};
use crate::relative::RelativeLookUp;
use glass_api::proto::{HeadGesture, HeadGestureKind};

pub const NUDGE_INHIBIT_MS: u64 = 1000;
pub const LOOK_UP_INHIBIT_AFTER_NUDGE_MS: u64 = 500;

/// What the daemon knows that the detectors depend on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Context {
    pub screen_off: bool,
    pub on_head: bool,
    pub head_wake_enabled: bool,
    pub wake_angle_deg: f32,
    /// `nudge_sleep_enabled`, or a client asked for nudges.
    pub nudge_wanted: bool,
    /// A client asked for relative look-up.
    pub relative_wanted: bool,
}

impl Default for Context {
    fn default() -> Self {
        Context {
            screen_off: false,
            on_head: true,
            head_wake_enabled: true,
            wake_angle_deg: 30.0,
            nudge_wanted: false,
            relative_wanted: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Active {
    look_up: bool,
    nudge: bool,
    relative: bool,
}

#[derive(Debug, Clone)]
pub struct HeadService {
    ctx: Context,
    active: Active,
    gravity: Gravity,
    look_up: LookUp,
    nudge: Nudge,
    relative: RelativeLookUp,
    look_up_inhibit_until_us: u64,
    nudge_inhibit_until_us: u64,
}

impl Default for HeadService {
    fn default() -> Self {
        Self::new(Context::default())
    }
}

impl HeadService {
    pub fn new(ctx: Context) -> Self {
        let mut s = HeadService {
            ctx,
            active: Active::default(),
            gravity: Gravity::default(),
            look_up: LookUp::new(ctx.wake_angle_deg),
            nudge: Nudge::new(NudgeConfig::default()),
            relative: RelativeLookUp::new(),
            look_up_inhibit_until_us: 0,
            nudge_inhibit_until_us: 0,
        };
        s.set_context(ctx);
        s
    }

    fn compute(&self, ctx: &Context) -> Active {
        if !ctx.on_head {
            return Active::default();
        }
        Active {
            look_up: ctx.head_wake_enabled
                && !ctx.relative_wanted
                && (ctx.screen_off || self.look_up.fired()),
            nudge: !ctx.screen_off && ctx.nudge_wanted,
            relative: ctx.relative_wanted,
        }
    }

    /// Updates the context; detectors that start are reset.
    pub fn set_context(&mut self, ctx: Context) {
        self.look_up.set_angle(ctx.wake_angle_deg);
        let next = self.compute(&ctx);
        if next.look_up != self.active.look_up {
            self.look_up.reset();
        }
        if next.nudge && !self.active.nudge {
            self.nudge.reset();
        }
        if next.relative && !self.active.relative {
            self.relative.reset();
        }
        if !self.channels_of(&next).accel {
            self.gravity.reset();
        }
        self.ctx = ctx;
        self.active = next;
    }

    pub fn context(&self) -> Context {
        self.ctx
    }

    fn channels_of(&self, a: &Active) -> Channels {
        let gyro = a.nudge || a.relative;
        Channels {
            // The accelerometer also gives the pitch reported with nudges.
            accel: a.look_up || gyro,
            gyro,
        }
    }

    /// The sensors that must stream now.
    pub fn channels(&self) -> Channels {
        self.channels_of(&self.active)
    }

    /// The current pitch estimate, degrees.
    pub fn pitch_deg(&self) -> Option<f32> {
        self.gravity.pitch()
    }

    /// A client sets the look-up inhibition for `ms` (0 clears it).
    pub fn inhibit_look_up(&mut self, now_us: u64, ms: u32) {
        self.look_up_inhibit_until_us = if ms == 0 {
            0
        } else {
            now_us + u64::from(ms) * 1000
        };
    }

    fn event(&self, kind: HeadGestureKind, pitch_deg: f32, now_us: u64) -> HeadGesture {
        HeadGesture {
            kind: kind as i32,
            pitch_deg,
            timestamp_us: now_us,
        }
    }

    fn forward(&mut self, kind: HeadGestureKind, pitch: f32, now_us: u64) -> Option<HeadGesture> {
        match kind {
            HeadGestureKind::LookUp => {
                if !self.ctx.screen_off || now_us < self.look_up_inhibit_until_us {
                    return None;
                }
                self.nudge_inhibit_until_us = now_us + NUDGE_INHIBIT_MS * 1000;
            }
            HeadGestureKind::Nudge => {
                if self.ctx.screen_off || now_us < self.nudge_inhibit_until_us {
                    return None;
                }
                self.look_up_inhibit_until_us = self
                    .look_up_inhibit_until_us
                    .max(now_us + LOOK_UP_INHIBIT_AFTER_NUDGE_MS * 1000);
                self.nudge_inhibit_until_us = now_us + NUDGE_INHIBIT_MS * 1000;
            }
            HeadGestureKind::RelativeLookUp => {
                self.nudge_inhibit_until_us = now_us + NUDGE_INHIBIT_MS * 1000;
            }
            HeadGestureKind::LookDown | HeadGestureKind::Unspecified => {}
        }
        Some(self.event(kind, pitch, now_us))
    }

    /// One IMU sample. After a look-down the detectors may stop: check
    /// [`channels`](Self::channels) after each call.
    pub fn push(&mut self, s: &Sample, now_us: u64) -> Vec<HeadGesture> {
        let mut out = Vec::new();
        if let Some(a) = s.accel
            && self.gravity.push(s.t_ns, a)
            && self.active.look_up
            && let Some(g) = self.gravity.get()
        {
            let look = self.look_up.push(g);
            match look {
                Some(Look::Up { pitch_deg }) => {
                    out.extend(self.forward(HeadGestureKind::LookUp, pitch_deg, now_us))
                }
                Some(Look::Down { pitch_deg }) => {
                    out.extend(self.forward(HeadGestureKind::LookDown, pitch_deg, now_us));
                    // The screen may be on: the detector may stop now.
                    self.set_context(self.ctx);
                }
                None => {}
            }
        }
        let pitch = self.gravity.pitch().unwrap_or(0.0);
        if let Some(g) = s.gyro {
            if self.active.nudge && self.nudge.push(s.t_ns, g) {
                out.extend(self.forward(HeadGestureKind::Nudge, pitch, now_us));
            }
            if self.active.relative && self.relative.push(s.t_ns, g[0]) {
                out.extend(self.forward(HeadGestureKind::RelativeLookUp, pitch, now_us));
            }
        }
        out
    }

    /// A gesture from the mock, under the same rules as a detected one: it is forwarded
    /// only if its detector runs and the forwarding rules allow it.
    pub fn inject(&mut self, kind: HeadGestureKind, now_us: u64) -> Option<HeadGesture> {
        let angle = self.ctx.wake_angle_deg;
        let pitch = |d: f32| self.gravity.pitch().unwrap_or(d);
        match kind {
            HeadGestureKind::LookUp if self.active.look_up && !self.look_up.fired() => {
                self.look_up.force(true);
                let r = self.forward(kind, angle + 5.0, now_us);
                self.set_context(self.ctx);
                r
            }
            HeadGestureKind::LookDown if self.active.look_up && self.look_up.fired() => {
                self.look_up.force(false);
                let r = self.forward(kind, angle - 10.0, now_us);
                self.set_context(self.ctx);
                r
            }
            HeadGestureKind::Nudge if self.active.nudge => {
                let p = pitch(0.0);
                self.forward(kind, p, now_us)
            }
            HeadGestureKind::RelativeLookUp if self.active.relative => {
                let p = pitch(0.0);
                self.forward(kind, p, now_us)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{nudge_x, stream};

    const MS: u64 = 1000;

    fn off() -> Context {
        Context {
            screen_off: true,
            ..Context::default()
        }
    }

    fn kinds(v: &[HeadGesture]) -> Vec<HeadGestureKind> {
        v.iter()
            .map(|g| HeadGestureKind::try_from(g.kind).unwrap())
            .collect()
    }

    /// Feeds samples; the monotonic time follows the sensor time.
    fn run(h: &mut HeadService, s: &[Sample]) -> Vec<HeadGesture> {
        s.iter()
            .flat_map(|x| h.push(x, (x.t_ns / 1000) as u64))
            .collect()
    }

    #[test]
    fn sensors_run_only_when_needed() {
        let mut h = HeadService::new(Context::default());
        assert_eq!(
            h.channels(),
            Channels::default(),
            "screen on, nothing wanted"
        );
        h.set_context(off());
        assert_eq!(
            h.channels(),
            Channels {
                accel: true,
                gyro: false
            }
        );
        h.set_context(Context {
            on_head: false,
            ..off()
        });
        assert!(!h.channels().any(), "off the head");
        h.set_context(Context {
            head_wake_enabled: false,
            ..off()
        });
        assert!(!h.channels().any());
        h.set_context(Context {
            nudge_wanted: true,
            ..Context::default()
        });
        assert_eq!(
            h.channels(),
            Channels {
                accel: true,
                gyro: true
            }
        );
        // Nudge needs the screen on.
        h.set_context(Context {
            nudge_wanted: true,
            ..off()
        });
        assert!(!h.channels().gyro);
        h.set_context(Context {
            relative_wanted: true,
            ..off()
        });
        assert!(h.channels().gyro);
    }

    #[test]
    fn look_up_with_screen_off_then_look_down_after_the_wake() {
        let mut h = HeadService::new(off());
        // Level for 1 s, then up to 40° over 0.5 s, hold, then back to 0 at 3 s.
        let s = stream(
            0,
            4.0,
            |t| {
                if t < 1.0 {
                    0.0
                } else if t < 1.5 {
                    (t - 1.0) * 80.0
                } else if t < 3.0 {
                    40.0
                } else {
                    0.0
                }
            },
            |_| [0.0; 3],
        );
        let mut out = Vec::new();
        for x in &s {
            let now = (x.t_ns / 1000) as u64;
            let ev = h.push(x, now);
            if ev.iter().any(|g| g.kind == HeadGestureKind::LookUp as i32) {
                // The daemon wakes the screen.
                h.set_context(Context::default());
                assert!(h.channels().accel, "keeps running until the look-down");
            }
            out.extend(ev);
        }
        assert_eq!(
            kinds(&out),
            vec![HeadGestureKind::LookUp, HeadGestureKind::LookDown]
        );
        assert!(out[0].pitch_deg >= 30.0);
        assert!(out[1].pitch_deg < 23.0);
        assert!(
            !h.channels().any(),
            "stopped after the look-down with the screen on"
        );
    }

    #[test]
    fn look_up_inhibited_by_a_client_and_replaced_by_relative() {
        let up = stream(0, 2.0, |t| if t < 0.5 { 0.0 } else { 40.0 }, |_| [0.0; 3]);
        let mut h = HeadService::new(off());
        h.inhibit_look_up(0, 5000);
        assert!(run(&mut h, &up).is_empty());
        let mut h = HeadService::new(off());
        h.inhibit_look_up(0, 5000);
        h.inhibit_look_up(0, 0);
        assert_eq!(kinds(&run(&mut h, &up)), vec![HeadGestureKind::LookUp]);
        let mut h = HeadService::new(Context {
            relative_wanted: true,
            ..off()
        });
        // Look-up does not run; gyro x is zero in this stream, so no relative either.
        assert!(run(&mut h, &up).is_empty());
    }

    #[test]
    fn nudge_rules_and_inhibitions() {
        let ctx = Context {
            nudge_wanted: true,
            ..Context::default()
        };
        let nudge = |start: f32| {
            stream(
                (start * 1e9) as i64,
                1.0,
                |_| 0.0,
                |t| [nudge_x(t, 0.2, 2.0), 0.0, 0.0],
            )
        };
        let mut h = HeadService::new(ctx);
        let first = run(&mut h, &nudge(0.0));
        assert_eq!(kinds(&first), vec![HeadGestureKind::Nudge]);
        // A second nudge 0.6 s later is inside the 1000 ms nudge inhibition.
        assert!(run(&mut h, &nudge(0.6)).is_empty());
        let third = run(&mut h, &nudge(2.0));
        assert_eq!(kinds(&third), vec![HeadGestureKind::Nudge]);
        // The nudge started a 500 ms look-up inhibition.
        let t = third[0].timestamp_us;
        h.set_context(off());
        let mut h2 = h.clone();
        assert_eq!(h.inject(HeadGestureKind::LookUp, t + 300 * MS), None);
        assert!(h2.inject(HeadGestureKind::LookUp, t + 600 * MS).is_some());
        // A look-up starts the 1000 ms nudge inhibition.
        h2.set_context(ctx);
        assert_eq!(h2.inject(HeadGestureKind::Nudge, t + 1500 * MS), None);
        assert!(h2.inject(HeadGestureKind::Nudge, t + 1700 * MS).is_some());
    }

    #[test]
    fn relative_look_up_fires_from_any_angle_with_screen_on() {
        let mut h = HeadService::new(Context {
            relative_wanted: true,
            ..Context::default()
        });
        let rate = 20f32.to_radians();
        let s = stream(
            0,
            3.0,
            |_| 10.0,
            |t| [if (1.0..2.0).contains(&t) { rate } else { 0.0 }, 0.0, 0.0],
        );
        let out = run(&mut h, &s);
        assert_eq!(kinds(&out), vec![HeadGestureKind::RelativeLookUp]);
        assert!((out[0].pitch_deg - 10.0).abs() < 1.0);
    }

    #[test]
    fn inject_follows_the_rules() {
        let mut h = HeadService::new(Context::default());
        // Screen on: no look-up detector, no nudge wanted.
        assert_eq!(h.inject(HeadGestureKind::LookUp, 0), None);
        assert_eq!(h.inject(HeadGestureKind::Nudge, 0), None);
        h.set_context(off());
        let g = h.inject(HeadGestureKind::LookUp, 0).unwrap();
        assert_eq!(g.pitch_deg, 35.0);
        assert_eq!(h.inject(HeadGestureKind::LookUp, 0), None, "not re-armed");
        h.set_context(Context::default()); // woken: keeps running until the look-down
        assert!(h.channels().accel);
        assert!(h.inject(HeadGestureKind::LookDown, 0).is_some());
        assert!(!h.channels().any());
        assert_eq!(h.inject(HeadGestureKind::RelativeLookUp, 0), None);
    }
}
