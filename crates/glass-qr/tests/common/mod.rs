//! Synthetic camera frames: a QR drawn into a scene, seen at an angle, through a dim and noisy
//! sensor. Deterministic, from a seed. No real image and no real network in here.
#![allow(dead_code)]

use glass_qr::setup::{self, TAG_COMPANION_ADDR, TAG_SEED, TAG_WIFI_AUTH, TAG_WIFI_PASSWORD};
use glass_qr::setup::{TAG_PAIRING_CODE, TAG_WIFI_SSID};
use qrcode::{Color, EcLevel, QrCode};

/// xorshift64*.
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (((self.next() >> 32) * n as u64) >> 32) as usize
    }
    pub fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// About normal, mean 0, deviation 1: a sum of 12 uniforms.
    pub fn normal(&mut self) -> f32 {
        (0..12).map(|_| self.unit()).sum::<f32>() - 6.0
    }
}

/// `true` is a dark module.
pub struct Matrix {
    pub size: usize,
    pub dark: Vec<bool>,
}

pub fn matrix(text: &[u8], level: EcLevel) -> Matrix {
    let code = QrCode::with_error_correction_level(text, level).expect("fits in a QR");
    Matrix {
        size: code.width(),
        dark: code.to_colors().iter().map(|c| *c == Color::Dark).collect(),
    }
}

/// How a code appears in a frame.
#[derive(Clone, Copy)]
pub struct View {
    pub width: usize,
    pub height: usize,
    /// Size of a module, in pixels.
    pub px_per_module: f32,
    pub angle_deg: f32,
    /// Foreshortening along the two axes of the frame: 0.001 is a strong tilt.
    pub tilt: (f32, f32),
    pub dark: u8,
    pub light: u8,
    /// Standard deviation of the sensor noise, in gray levels.
    pub noise: f32,
    /// Added at the right edge of the frame and removed at the left: uneven light.
    pub gradient: f32,
}

impl View {
    pub fn plain() -> View {
        View {
            width: 640,
            height: 480,
            px_per_module: 5.0,
            angle_deg: 0.0,
            tilt: (0.0, 0.0),
            dark: 20,
            light: 220,
            noise: 0.0,
            gradient: 0.0,
        }
    }
}

/// Renders a frame: for each pixel, undoes the view to find what it sees of the code, averaging 4
/// samples per pixel as a lens and a sensor would. The code has the quiet zone the standard asks
/// for, on a mid-gray background.
pub fn render(code: &Matrix, view: &View, rng: &mut Rng) -> Vec<u8> {
    let mut out = vec![0u8; view.width * view.height];
    let (s, c) = view.angle_deg.to_radians().sin_cos();
    let (cx, cy) = (view.width as f32 / 2.0, view.height as f32 / 2.0);
    let half = code.size as f32 / 2.0;
    let module_at = |px: f32, py: f32| -> f32 {
        let (dx, dy) = (px - cx, py - cy);
        let w = 1.0 + view.tilt.0 * dx + view.tilt.1 * dy;
        let (dx, dy) = (dx / w, dy / w);
        let u = (c * dx + s * dy) / view.px_per_module + half;
        let v = (-s * dx + c * dy) / view.px_per_module + half;
        let quiet = 4.0;
        let (lo, hi) = (-quiet, code.size as f32 + quiet);
        if u < lo || v < lo || u >= hi || v >= hi {
            return 0.55;
        }
        if u < 0.0 || v < 0.0 || u >= code.size as f32 || v >= code.size as f32 {
            return 1.0;
        }
        if code.dark[v as usize * code.size + u as usize] {
            0.0
        } else {
            1.0
        }
    };
    let span = f32::from(view.light) - f32::from(view.dark);
    for y in 0..view.height {
        for x in 0..view.width {
            let mut level = 0.0;
            for (ox, oy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                level += module_at(x as f32 + ox, y as f32 + oy);
            }
            let mut v = f32::from(view.dark) + span * level / 4.0;
            v += view.gradient * (x as f32 / view.width as f32 - 0.5);
            if view.noise > 0.0 {
                v += view.noise * rng.normal();
            }
            out[y * view.width + x] = v.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// A frame tiled with QR finder patterns, one module `cell / 7` pixels (at least one).
pub fn finder_field(width: usize, height: usize, cell: usize) -> Vec<u8> {
    let m = (cell / 7).max(1);
    let pitch = 8 * m;
    let mut f = vec![255u8; width * height];
    for y in 0..height {
        for x in 0..width {
            let (u, v) = ((x % pitch) / m, (y % pitch) / m);
            let ring = matches!(u, 0 | 6) || matches!(v, 0 | 6);
            let core = (2..=4).contains(&u) && (2..=4).contains(&v);
            if u < 7 && v < 7 && (ring || core) {
                f[y * width + x] = 0;
            }
        }
    }
    f
}

/// Field `tag` of a body.
pub fn field(body: &mut Vec<u8>, tag: u8, value: &[u8]) {
    body.push(tag);
    body.push(u8::try_from(value.len()).expect("a field fits"));
    body.extend_from_slice(value);
}

pub const TEST_SSID: &str = "Test-Network";
/// Made up for these tests. Not anyone's password.
pub const TEST_PASSWORD: &str = "not-a-real-password";
pub const TEST_CODE: &str = "TEST-pairing_code";
pub const TEST_ADDR: &str = "192.168.1.20:7700";
pub const TEST_SEED: [u8; setup::SEED_BYTES] = *b"not-random-test-seed-32-bytes-ok";

/// A typical full code: a WPA network, the companion's address, a seed and a pairing code.
pub fn full_body() -> Vec<u8> {
    let mut b = Vec::new();
    field(&mut b, TAG_WIFI_SSID, TEST_SSID.as_bytes());
    field(&mut b, TAG_WIFI_AUTH, &[2]);
    field(&mut b, TAG_WIFI_PASSWORD, TEST_PASSWORD.as_bytes());
    field(&mut b, TAG_COMPANION_ADDR, TEST_ADDR.as_bytes());
    field(&mut b, TAG_SEED, &TEST_SEED);
    field(&mut b, TAG_PAIRING_CODE, TEST_CODE.as_bytes());
    b
}

pub fn full_text() -> String {
    setup::seal(&full_body())
}
