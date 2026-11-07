//! Generated QR codes through the decoder, from clean to as bad as a dim handheld view, and frames
//! built to be awkward. All pixels here are synthetic: none of this says how the real camera does.

mod common;

use common::*;
use glass_qr::scan::*;
use glass_qr::setup::{self, SetupOffer};
use qrcode::EcLevel;

fn offer_in(frame: &[u8], view: &View) -> Option<SetupOffer> {
    let f = Frame::new(frame, view.width, view.height, view.width).unwrap();
    match Scanner::new().scan(f) {
        Scan::Setup(text) => Some(setup::parse(&text).unwrap().offer()),
        _ => None,
    }
}

fn read_with(view: &View, seed: u64) -> bool {
    let code = matrix(full_text().as_bytes(), EcLevel::M);
    let frame = render(&code, view, &mut Rng::new(seed));
    offer_in(&frame, view).is_some_and(|o| o.wifi_ssid.as_deref() == Some(TEST_SSID))
}

#[test]
fn a_clean_frame_gives_the_offer() {
    assert!(read_with(&View::plain(), 1));
}

#[test]
fn every_angle() {
    for deg in (0..360).step_by(5) {
        let view = View {
            angle_deg: deg as f32,
            ..View::plain()
        };
        assert!(read_with(&view, 1), "{deg} degrees");
    }
}

#[test]
fn every_angle_with_sensor_noise() {
    for deg in (0..360).step_by(15) {
        let view = View {
            angle_deg: deg as f32,
            noise: 15.0,
            ..View::plain()
        };
        assert!(read_with(&view, deg as u64 + 1), "{deg} degrees");
    }
}

#[test]
fn a_tilted_page() {
    for tilt in [
        (0.0006, 0.0),
        (0.0, 0.0006),
        (-0.0006, 0.0004),
        (0.0004, -0.0006),
    ] {
        let view = View {
            tilt,
            angle_deg: 12.0,
            noise: 5.0,
            ..View::plain()
        };
        // One of these views, with one noise seed, gives a fourth finder pattern from the code's own
        // modules, and the rule against stray patterns then refuses that frame. Over a few frames,
        // as a scan sees them, each view is read.
        let read = (3..8).filter(|&seed| read_with(&view, seed)).count();
        assert!(read >= 3, "{tilt:?}: {read} of 5");
    }
}

/// The first camera frames were very dark, so this is not a corner case for this device.
#[test]
fn a_dim_scene() {
    let view = View {
        dark: 4,
        light: 40,
        noise: 3.0,
        angle_deg: 7.0,
        ..View::plain()
    };
    assert!(read_with(&view, 4));
}

#[test]
fn uneven_light() {
    let view = View {
        gradient: 120.0,
        noise: 5.0,
        ..View::plain()
    };
    assert!(read_with(&view, 5));
}

#[test]
fn a_720p_frame() {
    let view = View {
        width: 1280,
        height: 720,
        px_per_module: 8.0,
        angle_deg: 20.0,
        noise: 8.0,
        ..View::plain()
    };
    assert!(read_with(&view, 6));
}

#[test]
fn rows_with_padding() {
    let view = View::plain();
    let frame = render(
        &matrix(full_text().as_bytes(), EcLevel::M),
        &view,
        &mut Rng::new(1),
    );
    let stride = view.width + 32;
    let mut padded = vec![0x55; stride * view.height];
    for y in 0..view.height {
        let row = &frame[y * view.width..(y + 1) * view.width];
        padded[y * stride..y * stride + view.width].copy_from_slice(row);
    }
    // No padding after the last row.
    padded.truncate(stride * (view.height - 1) + view.width);
    let f = Frame::new(&padded, view.width, view.height, stride).unwrap();
    assert!(matches!(Scanner::new().scan(f), Scan::Setup(_)));
}

