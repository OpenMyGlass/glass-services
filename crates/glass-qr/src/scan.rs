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
/// Codes in one frame at most, read or not. A frame with more is refused as [`Scan::Ambiguous`]:
/// codes that cannot be read must not crowd a real one out.
pub const MAX_CODES: usize = 4;
/// The most grids `quircs` reports for a frame; it ignores any more. A frame where it reached this
/// is beyond [`MAX_CODES`] anyway.
pub const DECODER_MAX_GRIDS: usize = 8;
/// The most finder patterns `quircs` keeps (`record_capstone`); it drops any more without a word,
/// so a code below a row of them would go unseen.
pub const DECODER_MAX_CAPSTONES: usize = 32;
/// The most dark regions `quircs` labels (`region_code`), counting its two reserved ones; past it,
/// it finds no more finder patterns.
pub const DECODER_MAX_REGIONS: usize = 65_534;
/// Finder patterns that [`MAX_CODES`] codes, 3 each, and a margin of 8 can explain. The margin is
/// for patterns a scene can make by chance: in the synthetic frames of the tests a code alone gives
/// its 3 and no more, and alignment patterns, of another shape, are not counted. More than this
/// is a frame built to hide something, or too busy to trust; it was chosen, not measured on real
/// scenes, and the Glass scene test should check it.
pub const MAX_CAPSTONES_EXPLAINED: usize = 3 * MAX_CODES + 8;

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
    /// Nothing offered, because the frame may hide a code. A sticker next to the real code must
    /// not win by being read first, nor by crowding the real code out of the decoder. So:
    ///
    /// - two different valid setup codes;
    /// - more than [`MAX_CODES`] codes, read or not;
    /// - [`DECODER_MAX_CAPSTONES`] finder patterns, the most `quircs` keeps, so it may have
    ///   dropped some; or more than [`MAX_CAPSTONES_EXPLAINED`];
    /// - [`DECODER_MAX_REGIONS`] dark regions, the most it labels, so it may have stopped looking.
    ///
    /// Not caught here: a real code that cannot be read (blurred, or a stray finder pattern next to
    /// it taking one of its own into a wrong triple) beside a hostile one that can. [`Session`]
    /// refuses a scan that ever sees two different setup codes, and the offer screen shows all a
    /// code would change, which is then what stops it.
    Ambiguous,
    /// The text of the one valid setup code in view, which [`setup::parse`] accepts.
    Setup(Zeroizing<Vec<u8>>),
}

/// The codes in a frame: what the decoder found, and the texts of those that could be read.
pub struct Codes {
    /// Grids found, read or not, at most [`DECODER_MAX_GRIDS`].
    pub grids: usize,
    /// Finder patterns found, at most [`DECODER_MAX_CAPSTONES`].
    pub capstones: usize,
    /// Dark regions labelled, at most [`DECODER_MAX_REGIONS`].
    pub regions: usize,
    /// Texts of at most [`MAX_TEXT_BYTES`], one per grid that decoded.
    pub texts: Vec<Zeroizing<Vec<u8>>>,
}

/// What a frame's codes add up to.
pub(crate) fn decide(codes: Codes) -> Scan {
    if codes.grids > MAX_CODES
        || codes.capstones >= DECODER_MAX_CAPSTONES
        || codes.capstones > MAX_CAPSTONES_EXPLAINED
        || codes.regions >= DECODER_MAX_REGIONS
    {
        return Scan::Ambiguous;
    }
    let mut result = Scan::NoCode;
    let mut found: Option<Zeroizing<Vec<u8>>> = None;
    for text in codes.texts {
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

    /// Every grid the decoder finds, each decoded: those that do not decode, or with a longer text
    /// than [`MAX_TEXT_BYTES`], are counted in [`Codes::grids`] but have no text.
    pub fn read_codes(&mut self, frame: Frame<'_>) -> Codes {
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
        let mut codes = Codes {
            grids: 0,
            capstones: 0,
            regions: 0,
            texts: Vec::new(),
        };
        for code in self.quirc.identify(width, height, pixels) {
            codes.grids += 1;
            let Some(data) = code.ok().and_then(|c| c.decode().ok()) else {
                continue;
            };
            let payload = Zeroizing::new(data.payload);
            if payload.len() <= MAX_TEXT_BYTES {
                codes.texts.push(payload);
            }
        }
        codes.capstones = self.quirc.num_capstones();
        codes.regions = self.quirc.num_regions();
        codes
    }

    /// Looks for a setup code in the frame. See [`Scan`] for a frame with several codes.
    pub fn scan(&mut self, frame: Frame<'_>) -> Scan {
        decide(self.read_codes(frame))
    }
}

/// Frames that must show the same setup text before it is offered.
pub const FRAMES_TO_AGREE: usize = 3;
/// Frames that may pass between two of those, read or not, before the count starts again.
pub const MAX_FRAMES_BETWEEN: usize = 5;

/// What a scan has come to, so far.
pub enum SessionStep {
    /// Keep scanning: this frame had no setup code, and this is why.
    Scanning(Scan),
    /// Keep scanning: this frame showed the setup text, which this many recent frames agree on.
    Agreeing(usize),
    /// The same setup text in [`FRAMES_TO_AGREE`] frames, and no other ever: offer it. The session
    /// is over.
    Offer(Zeroizing<Vec<u8>>),
    /// Two different setup codes have been seen, in one frame or in two, or the session is over.
    /// End the scan; the user may start another.
    Refused,
}

/// One scan's frames, judged together. A single frame cannot tell a real code that is hard to
/// read from one that is not there, so the scan offers a text only once [`FRAMES_TO_AGREE`] frames
/// have shown it, not more than [`MAX_FRAMES_BETWEEN`] frames apart, and refuses for good as soon
/// as any frame shows another setup code, or is [`Scan::Ambiguous`].
#[derive(Default)]
pub struct Session {
    /// The one setup text seen so far.
    text: Option<Zeroizing<Vec<u8>>>,
    agreeing: usize,
    since_last: usize,
    over: bool,
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }

    /// Takes the next frame's result.
    pub fn feed(&mut self, scan: Scan) -> SessionStep {
        if self.over {
            return SessionStep::Refused;
        }
        match scan {
            Scan::Ambiguous => {
                self.end();
                SessionStep::Refused
            }
            Scan::Setup(text) => {
                match &self.text {
                    Some(seen) if **seen != *text => {
                        self.end();
                        return SessionStep::Refused;
                    }
                    Some(_) if self.since_last <= MAX_FRAMES_BETWEEN => self.agreeing += 1,
                    _ => {
                        self.text = Some(text);
                        self.agreeing = 1;
                    }
                }
                self.since_last = 0;
                if self.agreeing >= FRAMES_TO_AGREE {
                    self.over = true;
                    return SessionStep::Offer(self.text.take().expect("set above"));
                }
                SessionStep::Agreeing(self.agreeing)
            }
            other => {
                self.since_last += 1;
                SessionStep::Scanning(other)
            }
        }
    }

    fn end(&mut self) {
        self.over = true;
        self.text = None;
    }
}
