//! The setup QR: from a grayscale camera frame to a validated offer the user can confirm.
//!
//! A QR is untrusted input from whatever is in front of the camera, and a setup QR carries
//! secrets. So there are two steps with a narrow view between them:
//!
//! - [`scan`] (feature `decoder`) finds QR codes in a frame and returns the text of the first one
//!   that is a valid setup. It belongs in the process that owns the frames: it is the part with
//!   the large attack surface, and its module doc says what a bug there does.
//! - [`setup`] parses that text under strict limits into a [`setup::Setup`]. Its
//!   [`setup::Setup::offer`] is the only part for the launcher, and holds no secret. Nothing may
//!   be applied until the user has confirmed the offer with a tap.
//!
//! No part of this crate logs, writes a file or touches the camera.

#[cfg(feature = "decoder")]
pub mod scan;
pub mod setup;