#[test]
fn another_code_in_view_is_refused_and_does_not_end_the_scan() {
    for text in [
        &b"https://example.com/"[..],
        b"WIFI:S:Net;T:WPA;P:password;;",
    ] {
        let view = View::plain();
        let frame = render(&matrix(text, EcLevel::M), &view, &mut Rng::new(1));
        let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
        assert_eq!(Scanner::new().read_codes(f).texts.len(), 1);
        assert!(matches!(
            Scanner::new().scan(f),
            Scan::Refused(setup::SetupError::NotSetup)
        ));
    }
}

/// Two codes side by side: the setup one wins, whichever the decoder meets first.
#[test]
fn a_setup_code_next_to_a_link() {
    let (width, height) = (1280, 480);
    let half = View {
        width: width / 2,
        height,
        ..View::plain()
    };
    let setup = render(
        &matrix(full_text().as_bytes(), EcLevel::M),
        &half,
        &mut Rng::new(1),
    );
    let link = render(
        &matrix(b"https://example.com/", EcLevel::M),
        &half,
        &mut Rng::new(2),
    );
    for (left, right) in [(&setup, &link), (&link, &setup)] {
        let mut frame = Vec::with_capacity(width * height);
        for y in 0..height {
            frame.extend_from_slice(&left[y * half.width..(y + 1) * half.width]);
            frame.extend_from_slice(&right[y * half.width..(y + 1) * half.width]);
        }
        let f = Frame::new(&frame, width, height, width).unwrap();
        assert_eq!(Scanner::new().read_codes(f).texts.len(), 2);
        assert!(matches!(Scanner::new().scan(f), Scan::Setup(_)));
    }
}

/// Two codes side by side in one frame.
fn side_by_side(left: &[u8], right: &[u8]) -> Scan {
    let (width, height) = (1280, 480);
    let half = View {
        width: width / 2,
        height,
        ..View::plain()
    };
    let l = render(&matrix(left, EcLevel::M), &half, &mut Rng::new(1));
    let r = render(&matrix(right, EcLevel::M), &half, &mut Rng::new(2));
    let mut frame = Vec::with_capacity(width * height);
    for y in 0..height {
        frame.extend_from_slice(&l[y * half.width..(y + 1) * half.width]);
        frame.extend_from_slice(&r[y * half.width..(y + 1) * half.width]);
    }
    let f = Frame::new(&frame, width, height, width).unwrap();
    assert_eq!(Scanner::new().read_codes(f).texts.len(), 2);
    Scanner::new().scan(f)
}

/// A sticker with an attacker's code beside the real one: neither wins, whichever is read first.
#[test]
fn two_different_setup_codes_are_ambiguous() {
    use glass_qr::setup::{TAG_PAIRING_CODE, TAG_WIFI_AUTH, TAG_WIFI_SSID};
    let mut hostile = Vec::new();
    field(&mut hostile, TAG_WIFI_SSID, TEST_SSID.as_bytes());
    field(&mut hostile, TAG_WIFI_AUTH, &[0]);
    field(&mut hostile, TAG_PAIRING_CODE, b"attacker-code");
    let (real, hostile) = (full_text(), setup::seal(&hostile));
    assert!(matches!(
        side_by_side(real.as_bytes(), hostile.as_bytes()),
        Scan::Ambiguous
    ));
    assert!(matches!(
        side_by_side(hostile.as_bytes(), real.as_bytes()),
        Scan::Ambiguous
    ));
}

/// Up to six codes across one 1280x720 frame, three cells by two, each turned a little differently
/// so that finder patterns of neighbours do not line up.
fn grid_of_codes(texts: &[&[u8]]) -> Vec<u8> {
    let (w, h) = (1280, 720);
    let (cw, ch) = (w / 3, h / 2);
    let mut frame = vec![140u8; w * h];
    for (i, text) in texts.iter().enumerate() {
        let view = View {
            width: cw,
            height: ch,
            px_per_module: 3.0,
            angle_deg: [0.0, 30.0, 60.0, 15.0, 45.0, 75.0][i],
            ..View::plain()
        };
        let part = render(
            &matrix(text, EcLevel::L),
            &view,
            &mut Rng::new(i as u64 + 1),
        );
        let (x0, y0) = ((i % 3) * cw, (i / 3) * ch);
        for y in 0..ch {
            frame[(y0 + y) * w + x0..(y0 + y) * w + x0 + cw]
                .copy_from_slice(&part[y * cw..(y + 1) * cw]);
        }
    }
    frame
}

