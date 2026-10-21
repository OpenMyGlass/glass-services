//! Head pitch and roll from the accelerometer, and the look-up / look-down detector
//! (docs/api.md "Head gestures", docs/specs/head-gestures.md §2.2, §3.2).
//!
//! - Gravity is low-pass filtered (first order, 5 /s, the gain of the stock accelerometer
//!   look-up detector, head-gestures.md §7). Samples whose magnitude is outside
//!   0.5 g..1.5 g are rejected (the stock outlier limits).
//! - Pitch: `-atan2(gz, sqrt(gx² + gy²))`, positive up (head-gestures.md §2.2).
//! - Roll (ours, not in the specs): the tilt of gravity in the x-y plane away from +y,
//!   `atan2(gx, gy)`; this assumes +y points up when the head is level (Android frame).
//! - `LOOK_UP` when the pitch reaches the wake angle with |roll| under 10°; it then needs the
//!   pitch to fall below the wake angle minus 7° (`LOOK_DOWN`) before it can fire again.
//! - When the detector starts, it is armed only if the pitch is below the re-arm angle, so
//!   starting with the head already up does not fire.

/// Lower and upper accepted acceleration magnitudes, g.
pub const MIN_G: f32 = 0.5;
pub const MAX_G: f32 = 1.5;
/// Re-arm hysteresis below the wake angle, degrees.
pub const HYSTERESIS_DEG: f32 = 7.0;
/// Largest roll for a look-up, degrees.
pub const MAX_ROLL_DEG: f32 = 10.0;
/// Low-pass gain, 1/s.
pub const LOWPASS_PER_S: f32 = 5.0;
/// A gap longer than this restarts the filter, s.
const MAX_GAP_S: f32 = 1.0;

/// Pitch above the horizon, degrees, from a gravity vector.
pub fn pitch_deg(g: [f32; 3]) -> f32 {
    -g[2].atan2((g[0] * g[0] + g[1] * g[1]).sqrt()).to_degrees()
}

/// Roll, degrees, from a gravity vector (ours, see the module documentation).
pub fn roll_deg(g: [f32; 3]) -> f32 {
    g[0].atan2(g[1]).to_degrees()
}

/// Low-pass filtered gravity.
#[derive(Debug, Clone, Default)]
pub struct Gravity {
    g: Option<[f32; 3]>,
    last_ns: i64,
}

impl Gravity {
    /// Feeds one accelerometer sample (g); returns false if it was rejected.
    pub fn push(&mut self, t_ns: i64, a: [f32; 3]) -> bool {
        let m = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        if !(MIN_G..=MAX_G).contains(&m) {
            return false;
        }
        let dt = (t_ns - self.last_ns) as f32 / 1e9;
        self.last_ns = t_ns;
        match &mut self.g {
            Some(g) if dt > 0.0 && dt <= MAX_GAP_S => {
                let k = 1.0 - (-LOWPASS_PER_S * dt).exp();
                for i in 0..3 {
                    g[i] += k * (a[i] - g[i]);
                }
            }
            _ => self.g = Some(a),
        }
        true
    }

    pub fn get(&self) -> Option<[f32; 3]> {
        self.g
    }

    pub fn pitch(&self) -> Option<f32> {
        self.g.map(pitch_deg)
    }

    pub fn reset(&mut self) {
        self.g = None;
    }
}

/// What the look-up detector saw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Look {
    Up { pitch_deg: f32 },
    Down { pitch_deg: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not started, or no sample yet.
    Unknown,
    Armed,
    Fired,
}

#[derive(Debug, Clone)]
pub struct LookUp {
    angle_deg: f32,
    state: State,
}

impl LookUp {
    pub fn new(angle_deg: f32) -> Self {
        LookUp {
            angle_deg,
            state: State::Unknown,
        }
    }

    pub fn set_angle(&mut self, angle_deg: f32) {
        self.angle_deg = angle_deg;
    }

    pub fn angle(&self) -> f32 {
        self.angle_deg
    }

    /// A look-up fired and the pitch has not come back down yet.
    pub fn fired(&self) -> bool {
        self.state == State::Fired
    }

    /// Forgets the state (the detector stopped).
    pub fn reset(&mut self) {
        self.state = State::Unknown;
    }

