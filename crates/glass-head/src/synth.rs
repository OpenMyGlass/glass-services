//! Synthetic IMU streams for tests (and the documentation of what the detectors expect).

use crate::iio::Sample;

/// Sample period at 50 Hz, ns.
pub const PERIOD_NS: i64 = 20_000_000;

/// The gravity vector (g, device frame, +y up when level) for a head pitch and roll.
pub fn gravity_at(pitch_deg: f32, roll_deg: f32) -> [f32; 3] {
    let (p, r) = (pitch_deg.to_radians(), roll_deg.to_radians());
    let h = p.cos();
    [h * r.sin(), h * r.cos(), -p.sin()]
}

/// Builds a 50 Hz stream from functions of time (s): pitch (degrees) and gyro (rad/s).
pub fn stream(
    start_ns: i64,
    duration_s: f32,
    pitch_deg: impl Fn(f32) -> f32,
    gyro: impl Fn(f32) -> [f32; 3],
) -> Vec<Sample> {
    let n = (duration_s * 50.0).round() as i64;
    (0..n)
        .map(|i| {
            let t = i as f32 / 50.0;
            Sample {
                t_ns: start_ns + i * PERIOD_NS,
                gyro: Some(gyro(t)),
                accel: Some(gravity_at(pitch_deg(t), 0.0)),
            }
        })
        .collect()
}

/// A half-sine pulse of `peak` lasting `dur` seconds from `t0`, then zero.
pub fn pulse(t: f32, t0: f32, dur: f32, peak: f32) -> f32 {
    if t < t0 || t > t0 + dur {
        0.0
    } else {
        peak * (std::f32::consts::PI * (t - t0) / dur).sin()
    }
}

/// A quick nudge on gyro x: up at `peak` rad/s for 0.15 s from `t0`, then back down at
/// half the peak for 0.2 s.
pub fn nudge_x(t: f32, t0: f32, peak: f32) -> f32 {
    pulse(t, t0, 0.15, peak) - pulse(t, t0 + 0.15, 0.2, peak * 0.5)
}
