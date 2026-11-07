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
        assert!(read_with(&view, 3), "{tilt:?}");
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
        assert_eq!(Scanner::new().read_codes(f).len(), 1);
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
        assert_eq!(Scanner::new().read_codes(f).len(), 2);
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
    assert_eq!(Scanner::new().read_codes(f).len(), 2);
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
    assert!(Scanner::new().read_codes(f).is_empty());
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