/// A sticker of junk codes and a hostile setup code next to the real one: the junk must not
/// crowd the real code out of the decoder, whatever the order, so the frame is refused.
#[test]
fn junk_codes_cannot_crowd_out_the_real_one() {
    use glass_qr::setup::{TAG_PAIRING_CODE, TAG_WIFI_AUTH, TAG_WIFI_SSID};
    let mut hostile = Vec::new();
    field(&mut hostile, TAG_WIFI_SSID, TEST_SSID.as_bytes());
    field(&mut hostile, TAG_WIFI_AUTH, &[0]);
    field(&mut hostile, TAG_PAIRING_CODE, b"attacker-code");
    let hostile = setup::seal(&hostile);
    let real = full_text();
    // Longer than any setup text: decoded, then dropped.
    let junk = "j".repeat(setup::MAX_TEXT_BYTES + 1);
    let j = junk.as_bytes();
    let (h, r) = (hostile.as_bytes(), real.as_bytes());
    for order in [
        [j, j, j, h, r],
        [r, j, j, j, h],
        [h, j, r, j, j],
        [j, h, j, j, r],
    ] {
        let frame = grid_of_codes(&order);
        let f = Frame::new(&frame, 1280, 720, 1280).unwrap();
        let codes = Scanner::new().read_codes(f);
        // The decoder sees all five, and reads both setup codes: the junk only fills slots.
        assert_eq!(codes.grids, 5);
        assert_eq!(codes.texts.len(), 2);
        assert!(matches!(Scanner::new().scan(f), Scan::Ambiguous));
    }
    // Even junk alone beside the real code: a grid that gives no text could be a code hidden, so
    // the frame is refused.
    let frame = grid_of_codes(&[j, r, j]);
    let f = Frame::new(&frame, 1280, 720, 1280).unwrap();
    assert!(matches!(Scanner::new().scan(f), Scan::Ambiguous));
}

#[test]
fn the_same_setup_code_twice_is_not_ambiguous() {
    let real = full_text();
    assert!(matches!(
        side_by_side(real.as_bytes(), real.as_bytes()),
        Scan::Setup(_)
    ));
}

#[test]
fn a_code_longer_than_any_setup_is_dropped_unread() {
    let text = "x".repeat(setup::MAX_TEXT_BYTES + 1);
    let view = View {
        px_per_module: 6.0,
        ..View::plain()
    };
    let frame = render(
        &matrix(text.as_bytes(), EcLevel::L),
        &view,
        &mut Rng::new(1),
    );
    let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
    assert!(Scanner::new().read_codes(f).texts.is_empty());
    assert!(matches!(Scanner::new().scan(f), Scan::NoCode));
}

#[test]
fn a_code_that_is_not_utf8_is_still_only_refused() {
    let view = View::plain();
    let frame = render(
        &matrix(&[0xff, 0xfe, 0x00, 0x80], EcLevel::M),
        &view,
        &mut Rng::new(1),
    );
    let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
    assert!(matches!(
        Scanner::new().scan(f),
        Scan::Refused(setup::SetupError::NotSetup)
    ));
}

/// A clean, perfectly readable QR can still say the wrong thing.
#[test]
fn a_readable_code_with_a_bad_text_is_refused() {
    use glass_qr::setup::{SetupError, TAG_WIFI_AUTH, TAG_WIFI_SSID};
    let text = full_text();
    let bad_crc = format!("{}:00000000", text.rsplit_once(':').unwrap().0);
    // A secured network with no password: it must not turn into joining it as open.
    let mut no_password = Vec::new();
    field(&mut no_password, TAG_WIFI_SSID, b"Net");
    field(&mut no_password, TAG_WIFI_AUTH, &[2]);
    let cases = [
        (bad_crc, SetupError::Checksum),
        (
            setup::seal(&no_password),
            SetupError::Inconsistent(TAG_WIFI_AUTH),
        ),
        (setup::seal(&[]), SetupError::Empty),
        (
            "omg-setup:2:AA:00000000".into(),
            SetupError::UnsupportedVersion,
        ),
    ];
    for (text, want) in cases {
        let view = View::plain();
        let frame = render(
            &matrix(text.as_bytes(), EcLevel::M),
            &view,
            &mut Rng::new(1),
        );
        let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
        match Scanner::new().scan(f) {
            Scan::Refused(e) => assert_eq!(e, want),
            _ => panic!("not refused as {want:?}"),
        }
    }
}