    /// Sets the state as if a look-up (`true`) or a look-down (`false`) had been seen
    /// (the mock).
    pub fn force(&mut self, up: bool) {
        self.state = if up { State::Fired } else { State::Armed };
    }

    /// Feeds the current gravity estimate.
    pub fn push(&mut self, g: [f32; 3]) -> Option<Look> {
        let pitch = pitch_deg(g);
        let level = self.angle_deg - HYSTERESIS_DEG;
        match self.state {
            State::Unknown => {
                self.state = if pitch < level {
                    State::Armed
                } else {
                    State::Fired
                };
                None
            }
            State::Armed if pitch >= self.angle_deg && roll_deg(g).abs() < MAX_ROLL_DEG => {
                self.state = State::Fired;
                Some(Look::Up { pitch_deg: pitch })
            }
            State::Fired if pitch < level => {
                self.state = State::Armed;
                Some(Look::Down { pitch_deg: pitch })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::gravity_at;

    #[test]
    fn pitch_and_roll() {
        assert!((pitch_deg([0.0, 1.0, 0.0])).abs() < 1e-4);
        assert!((pitch_deg(gravity_at(30.0, 0.0)) - 30.0).abs() < 1e-3);
        assert!((pitch_deg(gravity_at(-20.0, 0.0)) + 20.0).abs() < 1e-3);
        assert!((roll_deg(gravity_at(10.0, 15.0)) - 15.0).abs() < 1e-3);
        assert!((pitch_deg(gravity_at(10.0, 15.0)) - 10.0).abs() < 1e-3);
    }

    #[test]
    fn gravity_rejects_outliers_and_filters() {
        let mut g = Gravity::default();
        assert!(!g.push(0, [0.0, 0.3, 0.0]));
        assert!(!g.push(0, [0.0, 1.6, 0.0]));
        assert!(g.get().is_none());
        assert!(g.push(0, [0.0, 1.0, 0.0]));
        // A step to 40° up: after 1 s at 50 Hz the estimate is close (5 /s).
        let up = gravity_at(40.0, 0.0);
        for i in 1..=50 {
            g.push(i * 20_000_000, up);
        }
        assert!((g.pitch().unwrap() - 40.0).abs() < 1.0, "{:?}", g.pitch());
        // A long gap restarts the filter at the new sample.
        g.push(5_000_000_000, [0.0, 1.0, 0.0]);
        assert!(g.pitch().unwrap().abs() < 1e-3);
    }

    #[test]
    fn fires_once_then_rearms_below_the_hysteresis() {
        let mut l = LookUp::new(30.0);
        assert_eq!(l.push(gravity_at(0.0, 0.0)), None);
        assert_eq!(l.push(gravity_at(29.0, 0.0)), None);
        assert!(matches!(
            l.push(gravity_at(31.0, 0.0)),
            Some(Look::Up { .. })
        ));
        assert!(l.fired());
        assert_eq!(l.push(gravity_at(35.0, 0.0)), None);
        assert_eq!(l.push(gravity_at(24.0, 0.0)), None, "above 30 - 7");
        assert!(matches!(
            l.push(gravity_at(22.0, 0.0)),
            Some(Look::Down { .. })
        ));
        assert!(matches!(
            l.push(gravity_at(30.0, 0.0)),
            Some(Look::Up { .. })
        ));
    }

    #[test]
    fn roll_blocks_look_up() {
        let mut l = LookUp::new(30.0);
        l.push(gravity_at(0.0, 0.0));
        assert_eq!(l.push(gravity_at(35.0, 12.0)), None);
        assert!(matches!(
            l.push(gravity_at(35.0, -8.0)),
            Some(Look::Up { .. })
        ));
    }

    #[test]
    fn starting_with_the_head_up_does_not_fire() {
        let mut l = LookUp::new(30.0);
        assert_eq!(l.push(gravity_at(35.0, 0.0)), None);
        assert_eq!(l.push(gravity_at(36.0, 0.0)), None);
        assert!(matches!(
            l.push(gravity_at(10.0, 0.0)),
            Some(Look::Down { .. })
        ));
        assert!(matches!(
            l.push(gravity_at(33.0, 0.0)),
            Some(Look::Up { .. })
        ));
        // Same at 25° (between the re-arm and the wake angle).
        let mut l = LookUp::new(30.0);
        l.push(gravity_at(25.0, 0.0));
        assert!(l.fired());
    }
}
