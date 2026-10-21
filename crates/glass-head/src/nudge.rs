//! Nudge detector: a quick upward flick of the head that comes back (docs/api.md "Head
//! gestures", docs/specs/head-gestures.md §4).
//!
//! The stock detector is a boosted classifier whose model is Google data and is not in the
//! specs. This one is **ours**: a hand-designed threshold detector built from the behaviour
//! and limits the spec describes. Gyro x is the pitch rate (+ = head tilting up).
//!
//! 1. Frames closer than 15 ms to the previous one are skipped (as stock).
//! 2. A candidate starts when the pitch rate rises above [`NudgeConfig::up_rate`]
//!    (1.2 rad/s, the "moderate" preset of the stock threshold detector).
//! 3. It completes when the head comes back: the pitch rate falls below
//!    `-return_rate` (0.4 rad/s: the size of the "liberal" nod-down preset of the stock
//!    detectors, head-gestures.md §7; using it for the return of a nudge is our choice) within [`NudgeConfig::max_s`] (0.75 s, the stock
//!    wait-for-down timeout). A slower motion, such as looking up and staying up, is not a
//!    nudge.
//! 4. The other axes must be quiet over the last 1.0 s (stock limits): with the largest
//!    absolute rates `mx`, `my`, `mz`, `my/mx < 0.83`, `mz/mx < 0.42`, `my < 3.2 rad/s`,
//!    `mz < 2.0 rad/s`; and the integrated |y| and |z| over the candidate each stay under
//!    20° (the stock threshold detector's limit).
//! 5. At most one nudge per 0.5 s (stock).
//!
//! The thresholds need tuning on a device (head-gestures.md §9).

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NudgeConfig {
    /// Pitch rate that starts a candidate, rad/s.
    pub up_rate: f32,
    /// Downward pitch rate that completes it, rad/s (positive number).
    pub return_rate: f32,
    /// Longest candidate, s.
    pub max_s: f32,
    /// Window of the quietness check, s.
    pub quiet_s: f32,
    pub max_ratio_y: f32,
    pub max_ratio_z: f32,
    pub max_y: f32,
    pub max_z: f32,
    /// Largest integrated non-pitch rotation during the candidate, degrees.
    pub max_side_deg: f32,
    /// Frames closer than this are skipped, s.
    pub min_frame_s: f32,
    /// Shortest time between two nudges, s.
    pub refractory_s: f32,
}

impl Default for NudgeConfig {
    fn default() -> Self {
        NudgeConfig {
            up_rate: 1.2,
            return_rate: 0.4,
            max_s: 0.75,
            quiet_s: 1.0,
            max_ratio_y: 0.83,
            max_ratio_z: 0.42,
            max_y: 3.2,
            max_z: 2.0,
            max_side_deg: 20.0,
            min_frame_s: 0.015,
            refractory_s: 0.5,
        }
    }
}

/// A gap in the stream longer than this resets the detector, s.
const MAX_GAP_S: f32 = 0.2;

#[derive(Debug, Clone)]
pub struct Nudge {
    cfg: NudgeConfig,
    frames: VecDeque<(i64, [f32; 3])>,
    start_ns: Option<i64>,
    last_ns: Option<i64>,
    accepted_ns: Option<i64>,
}

fn secs(ns: i64) -> f32 {
    ns as f32 / 1e9
}

impl Nudge {
    pub fn new(cfg: NudgeConfig) -> Self {
        Nudge {
            cfg,
            frames: VecDeque::new(),
            start_ns: None,
            last_ns: None,
            accepted_ns: None,
        }
    }

    pub fn reset(&mut self) {
        self.frames.clear();
        self.start_ns = None;
        self.last_ns = None;
    }

    /// Feeds one gyro frame (rad/s); returns true on a nudge.
    pub fn push(&mut self, t_ns: i64, g: [f32; 3]) -> bool {
        if let Some(last) = self.last_ns {
            let dt = secs(t_ns - last);
            if dt < self.cfg.min_frame_s && dt >= 0.0 {
                return false;
            }
            if !(0.0..=MAX_GAP_S).contains(&dt) {
                self.reset();
            }
        }
        self.last_ns = Some(t_ns);
        self.frames.push_back((t_ns, g));
        let horizon = t_ns - (self.cfg.quiet_s * 1e9) as i64;
        while self.frames.front().is_some_and(|&(t, _)| t < horizon) {
            self.frames.pop_front();
        }
        let x = g[0];
        match self.start_ns {
            None => {
                if x > self.cfg.up_rate {
                    self.start_ns = Some(t_ns);
                }
                false
            }
            Some(start) if secs(t_ns - start) > self.cfg.max_s => {
                self.start_ns = None;
                false
            }
            Some(start) => {
                if x >= -self.cfg.return_rate {
                    return false;
                }
                self.start_ns = None;
                let ok = self.quiet(start)
                    && self
                        .accepted_ns
                        .is_none_or(|a| secs(t_ns - a) > self.cfg.refractory_s);
                if ok {
                    self.accepted_ns = Some(t_ns);
                }
                ok
            }
        }
    }