#[test]
fn printing_a_frame_shows_no_pixels() {
    let data = vec![0x42u8; 64 * 64];
    assert_eq!(
        format!("{:?}", Frame::new(&data, 64, 64, 64).unwrap()),
        "Frame(64x64)"
    );
}

#[test]
fn bad_frame_buffers_are_refused() {
    let data = vec![0u8; 640 * 480];
    assert!(Frame::new(&data, 640, 480, 640).is_ok());
    assert_eq!(
        Frame::new(&data, 0, 480, 640).unwrap_err(),
        FrameError::Size
    );
    assert_eq!(
        Frame::new(&data, 640, 0, 640).unwrap_err(),
        FrameError::Size
    );
    assert_eq!(
        Frame::new(&data, 20, 480, 640).unwrap_err(),
        FrameError::Size
    );
    assert_eq!(
        Frame::new(&data, 1281, 720, 1281).unwrap_err(),
        FrameError::Size
    );
    assert_eq!(
        Frame::new(&data, usize::MAX, usize::MAX, usize::MAX).unwrap_err(),
        FrameError::Size
    );
    assert_eq!(
        Frame::new(&data, 640, 480, 639).unwrap_err(),
        FrameError::Stride
    );
    assert_eq!(
        Frame::new(&data[..640 * 480 - 1], 640, 480, 640).unwrap_err(),
        FrameError::ShortBuffer
    );
    assert_eq!(
        Frame::new(&data, 640, 480, usize::MAX).unwrap_err(),
        FrameError::ShortBuffer
    );
}

/// Finder patterns everywhere, or nothing but edges: the frames that give the decoder most to do.
pub fn awkward_frames(width: usize, height: usize, rng: &mut Rng) -> Vec<Vec<u8>> {
    let mut out = vec![vec![0u8; width * height], vec![255u8; width * height]];
    out.push((0..width * height).map(|_| rng.next() as u8).collect());
    out.push(
        (0..width * height)
            .map(|i| {
                if (i % width + i / width) & 1 == 0 {
                    0
                } else {
                    255
                }
            })
            .collect(),
    );
    for cell in [7usize, 9, 14, 21, 35] {
        out.push(finder_field(width, height, cell));
    }
    out
}

#[test]
fn awkward_frames_do_not_panic_or_take_long() {
    let mut rng = Rng::new(7);
    let mut scanner = Scanner::new();
    for (w, h) in [(320, 240), (640, 480), (1280, 720)] {
        for frame in awkward_frames(w, h, &mut rng) {
            let start = std::time::Instant::now();
            let f = Frame::new(&frame, w, h, w).unwrap();
            assert!(!matches!(scanner.scan(f), Scan::Setup(_)));
            // Generous, for an unoptimized build on a busy machine. These frames do not reach the
            // slow case in the module doc of `scan`: that takes frames like the ones in `stress`.
            let took = start.elapsed();
            assert!(took.as_secs() < 3, "{w}x{h}: {took:?}");
        }
    }
}

