//! Relative look-up: the head pitched up by 17° or more within 5 s, from any start angle
//! (docs/specs/head-gestures.md §5).
//!
//! At most every 0.1 s, gyro x (the pitch rate) is integrated backwards from the newest
//! sample over the samples newer than the last trigger and the 5 s window (`Σ x·dt`); it
//! fires when that running sum passes 17°. After firing only newer samples count.

use std::collections::VecDeque;

pub const ANGLE_DEG: f32 = 17.0;
pub const WINDOW_S: f32 = 5.0;
pub const EVAL_EVERY_S: f32 = 0.1;

#[derive(Debug, Clone, Default)]
pub struct RelativeLookUp {
    samples: VecDeque<(i64, f32)>,
    trigger_ns: Option<i64>,
    eval_ns: Option<i64>,
}

impl RelativeLookUp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feeds gyro x (rad/s); returns true when it fires.
    pub fn push(&mut self, t_ns: i64, x: f32) -> bool {
        if self.samples.back().is_some_and(|&(t, _)| t >= t_ns) {
            // Time went backwards or repeated: start again.
            self.reset();
        }
        self.samples.push_back((t_ns, x));
        let horizon = t_ns - (WINDOW_S * 1e9) as i64;
        while self.samples.front().is_some_and(|&(t, _)| t < horizon) {
            self.samples.pop_front();
        }
        if self
            .eval_ns
            .is_some_and(|e| ((t_ns - e) as f32 / 1e9) < EVAL_EVERY_S)
        {
            return false;
        }
        self.eval_ns = Some(t_ns);
        let after = self.trigger_ns.unwrap_or(i64::MIN);
        let threshold = ANGLE_DEG.to_radians();
        let mut sum = 0f32;
        let mut it = self.samples.iter().rev().peekable();
        while let Some(&(t, x)) = it.next() {
            let Some(&&(tp, _)) = it.peek() else { break };
            if tp <= after {
                break;
            }
            sum += x * (t - tp) as f32 / 1e9;
            if sum > threshold {
                self.trigger_ns = Some(t_ns);
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{PERIOD_NS, pulse, stream};

    fn fires(gyro_x: impl Fn(f32) -> f32, dur: f32) -> Vec<f32> {
        let mut r = RelativeLookUp::new();
        stream(0, dur, |_| 0.0, |t| [gyro_x(t), 0.0, 0.0])
            .iter()
            .filter(|s| r.push(s.t_ns, s.gyro.unwrap()[0]))
            .map(|s| s.t_ns as f32 / 1e9)
            .collect()
    }

    #[test]
    fn twenty_degrees_in_two_seconds_fires_once() {
        // 20° over 2 s at a constant rate, then still.
        let rate = 20f32.to_radians() / 2.0;
        let f = fires(|t| if (1.0..3.0).contains(&t) { rate } else { 0.0 }, 6.0);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!((2.6..3.0).contains(&f[0]), "{f:?}");
    }

    #[test]
    fn fifteen_degrees_does_not_fire_and_slow_drift_is_forgotten() {
        let rate = 15f32.to_radians();
        assert!(fires(|t| if (1.0..2.0).contains(&t) { rate } else { 0.0 }, 4.0).is_empty());
        // 20° spread over 8 s: never 17° within 5 s.
        let slow = 20f32.to_radians() / 8.0;
        assert!(fires(|t| if (1.0..9.0).contains(&t) { slow } else { 0.0 }, 10.0).is_empty());
    }

    #[test]
    fn a_dip_then_a_rise_counts_from_the_bottom() {
        // Down 10°, then up 18°: the rise alone passes 17°.
        let f = fires(
            |t| {
                pulse(
                    t,
                    0.5,
                    1.0,
                    -(10f32.to_radians()) * std::f32::consts::PI / 2.0,
                ) + pulse(t, 1.6, 1.0, 18f32.to_radians() * std::f32::consts::PI / 2.0)
            },
            4.0,
        );
        assert_eq!(f.len(), 1, "{f:?}");
    }

    #[test]
    fn after_firing_only_newer_samples_count() {
        // 40° up in 2 s: fires once at 17°, then again after 17° more.
        let rate = 40f32.to_radians() / 2.0;
        let f = fires(|t| if (1.0..3.0).contains(&t) { rate } else { 0.0 }, 5.0);
        assert_eq!(f.len(), 2, "{f:?}");
    }

    #[test]
    fn evaluates_at_most_every_100_ms() {
        let mut r = RelativeLookUp::new();
        assert!(!r.push(0, 0.0));
        // A huge rate 20 ms later is not evaluated yet.
        assert!(!r.push(PERIOD_NS, 100.0));
        assert!(r.push(6 * PERIOD_NS, 0.0));
    }
}