    fn quiet(&self, start_ns: i64) -> bool {
        let (mut mx, mut my, mut mz) = (0f32, 0f32, 0f32);
        for (_, g) in &self.frames {
            mx = mx.max(g[0].abs());
            my = my.max(g[1].abs());
            mz = mz.max(g[2].abs());
        }
        if mx <= 0.0
            || my / mx >= self.cfg.max_ratio_y
            || mz / mx >= self.cfg.max_ratio_z
            || my >= self.cfg.max_y
            || mz >= self.cfg.max_z
        {
            return false;
        }
        let (mut ay, mut az) = (0f32, 0f32);
        let mut prev: Option<i64> = None;
        for &(t, g) in self.frames.iter().filter(|(t, _)| *t >= start_ns) {
            if let Some(p) = prev {
                let dt = secs(t - p);
                ay += g[1].abs() * dt;
                az += g[2].abs() * dt;
            }
            prev = Some(t);
        }
        ay.to_degrees() < self.cfg.max_side_deg && az.to_degrees() < self.cfg.max_side_deg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{PERIOD_NS, nudge_x, pulse, stream};

    fn count(samples: &[crate::iio::Sample]) -> usize {
        let mut n = Nudge::new(NudgeConfig::default());
        samples
            .iter()
            .filter(|s| n.push(s.t_ns, s.gyro.unwrap()))
            .count()
    }

    #[test]
    fn quick_nudge_is_detected_once() {
        let s = stream(0, 2.0, |_| 0.0, |t| [nudge_x(t, 0.5, 2.0), 0.05, -0.05]);
        assert_eq!(count(&s), 1);
    }

    #[test]
    fn two_nudges_need_the_refractory_time() {
        let s = stream(
            0,
            3.0,
            |_| 0.0,
            |t| {
                [
                    nudge_x(t, 0.5, 2.0) + nudge_x(t, 0.9, 2.0) + nudge_x(t, 2.0, 2.0),
                    0.0,
                    0.0,
                ]
            },
        );
        // The second starts 0.4 s after the first and completes inside 0.5 s of it.
        assert_eq!(count(&s), 2);
    }

    #[test]
    fn slow_look_up_and_staying_up_is_not_a_nudge() {
        // Up at 1.5 rad/s for 0.4 s, then still: no return within 0.75 s.
        let s = stream(0, 2.0, |_| 0.0, |t| [pulse(t, 0.5, 0.4, 1.5), 0.0, 0.0]);
        assert_eq!(count(&s), 0);
        // A slow motion under the up rate, even if it comes back.
        let s = stream(0, 3.0, |_| 0.0, |t| [nudge_x(t, 0.5, 1.0), 0.0, 0.0]);
        assert_eq!(count(&s), 0);
    }

    #[test]
    fn other_axes_must_be_quiet() {
        // Yaw while nudging: y/x too large.
        let s = stream(
            0,
            2.0,
            |_| 0.0,
            |t| [nudge_x(t, 0.5, 2.0), pulse(t, 0.4, 0.4, 1.8), 0.0],
        );
        assert_eq!(count(&s), 0);
        // Roll: z/x too large.
        let s = stream(
            0,
            2.0,
            |_| 0.0,
            |t| [nudge_x(t, 0.5, 2.0), 0.0, pulse(t, 0.4, 0.4, 0.9)],
        );
        assert_eq!(count(&s), 0);
        // Some y under the ratio is fine.
        let s = stream(
            0,
            2.0,
            |_| 0.0,
            |t| [nudge_x(t, 0.5, 2.0), pulse(t, 0.4, 0.4, 1.2), 0.0],
        );
        assert_eq!(count(&s), 1);
    }

    #[test]
    fn fast_frames_are_skipped_and_gaps_reset() {
        let mut n = Nudge::new(NudgeConfig::default());
        assert!(!n.push(0, [2.0, 0.0, 0.0]));
        // 5 ms later: skipped, so it cannot complete the candidate.
        assert!(!n.push(5_000_000, [-1.0, 0.0, 0.0]));
        // A gap of 1 s resets the candidate.
        assert!(!n.push(1_000_000_000, [-1.0, 0.0, 0.0]));
        assert!(!n.push(1_000_000_000 + PERIOD_NS, [-1.0, 0.0, 0.0]));
    }
}
