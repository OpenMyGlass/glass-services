//! Finding QR codes in a grayscale frame, with `quircs`.
//!
//! **Why not `rqrr`:** it was tried first. It copies the whole frame for each candidate triple of
//! finder patterns, so a 640x480 frame tiled with them took over 15 s on the host, and it read less
//! in a dim scene. `quircs` holds at most 32 finder patterns and 8 grids, and read more in the dim
//! and noisy cases tried. It was weaker only below 3 pixels per module.
//!
//! **The time for a frame has no useful bound.** `quircs` cannot be interrupted, and its search for
//! the alignment pattern takes about 100 steps per pixel of a module's estimated area, an estimate
//! that wrong finder patterns can make far larger than the frame. In 30000 random synthetic
//! frames of 320x240, one took 5 s on the host this way, against about a millisecond for most.
//!
//! **A panic here ends the process.** The frame comes from the world, `quircs` has assertions, and
//! the release profile aborts on a panic. None was found in release builds (see the `stress` test
//! in `tests/scan.rs`), which does not prove there is none.
//!
//! So this belongs in the process that owns the camera, on its own thread, with the process killed
//! when the scan's deadline passes, and not in glassd, which builds this crate without the
//! `decoder` feature.
//!
//! The text of a code is wiped when dropped, as a best effort. This code cannot wipe the frame,
//! which belongs to the caller, nor the copy of it that [`Scanner`] holds in black and white until
//! the next frame or its end, nor what `quircs` left in memory it has freed.

use quircs::Quirc;
use zeroize::Zeroizing;

use crate::setup::{self, MAX_TEXT_BYTES, SetupError};

/// The largest frame accepted: the largest the camera design scans at (1280x720).
pub const MAX_PIXELS: usize = 1280 * 720;
/// The smallest side of a frame: version 1 of a QR code at one pixel per module.
pub const MIN_SIDE: usize = 21;
/// Codes decoded in one frame at most. More than one in view is already unusual.
pub const MAX_CODES: usize = 4;

/// One luma plane, 8 bits per pixel. Printing one shows its size, not what the camera saw.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
}

impl std::fmt::Debug for Frame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{})", self.width, self.height)
    }
}

/// Why a buffer is not a frame this code takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// A side below [`MIN_SIDE`], or more pixels than [`MAX_PIXELS`].
    Size,
    /// The stride is smaller than the width.
    Stride,
    /// The buffer ends before the last pixel.
    ShortBuffer,
}

impl<'a> Frame<'a> {
    /// A frame of `height` rows, each starting `stride` bytes after the last. The last row need
    /// not have its padding.
    pub fn new(
        data: &'a [u8],
        width: usize,
        height: usize,
        stride: usize,
    ) -> Result<Frame<'a>, FrameError> {
        if width < MIN_SIDE
            || height < MIN_SIDE
            || width.checked_mul(height).is_none_or(|n| n > MAX_PIXELS)
        {
            return Err(FrameError::Size);
        }
        if stride < width {
            return Err(FrameError::Stride);
        }
        let needed = stride
            .checked_mul(height - 1)
            .and_then(|n| n.checked_add(width))
            .ok_or(FrameError::ShortBuffer)?;
        if data.len() < needed {
            return Err(FrameError::ShortBuffer);
        }
        Ok(Frame {
            data,
            width,
            height,
            stride,
        })
    }
}

/// What a frame held.
pub enum Scan {
    /// No code that could be read.
    NoCode,
    /// Codes that are not valid setup codes, and the reason for the last one. Keep scanning: a
    /// link on a poster in view must not end the scan.
    Refused(SetupError),
    /// The text of the first valid setup code, which [`setup::parse`] accepts.
    Setup(Zeroizing<Vec<u8>>),
}

/// Reads frames, one at a time. It keeps its buffers from one frame to the next, so make one per
/// scanning session rather than one per frame.
#[derive(Default)]
pub struct Scanner {
    quirc: Quirc,
    /// Rows without their padding, when the frame has any.
    tight: Zeroizing<Vec<u8>>,
}

impl Scanner {
    pub fn new() -> Scanner {
        Scanner::default()
    }

    /// The texts of up to [`MAX_CODES`] codes in the frame, of at most [`MAX_TEXT_BYTES`]. A code
    /// that does not decode, or with a longer text, is left out.
    pub fn read_codes(&mut self, frame: Frame<'_>) -> Vec<Zeroizing<Vec<u8>>> {
        let Frame {
            data,
            width,
            height,
            stride,
        } = frame;
        let pixels = if stride == width {
            &data[..width * height]
        } else {
            self.tight.clear();
            // Reserved first, so that it does not move and leave copies behind.
            self.tight.reserve(width * height);
            for row in data.chunks(stride).take(height) {
                self.tight.extend_from_slice(&row[..width]);
            }
            &self.tight[..]
        };
        self.quirc
            .identify(width, height, pixels)
            .take(MAX_CODES)
            .filter_map(|code| {
                let payload = Zeroizing::new(code.ok()?.decode().ok()?.payload);
                (payload.len() <= MAX_TEXT_BYTES).then_some(payload)
            })
            .collect()
    }

    /// Looks for a setup code in the frame. A frame with both takes the setup code.
    pub fn scan(&mut self, frame: Frame<'_>) -> Scan {
        let mut result = Scan::NoCode;
        for text in self.read_codes(frame) {
            match setup::parse(&text) {
                Ok(_) => return Scan::Setup(text),
                Err(e) => result = Scan::Refused(e),
            }
        }
        result
    }
}
