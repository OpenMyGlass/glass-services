//! Head gestures (docs/api.md "Head gestures", docs/specs/head-gestures.md) from the
//! MPU-9150's accelerometer and gyroscope, read through IIO.
//!
//! - [`iio`]: the IIO buffer reader (scan elements, the Glass format byte, timestamps,
//!   scales and the board orientation matrix) and the device setup;
//! - [`lookup`]: pitch and roll from gravity, look-up and look-down;
//! - [`nudge`]: our nudge detector (the stock one is a classifier whose model is not
//!   specified);
//! - [`relative`]: relative look-up (17° within 5 s);
//! - [`service`]: which detectors run, inhibition windows and forwarding rules;
//! - [`synth`]: synthetic sample streams for tests.
//!
//! Everything but the device I/O in [`iio::Imu`] is pure. Stock Glass detects the look-up
//! in the IMU's DMP firmware; that firmware image is not redistributable, so the look-up is
//! computed here from the raw accelerometer instead.

pub mod iio;
pub mod lookup;
pub mod nudge;
pub mod relative;
pub mod service;
pub mod synth;

pub use iio::{Channels, Imu, Sample};
pub use service::{Context, HeadService};
