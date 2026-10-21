//! Recognizer configuration: axis ranges, orientation and thresholds.
//!
//! Threshold defaults come from `docs/specs/touchpad-gestures.md` (§ numbers below). Values
//! that depend on the device (axis maxima, touch slop in device units) are marked
//! **[device]**: they must be checked on a Glass.

/// Range of one absolute axis, as reported by `EVIOCGABS`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisRange {
    pub min: i32,
    pub max: i32,
}

impl AxisRange {
    pub const fn new(min: i32, max: i32) -> Self {
        AxisRange { min, max }
    }

    /// `max - min`, at least 1.
    pub fn span(&self) -> f32 {
        ((self.max as i64 - self.min as i64).max(1)) as f32
    }
}

/// Everything the recognizer needs to know about the pad and the thresholds.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// `ABS_MT_POSITION_X` range. Default 0..1000: the kernel source only shows X spans at
    /// least ~1000 (kernel-interfaces.md §1.2). **[device]** glassd reads the real range.
    pub x: AxisRange,
    /// `ABS_MT_POSITION_Y` range. Default 0..187, same caveat.
    pub y: AxisRange,
    /// `ABS_MT_PRESSURE` maximum (255 on `sensor00fn11`, kernel-interfaces.md §1.2).
    pub pressure_max: i32,
    /// `true` if "forward" (toward the eye, GDK `SWIPE_RIGHT`) is increasing raw X.
    /// Stock: forward is +X (touchpad-gestures.md §3).
    pub forward_is_positive_x: bool,
    /// `true` if "down" is increasing raw Y. Stock: +Y is down (touchpad-gestures.md §3).
    pub down_is_positive_y: bool,
    /// Tap radius in device units. Stock uses Android's scaled touch slop (8 dp) in the
    /// touchpad's raw units (touchpad-gestures.md §4.2). **[device]** the density is unknown;
    /// 12 assumes 1.5 units per dp. Provisional.
    pub touch_slop: f32,
    /// Three fingers may move this many times the slop and still tap (§4.2: 2).
    pub three_finger_slop_factor: f32,
    /// Hold time for a long press (§4.2: Android's default 500 ms).
    pub long_press_ms: u64,
    /// A forward swipe needs an X release velocity above this, in device units per ms
    /// (§4.2: 0.7).
    pub forward_min_velocity: f32,
    /// A backward swipe needs an X release velocity below minus this, in device units per ms
    /// (§4.2: 1.0).
    pub backward_min_velocity: f32,
    /// Largest angle from the X axis, in degrees, for a forward swipe (§4.2: 18).
    pub forward_max_angle_deg: f32,
    /// Largest angle from the X axis, in degrees, for a backward swipe (§4.2: 25).
    pub backward_max_angle_deg: f32,
    /// Time constant of the velocity low-pass filter, ms (§4.2: 1/(2π·0.01) ≈ 15.915494).
    pub velocity_rc_ms: f32,
    /// Both fingers of a two-finger swipe must lift within this many ms (§4.2: 100).
    pub two_finger_release_window_ms: u64,
    /// Encoded screen-off frames: largest normalised pressure (§5.2: 0.01).
    pub encoded_max_pressure: f32,
    /// Encoded screen-off frames: smallest touch major and minor (§5.2: 14.8).
    pub encoded_min_size: f32,
    /// Encoded screen-off frames: smallest |dx| for a flick (§5.2: 6).
    pub encoded_min_flick: i32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            x: AxisRange::new(0, 1000),
            y: AxisRange::new(0, 187),
            pressure_max: 255,
            forward_is_positive_x: true,
            down_is_positive_y: true,
            touch_slop: 12.0,
            three_finger_slop_factor: 2.0,
            long_press_ms: 500,
            forward_min_velocity: 0.7,
            backward_min_velocity: 1.0,
            forward_max_angle_deg: 18.0,
            backward_max_angle_deg: 25.0,
            velocity_rc_ms: 15.915_494,
            two_finger_release_window_ms: 100,
            encoded_max_pressure: 0.01,
            encoded_min_size: 14.8,
            encoded_min_flick: 6,
        }
    }
}

impl Config {
    /// The defaults with the given axis ranges.
    pub fn with_axes(x: AxisRange, y: AxisRange) -> Self {
        Config {
            x,
            y,
            ..Config::default()
        }
    }
}
