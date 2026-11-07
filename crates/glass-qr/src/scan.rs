//! Finding QR codes in a grayscale frame, with `quircs`.
//!
//! **Why not `rqrr`:** it was tried first. It copies the whole frame for each candidate triple of
//! finder patterns, so a 640x480 frame tiled with them took over 15 s on the host, and it read less
//! in a dim scene. `quircs` holds at most 32 finder patterns and 8 grids, and read more in the dim
//! and noisy cases tried. It was weaker only below 3 pixels per module.
//!
//! **Time:** `quircs` is vendored with one fix (`vendor/quircs/VENDORED.md`): its search for the
//! alignment pattern ran about 100 steps per pixel of a module's estimated area, which wrong finder
//! patterns could make far larger than the frame. Before the fix, 10 of 30000 random synthetic
//! frames of 320x240 took 0.1 to 5 s on the host; the `slow_frames` test holds them. Nothing proves
//! there is no other slow case, and `quircs` cannot be interrupted.
//!
//! **A panic here ends the process.** The frame comes from the world, `quircs` has assertions, and
//! the release profile aborts on a panic. None was found in release builds (see the `stress` test
//! in `tests/scan.rs`), which does not prove there is none.
//!
//! So the camera process runs this through [`crate::worker`], in a process of its own that it kills
//! at a deadline, and glassd builds this crate without the `decoder` feature.
//!
//! The text of a code is wiped when dropped, as a best effort. Not wiped: the frame, which belongs to
//! the caller; the black-and-white copy `quircs` keeps of the last frame until the next one or its
//! end; what `quircs` left in memory it has freed. The worker's process ending frees all of them.

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
    pub(crate) width: usize,
    pub(crate) height: usize,
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

/// Whether a frame of this size is taken, before any buffer is made for it.
pub fn check_size(width: usize, height: usize) -> Result<(), FrameError> {
    if width < MIN_SIDE
        || height < MIN_SIDE
        || width.checked_mul(height).is_none_or(|n| n > MAX_PIXELS)
    {
        return Err(FrameError::Size);
    }
    Ok(())
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
        check_size(width, height)?;
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

    /// The rows, without their padding.
    pub(crate) fn rows(&self) -> impl Iterator<Item = &'a [u8]> {
        let width = self.width;
        self.data
            .chunks(self.stride)
            .take(self.height)
            .map(move |row| &row[..width])
    }
}

/// What a frame held.
pub enum Scan {
    /// No code that could be read.
    NoCode,
    /// Codes that are not valid setup codes, and the reason for the last one. Keep scanning: a
    /// link on a poster in view must not end the scan.
    Refused(SetupError),
    /// Two different valid setup codes. Neither is offered: a sticker next to the real code must
    /// not be able to win by being read first. Keep scanning, and say why nothing is offered.
    Ambiguous,
    /// The text of the one valid setup code in view, which [`setup::parse`] accepts.
    Setup(Zeroizing<Vec<u8>>),
}

/// What a frame's codes add up to.
pub(crate) fn decide(texts: Vec<Zeroizing<Vec<u8>>>) -> Scan {
    let mut result = Scan::NoCode;
    let mut found: Option<Zeroizing<Vec<u8>>> = None;
    for text in texts {
        match setup::parse(&text) {
            Ok(_) => match &found {
                Some(first) if **first != *text => return Scan::Ambiguous,
                Some(_) => {}
                None => found = Some(text),
            },
            Err(e) => result = Scan::Refused(e),
        }
    }
    found.map_or(result, Scan::Setup)
}

/// Reads frames, one at a time, in this process. It keeps its buffers from one frame to the next,
/// so make one per scanning session rather than one per frame.
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
        let (width, height) = (frame.width, frame.height);
        let pixels = if frame.stride == width {
            &frame.data[..width * height]
        } else {
            let n = width * height;
            if self.tight.capacity() < n {
                // A new buffer rather than a larger one, so that the old one is wiped as it goes
                // instead of left behind by a reallocation.
                self.tight = Zeroizing::new(Vec::with_capacity(n));
            }
            self.tight.clear();
            for row in frame.rows() {
                self.tight.extend_from_slice(row);
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

    /// Looks for a setup code in the frame. See [`Scan`] for a frame with several codes.
    pub fn scan(&mut self, frame: Frame<'_>) -> Scan {
        decide(self.read_codes(frame))
    }
}