/// Damage to a real code: runs of pixels overwritten. Most of these must simply not decode. If one
/// does it has passed the Reed-Solomon code and the checksum, so it can only be the same text.
#[test]
fn damaged_codes_do_not_panic_or_give_another_text() {
    let view = View {
        width: 320,
        height: 240,
        px_per_module: 3.0,
        ..View::plain()
    };
    let text = full_text();
    let clean = render(
        &matrix(text.as_bytes(), EcLevel::L),
        &view,
        &mut Rng::new(1),
    );
    let mut rng = Rng::new(9);
    for _ in 0..300 {
        let mut frame = clean.clone();
        for _ in 0..=rng.below(40) {
            let (x, y) = (rng.below(320 - 30), rng.below(240 - 12));
            let v = rng.next() as u8;
            for row in 0..=rng.below(12) {
                frame[(y + row) * 320 + x..(y + row) * 320 + x + rng.below(30)].fill(v);
            }
        }
        let f = Frame::new(&frame, 320, 240, 320).unwrap();
        if let Scan::Setup(t) = Scanner::new().scan(f) {
            assert_eq!(&t[..], text.as_bytes());
        }
    }
}

/// Frames that once took up to 5 s each, from the alignment search fixed in vendor/quircs.
#[test]
fn slow_frames() {
    let mut scanner = Scanner::new();
    for seed in SLOW_SEEDS {
        let frame = stress_frame(seed, 320, 240);
        let start = std::time::Instant::now();
        scanner.scan(Frame::new(&frame, 320, 240, 320).unwrap());
        let took = start.elapsed();
        // Unoptimized and on a busy machine: still far below what these took optimized before.
        assert!(took.as_millis() < 500, "seed {seed}: {took:?}");
    }
}

/// A long search for an image that makes the decoder panic, or a slow one, which it prints. Off by
/// default; run with `cargo test --release -p glass-qr --test scan -- --ignored --nocapture stress`,
/// with `QR_STRESS` set to the number of frames.
#[test]
#[ignore]
fn stress() {
    let n: u64 = std::env::var("QR_STRESS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let (w, h) = (320, 240);
    let mut slowest = std::time::Duration::ZERO;
    let mut decoded = 0u64;
    for seed in 0..n {
        let frame = stress_frame(seed, w, h);
        let f = Frame::new(&frame, w, h, w).unwrap();
        let t = std::time::Instant::now();
        let hit = std::panic::catch_unwind(|| matches!(Scanner::new().scan(f), Scan::Setup(_)));
        let took = t.elapsed();
        match hit {
            Ok(true) => decoded += 1,
            Ok(false) => {}
            Err(_) => panic!("decoder panic at seed {seed}"),
        }
        slowest = slowest.max(took);
        // Timed again at once, to tell a slow frame from a busy machine.
        if took.as_millis() > 100 {
            let t = std::time::Instant::now();
            Scanner::new().scan(f);
            println!("seed {seed}: {took:?}, again {:?}", t.elapsed());
        }
    }
    println!("{n} frames, {decoded} read, slowest {slowest:?}");
}

const W: usize = 1280;
const H: usize = 720;

fn paste(frame: &mut [u8], part: &[u8], pw: usize, ph: usize, x0: usize, y0: usize) {
    for y in 0..ph {
        frame[(y0 + y) * W + x0..(y0 + y) * W + x0 + pw]
            .copy_from_slice(&part[y * pw..(y + 1) * pw]);
    }
}

/// One lone finder pattern, of module `m`, on a light square, at (x0, y0).
fn lone_finder(frame: &mut [u8], x0: usize, y0: usize, m: usize) {
    for v in 0..9 * m {
        frame[(y0 + v) * W + x0..(y0 + v) * W + x0 + 9 * m].fill(220);
    }
    for v in 0..7 {
        for u in 0..7 {
            let ring = matches!(u, 0 | 6) || matches!(v, 0 | 6);
            let core = (2..=4).contains(&u) && (2..=4).contains(&v);
            if ring || core {
                for dy in 0..m {
                    let row = (y0 + m + v * m + dy) * W + x0 + m + u * m;
                    frame[row..row + m].fill(20);
                }
            }
        }
    }
}

fn hostile_text() -> String {
    use glass_qr::setup::{TAG_PAIRING_CODE, TAG_WIFI_AUTH, TAG_WIFI_SSID};
    let mut hostile = Vec::new();
    field(&mut hostile, TAG_WIFI_SSID, TEST_SSID.as_bytes());
    field(&mut hostile, TAG_WIFI_AUTH, &[0]);
    field(&mut hostile, TAG_PAIRING_CODE, b"attacker-code");
    setup::seal(&hostile)
}

/// The hostile code at the top left, the real one at the bottom right, and a band in between.
fn hostile_band_real(band: impl Fn(&mut [u8])) -> Vec<u8> {
    let mut frame = vec![140u8; W * H];
    let v = View {
        width: 600,
        height: 300,
        px_per_module: 3.0,
        ..View::plain()
    };
    let hostile = render(
        &matrix(hostile_text().as_bytes(), EcLevel::L),
        &v,
        &mut Rng::new(1),
    );
    let real = render(
        &matrix(full_text().as_bytes(), EcLevel::L),
        &v,
        &mut Rng::new(2),
    );
    paste(&mut frame, &hostile, 600, 300, 0, 0);
    paste(&mut frame, &real, 600, 300, 660, 410);
    band(&mut frame);
    frame
}

fn outcome(frame: &[u8]) -> &'static str {
    let f = Frame::new(frame, W, H, W).unwrap();
    match Scanner::new().scan(f) {
        Scan::Setup(t) if t[..] == *full_text().as_bytes() => "real",
        Scan::Setup(_) => "HOSTILE",
        Scan::Ambiguous => "ambiguous",
        Scan::NoCode => "nocode",
        Scan::Refused(_) => "refused",
    }
}

