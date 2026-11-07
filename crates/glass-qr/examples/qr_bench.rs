//! Time to scan one synthetic frame, and how much a view can take before reading fails. Run in
//! release mode on the machine to measure; prints no secret, because it holds none.

#[path = "../tests/common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use common::*;
use glass_qr::scan::{Frame, Scan, Scanner};
use qrcode::EcLevel;

/// Median, and the slowest, of `runs`.
fn time(frame: &[u8], w: usize, h: usize, runs: usize) -> (Duration, Duration, bool) {
    let mut times = Vec::with_capacity(runs);
    let mut read = false;
    let mut scanner = Scanner::new();
    for _ in 0..runs {
        let f = Frame::new(frame, w, h, w).unwrap();
        let t = Instant::now();
        read = matches!(scanner.scan(f), Scan::Setup(_));
        times.push(t.elapsed());
    }
    times.sort();
    (times[runs / 2], times[runs - 1], read)
}

fn line(label: &str, frame: &[u8], w: usize, h: usize, runs: usize) {
    let (median, worst, read) = time(frame, w, h, runs);
    println!(
        "{label:<44} median {:>9.2} ms  slowest {:>9.2} ms  read {read}",
        median.as_secs_f64() * 1e3,
        worst.as_secs_f64() * 1e3
    );
}

fn main() {
    let text = full_text();
    println!("setup text: {} bytes", text.len());
    let code = matrix(text.as_bytes(), EcLevel::M);
    println!("QR: {} modules a side, level M", code.size);
    let runs = 50;

    println!("\n# time per frame");
    for (w, h, ppm) in [
        (640, 480, 4.0),
        (640, 480, 6.0),
        (1280, 720, 5.0),
        (1280, 720, 10.0),
    ] {
        for (label, noise, angle) in [("clean", 0.0, 0.0), ("noise 10, 20 deg", 10.0, 20.0)] {
            let view = View {
                width: w,
                height: h,
                px_per_module: ppm,
                noise,
                angle_deg: angle,
                ..View::plain()
            };
            let frame = render(&code, &view, &mut Rng::new(1));
            line(
                &format!("{w}x{h} {ppm} px/module, {label}"),
                &frame,
                w,
                h,
                runs,
            );
        }
    }

    println!("\n# time per frame with no setup code");
    let mut rng = Rng::new(2);
    for (w, h) in [(640, 480), (1280, 720)] {
        let plain: Vec<u8> = (0..w * h)
            .map(|i| (100 + ((i % w) / 8 + (i / w) / 8) % 40) as u8)
            .collect();
        line(&format!("{w}x{h} soft scene"), &plain, w, h, runs);
        let noise: Vec<u8> = (0..w * h).map(|_| rng.next() as u8).collect();
        line(&format!("{w}x{h} full random noise"), &noise, w, h, runs);
        for m in [1usize, 2, 3, 5] {
            let f = finder_field(w, h, 7 * m);
            let label = format!("{w}x{h} finder patterns, module {m} px");
            line(&label, &f, w, h, runs);
        }
    }

    // The grid search costs most on large codes, so: as many of the largest as fit.
    println!("\n# time per frame, built to be slow");
    let big = matrix(&[b'x'; 2953], EcLevel::L);
    for (w, h, ppm) in [(640, 480, 2usize), (1280, 720, 2), (1280, 720, 3)] {
        let side = (big.size + 8) * ppm;
        let mut f = vec![255u8; w * h];
        let mut count = 0;
        for oy in 0..h / side {
            for ox in 0..w / side {
                count += 1;
                for y in 0..big.size * ppm {
                    for x in 0..big.size * ppm {
                        if big.dark[(y / ppm) * big.size + x / ppm] {
                            let (py, px) = (oy * side + 4 * ppm + y, ox * side + 4 * ppm + x);
                            f[py * w + px] = 0;
                        }
                    }
                }
            }
        }
        let label = format!("{w}x{h} {count} codes of {} modules, {ppm} px", big.size);
        line(&label, &f, w, h, 10);
    }

    println!("\n# frames read of 20, random angle, in the smallest frame that holds the code");
    println!(
        "{:>9} {:>9} {:>6} {:>6} {:>6} {:>6} {:>6}",
        "px/module", "frame", "n=0", "n=10", "n=20", "n=30", "n=45"
    );
    for ppm in [1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0] {
        // The diagonal of the code with its quiet zone must fit, whatever the angle.
        let (w, h) = if (code.size + 8) as f32 * ppm * 1.42 <= 480.0 {
            (640, 480)
        } else {
            (1280, 720)
        };
        print!("{ppm:>9} {:>9}", format!("{w}x{h}"));
        for noise in [0.0, 10.0, 20.0, 30.0, 45.0] {
            let mut read = 0;
            for seed in 0..20 {
                let mut rng = Rng::new(seed + 100);
                let view = View {
                    width: w,
                    height: h,
                    px_per_module: ppm,
                    noise,
                    angle_deg: 360.0 * rng.unit(),
                    ..View::plain()
                };
                let frame = render(&code, &view, &mut rng);
                let f = Frame::new(&frame, w, h, w).unwrap();
                read += usize::from(matches!(Scanner::new().scan(f), Scan::Setup(_)));
            }
            print!(" {read:>6}");
        }
        println!();
    }

    let (w, h) = (640, 480);
    println!("\n# frames read of 20, dim scene, 640x480, 5 px/module, random angle");
    println!(
        "{:>9} {:>6} {:>6} {:>6} {:>6}",
        "dark-lit", "n=0", "n=2", "n=4", "n=8"
    );
    for (dark, light) in [(4, 60), (4, 40), (4, 24), (4, 16), (2, 10)] {
        print!("{:>9}", format!("{dark}-{light}"));
        for noise in [0.0, 2.0, 4.0, 8.0] {
            let mut read = 0;
            for seed in 0..20 {
                let mut rng = Rng::new(seed + 200);
                let view = View {
                    dark,
                    light,
                    noise,
                    angle_deg: 360.0 * rng.unit(),
                    ..View::plain()
                };
                let frame = render(&code, &view, &mut rng);
                let f = Frame::new(&frame, w, h, w).unwrap();
                read += usize::from(matches!(Scanner::new().scan(f), Scan::Setup(_)));
            }
            print!(" {read:>6}");
        }
        println!();
    }
}
