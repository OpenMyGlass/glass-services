//! glasshub support for glassd (docs/specs/glasshub.md, docs/specs/kernel-interfaces.md §2):
//!
//! - [`sysfs`]: the kernel driver's attributes (presence, `don_doff` and `wink` events,
//!   enables, thresholds), with an allow-list of the attributes that may be written;
//! - [`wear`]: the on-head state machine (sensor, forced, power key, hinge), pure.
//!
//! Wink: the firmware's low-power wink interrupt (`wink`) is taken as the wink. The stock
//! system confirms it with a CPU classifier on the raw proximity stream; that classifier and
//! its models are not reproduced, so this is the "low-confidence" path of glasshub.md §8.

pub mod sysfs;
pub mod wear;

pub use sysfs::{GlassHub, Thresholds};
pub use wear::{Update, WearMachine};