/// A row of lone finder patterns between the two codes: `quircs` keeps 32 finder patterns and drops
/// the rest, so the real code below the row would go unseen and the hostile one win. From the
/// review's proof of concept.
#[test]
fn a_row_of_finder_patterns_cannot_hide_the_real_code() {
    let both = hostile_band_real(|_| {});
    assert_eq!(outcome(&both), "ambiguous", "both codes seen");
    for lone in [20, 26, 29, 31] {
        let frame = hostile_band_real(|f| {
            for i in 0..lone {
                lone_finder(f, 10 + i * 40, 330, 4);
            }
        });
        assert_eq!(outcome(&frame), "ambiguous", "{lone} lone finder patterns");
    }
}

/// A band of tiny 1:1:3:1:1 runs, each a candidate finder pattern in dark regions of its own, uses
/// up `quircs`'s region labels, after which it finds no more finder patterns. On this frame the
/// commit before this rule read neither code; the rule is that a frame at the cap is refused.
#[test]
fn a_frame_that_uses_up_the_region_labels_is_refused() {
    let mut frame = vec![140u8; W * H];
    let v = View {
        width: 400,
        height: 130,
        px_per_module: 2.0,
        ..View::plain()
    };
    let hostile = render(
        &matrix(hostile_text().as_bytes(), EcLevel::L),
        &v,
        &mut Rng::new(1),
    );
    let real = render(
        &matrix(full_text().as_bytes(), EcLevel::L),
        &v,
        &mut Rng::new(2),
    );
    paste(&mut frame, &hostile, 400, 130, 0, 0);
    paste(&mut frame, &real, 400, 130, 800, 590);
    let f = Frame::new(&frame, W, H, W).unwrap();
    assert_eq!(
        Scanner::new().read_codes(f).texts.len(),
        2,
        "both readable alone"
    );
    for y in (140..580).step_by(2) {
        let row = &mut frame[y * W..(y + 1) * W];
        row.fill(220);
        for x0 in (0..W - 8).step_by(8) {
            // dark 1, light 1, dark 3, light 1, dark 1, light 1
            for (dx, dark) in [1, 0, 1, 1, 1, 0, 1, 0].into_iter().enumerate() {
                if dark == 1 {
                    row[x0 + dx] = 20;
                }
            }
        }
    }
    let f = Frame::new(&frame, W, H, W).unwrap();
    let codes = Scanner::new().read_codes(f);
    assert_eq!(
        codes.regions,
        glass_qr::scan::DECODER_MAX_REGIONS,
        "labels used up"
    );
    assert_eq!(outcome(&frame), "ambiguous");
}

fn setup_scan(text: &str) -> Scan {
    Scan::Setup(zeroize::Zeroizing::new(text.as_bytes().to_vec()))
}

