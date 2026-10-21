//! glasshub support for glassd (docs/specs/glasshub.md, docs/specs/kernel-interfaces.md §2):
//!
//! - [`sysfs`]: the kernel driver's attributes (presence, `don_doff` and `wink` events,
//!   enables, thresholds), with an allow-list of the attributes that may be written;
//! - [`wear`]: the on-head state machine (sensor, forced, power key, hinge), pure.
//!
//! Wink: the firmware's low-power wink interrupt (`wink`) is taken as the wink, without the
//! confirmation the stock system adds (a CPU classifier on the raw proximity stream, whose
//! models are not reproduced). glasshub.md §8 offers this as an option; it is unconfirmed
//! and expected to give false positives.

pub mod sysfs;
pub mod wear;

pub use sysfs::{GlassHub, Thresholds};
pub use wear::{Update, WearMachine};