#[test]
fn a_session_offers_after_three_agreeing_frames() {
    let mut s = Session::new();
    let real = full_text();
    assert!(matches!(
        s.feed(setup_scan(&real)),
        SessionStep::Agreeing(1)
    ));
    assert!(matches!(
        s.feed(Scan::NoCode),
        SessionStep::Scanning(Scan::NoCode)
    ));
    assert!(matches!(
        s.feed(setup_scan(&real)),
        SessionStep::Agreeing(2)
    ));
    match s.feed(setup_scan(&real)) {
        SessionStep::Offer(t) => assert_eq!(&t[..], real.as_bytes()),
        _ => panic!("no offer"),
    }
    // Over once offered.
    assert!(matches!(s.feed(setup_scan(&real)), SessionStep::Refused));
}

#[test]
fn a_session_starts_counting_again_after_a_long_gap() {
    let mut s = Session::new();
    let real = full_text();
    s.feed(setup_scan(&real));
    s.feed(setup_scan(&real));
    for _ in 0..=MAX_FRAMES_BETWEEN {
        s.feed(Scan::NoCode);
    }
    assert!(matches!(
        s.feed(setup_scan(&real)),
        SessionStep::Agreeing(1)
    ));
}

/// The real code in one frame and a hostile one in a later frame, or the other way round: the scan
/// is refused for good, even if one of them then shows alone.
#[test]
fn a_session_that_ever_sees_two_setup_codes_is_refused() {
    let (real, hostile) = (full_text(), hostile_text());
    for (first, then) in [(&real, &hostile), (&hostile, &real)] {
        let mut s = Session::new();
        assert!(matches!(
            s.feed(setup_scan(first)),
            SessionStep::Agreeing(1)
        ));
        assert!(matches!(s.feed(Scan::NoCode), SessionStep::Scanning(_)));
        assert!(matches!(s.feed(setup_scan(then)), SessionStep::Refused));
        for _ in 0..FRAMES_TO_AGREE {
            assert!(matches!(s.feed(setup_scan(first)), SessionStep::Refused));
        }
    }
    let mut s = Session::new();
    s.feed(setup_scan(&real));
    assert!(matches!(s.feed(Scan::Ambiguous), SessionStep::Refused));
    assert!(matches!(s.feed(setup_scan(&real)), SessionStep::Refused));
}

/// One stray finder pattern up and left of the real code's top-left corner: `quircs` puts it in a
/// triple with two of the real code's, and the real code is lost while the hostile one is read.
/// These are the positions where the review's proof of concept got the hostile code offered, every
/// one with 2 grids, 1 text and 7 finder patterns.
#[test]
fn a_stray_finder_pattern_cannot_steal_the_real_code() {
    for (x, y) in [
        (854, 455),
        (857, 455),
        (860, 455),
        (863, 455),
        (854, 458),
        (857, 458),
        (857, 461),
        (857, 464),
        (857, 467),
        (857, 470),
        (857, 473),
    ] {
        let frame = hostile_band_real(|f| lone_finder(f, x, y, 3));
        let f = Frame::new(&frame, W, H, W).unwrap();
        let codes = Scanner::new().read_codes(f);
        assert_eq!(
            (codes.grids, codes.texts.len(), codes.capstones),
            (2, 1, 7),
            "the stray at ({x},{y}) still steals"
        );
        assert_eq!(outcome(&frame), "ambiguous", "stray at ({x},{y})");
    }
}

/// A frame whose finder patterns and grids all add up still gives its code.
#[test]
fn a_frame_where_everything_adds_up_gives_the_code() {
    let (frame, view) = {
        let view = View::plain();
        let code = matrix(full_text().as_bytes(), EcLevel::M);
        (render(&code, &view, &mut Rng::new(1)), view)
    };
    let f = Frame::new(&frame, view.width, view.height, view.width).unwrap();
    let codes = Scanner::new().read_codes(f);
    assert_eq!((codes.grids, codes.texts.len(), codes.capstones), (1, 1, 3));
    assert!(matches!(Scanner::new().scan(f), Scan::Setup(_)));
}
