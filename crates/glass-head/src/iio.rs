//! IIO access to the MPU-9150 (docs/specs/kernel-interfaces.md §3.2-3.4).
//!
//! - The device is the IIO device whose `name` is `mpu9150` under `/sys/bus/iio/devices`;
//!   its samples are read from `/dev/iio:deviceN`.
//! - Records follow the IIO buffer rules: the enabled scan elements in index order, each
//!   aligned to its storage size, then the 64-bit timestamp aligned to 8 bytes. The Glass
//!   driver adds a *format byte* (`0x02` gyro, `0x04` accel, ...) in the last padding byte
//!   before the timestamp; [`Layout`] finds it and [`Layout::decode`] returns it.
//! - Raw counts are signed 16-bit over the full scale; `in_anglvel_scale` and
//!   `in_accel_scale` read back the full scale in degrees per second and in g (not a factor
//!   per count). The driver does not rotate: `gyro_matrix` and `accl_matrix` are applied
//!   here, `out = M · chip`.
//! - Only the attributes in [`WRITABLE`] are written: power, rate, the sensor enables, the
//!   scan elements, the buffer enable and the trigger. Never the DMP firmware, self test,
//!   registers or the look-up tuning.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Where IIO devices are listed.
pub const DEVICES_ROOT: &str = "/sys/bus/iio/devices";
/// The IIO name of the IMU.
pub const IMU_NAME: &str = "mpu9150";
/// Sampling rate with the gyroscope, Hz: 20 ms between samples, as the stock software
/// detectors poll.
pub const RATE_HZ: u32 = 50;
/// Sampling rate with the accelerometer alone (look-up while the screen is off), Hz. Every
/// sample is an interrupt that wakes the CPU (the stock look-up runs in the DMP firmware
/// instead, which is not available), so the look-up uses the lowest rate the driver offers.
pub const LOOK_UP_RATE_HZ: u32 = 10;

/// Attributes (relative to the device directory) this module writes. Scan element enables
/// (`scan_elements/*_en`) are allowed too.
pub const WRITABLE: &[&str] = &[
    "power_state",
    "sampling_frequency",
    "gyro_enable",
    "accl_enable",
    "compass_enable",
    "buffer/enable",
    "trigger/current_trigger",
];

fn allowed(attr: &str) -> bool {
    WRITABLE.contains(&attr)
        || (attr.starts_with("scan_elements/")
            && attr.ends_with("_en")
            && !attr[14..].contains('/'))
}

/// A scan element's `_type`, e.g. `le:s16/16>>0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanType {
    pub big_endian: bool,
    pub signed: bool,
    pub realbits: u32,
    pub storagebits: u32,
    pub shift: u32,
}

impl ScanType {
    pub fn parse(s: &str) -> Option<ScanType> {
        let s = s.trim();
        let (endian, rest) = s.split_once(':')?;
        let big_endian = match endian {
            "be" => true,
            "le" => false,
            _ => return None,
        };
        let signed = match rest.chars().next()? {
            's' => true,
            'u' => false,
            _ => return None,
        };
        let (bits, shift) = rest[1..].split_once(">>")?;
        let (real, storage) = bits.split_once('/')?;
        let t = ScanType {
            big_endian,
            signed,
            realbits: real.parse().ok()?,
            storagebits: storage.parse().ok()?,
            shift: shift.parse().ok()?,
        };
        let ok = matches!(t.storagebits, 8 | 16 | 32 | 64)
            && t.realbits >= 1
            && t.realbits + t.shift <= t.storagebits;
        ok.then_some(t)
    }

    fn bytes(&self) -> usize {
        self.storagebits as usize / 8
    }

    fn decode(&self, b: &[u8]) -> i64 {
        let mut v: u64 = 0;
        for i in 0..b.len() {
            let byte = if self.big_endian {
                b[i]
            } else {
                b[b.len() - 1 - i]
            };
            v = (v << 8) | u64::from(byte);
        }
        v >>= self.shift;
        let bits = self.realbits;
        if bits < 64 {
            v &= (1u64 << bits) - 1;
            if self.signed && v & (1u64 << (bits - 1)) != 0 {
                return (v | !((1u64 << bits) - 1)) as i64;
            }
        }
        v as i64
    }
}

/// One enabled scan element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Name without the `_en` / `_index` / `_type` suffix, e.g. `in_accel_x`.
    pub name: String,
    pub index: u32,
    pub ty: ScanType,
}

pub const TIMESTAMP: &str = "in_timestamp";
pub const GYRO: [&str; 3] = ["in_anglvel_x", "in_anglvel_y", "in_anglvel_z"];
pub const ACCEL: [&str; 3] = ["in_accel_x", "in_accel_y", "in_accel_z"];

/// Format byte bits (Glass driver).
pub const FORMAT_GYRO: u8 = 0x02;
pub const FORMAT_ACCEL: u8 = 0x04;

/// The record layout for a set of enabled elements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Data elements with their byte offset, in scan order.
    pub fields: Vec<(Element, usize)>,
    pub timestamp: Option<(Element, usize)>,
    /// Offset of the format byte, when there is padding before the timestamp for it.
    pub format_byte: Option<usize>,
    /// Record size in bytes.
    pub size: usize,
}

/// A decoded record.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// Raw values by element name.
    pub values: Vec<(String, i64)>,
    pub timestamp_ns: Option<i64>,
    pub format: Option<u8>,
}

impl Record {
    pub fn get(&self, name: &str) -> Option<i64> {
        self.values.iter().find(|(n, _)| n == name).map(|&(_, v)| v)
    }
}

fn align(x: usize, a: usize) -> usize {
    x.div_ceil(a) * a
}

impl Layout {
    pub fn new(mut els: Vec<Element>) -> Layout {
        els.sort_by_key(|e| e.index);
        let mut off = 0;
        let mut fields = Vec::new();
        let mut timestamp = None;
        for e in els {
            if e.name == TIMESTAMP {
                timestamp = Some(e);
                continue;
            }
            off = align(off, e.ty.bytes());
            let n = e.ty.bytes();
            fields.push((e, off));
            off += n;
        }
        let data_end = off;
        let mut format_byte = None;
        let timestamp = timestamp.map(|e| {
            let at = align(off, e.ty.bytes());
            if at > data_end && data_end > 0 {
                format_byte = Some(at - 1);
            }
            off = at + e.ty.bytes();
            (e, at)
        });
        Layout {
            fields,
            timestamp,
            format_byte,
            size: off,
        }
    }

    /// The layout of the enabled elements in a `scan_elements` directory.
    pub fn read_dir(dir: impl AsRef<Path>) -> io::Result<Layout> {
        let dir = dir.as_ref();
        let mut els = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file = entry.file_name();
            let Some(name) = file.to_str().and_then(|n| n.strip_suffix("_en")) else {
                continue;
            };
            if fs::read_to_string(entry.path())?.trim() != "1" {
                continue;
            }
            let index = fs::read_to_string(dir.join(format!("{name}_index")))?
                .trim()
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad scan index"))?;
            let ty = ScanType::parse(&fs::read_to_string(dir.join(format!("{name}_type")))?)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad scan type"))?;
            els.push(Element {
                name: name.to_string(),
                index,
                ty,
            });
        }
        Ok(Layout::new(els))
    }

    /// Decodes one record of [`size`](Self::size) bytes.
    pub fn decode(&self, rec: &[u8]) -> Record {
        let field = |e: &Element, off: usize| e.ty.decode(&rec[off..off + e.ty.bytes()]);
        Record {
            values: self
                .fields
                .iter()
                .map(|(e, off)| (e.name.clone(), field(e, *off)))
                .collect(),
            timestamp_ns: self.timestamp.as_ref().map(|(e, off)| field(e, *off)),
            format: self.format_byte.map(|o| rec[o]),
        }
    }
}

/// A 3x3 matrix, row-major.
pub type Matrix = [[f32; 3]; 3];

/// The board's gyro and accel matrix (kernel-interfaces.md §3.4).
pub const BOARD_MATRIX: Matrix = [[0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]];

/// Parses `"a,b,c,d,e,f,g,h,i"`.
pub fn parse_matrix(s: &str) -> Option<Matrix> {
    let v: Vec<f32> = s
        .trim()
        .split(',')
        .map(|x| x.trim().parse::<f32>().ok())
        .collect::<Option<_>>()?;
    (v.len() == 9).then(|| [[v[0], v[1], v[2]], [v[3], v[4], v[5]], [v[6], v[7], v[8]]])
}

pub fn apply(m: &Matrix, v: [f32; 3]) -> [f32; 3] {
    let row = |r: &[f32; 3]| r[0] * v[0] + r[1] * v[1] + r[2] * v[2];
    [row(&m[0]), row(&m[1]), row(&m[2])]
}

/// Scales and orientation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    /// Gyro full scale, degrees per second (default 2000).
    pub gyro_full_dps: f32,
    /// Accel full scale, g (default 2).
    pub accel_full_g: f32,
    pub gyro_matrix: Matrix,
    pub accel_matrix: Matrix,
}

impl Default for Calibration {
    fn default() -> Self {
        Calibration {
            gyro_full_dps: 2000.0,
            accel_full_g: 2.0,
            gyro_matrix: BOARD_MATRIX,
            accel_matrix: BOARD_MATRIX,
        }
    }
}

/// One IMU sample in the device frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Sensor time, ns (the driver's clock; only differences are used).
    pub t_ns: i64,
    /// rad/s.
    pub gyro: Option<[f32; 3]>,
    /// g.
    pub accel: Option<[f32; 3]>,
}

impl Calibration {
    /// Reads the scales and matrices from the device directory; missing values keep the
    /// defaults.
    pub fn read(dir: &Path) -> Calibration {
        let mut c = Calibration::default();
        let num = |f: &str| {
            fs::read_to_string(dir.join(f))
                .ok()
                .and_then(|s| s.trim().parse::<f32>().ok())
                .filter(|v| *v > 0.0)
        };
        let mat = |f: &str| {
            fs::read_to_string(dir.join(f))
                .ok()
                .and_then(|s| parse_matrix(&s))
        };
        if let Some(v) = num("in_anglvel_scale") {
            c.gyro_full_dps = v;
        }
        if let Some(v) = num("in_accel_scale") {
            c.accel_full_g = v;
        }
        if let Some(m) = mat("gyro_matrix") {
            c.gyro_matrix = m;
        }
        if let Some(m) = mat("accl_matrix") {
            c.accel_matrix = m;
        }
        c
    }

    /// Converts a record. A channel is missing if its elements are not all in the record,
    /// or if the record has a format byte without that channel's bit.
    pub fn sample(&self, r: &Record, fallback_t_ns: i64) -> Sample {
        let three = |names: [&str; 3], bit: u8| -> Option<[f32; 3]> {
            if r.format.is_some_and(|f| f & bit == 0) {
                return None;
            }
            Some([
                r.get(names[0])? as f32,
                r.get(names[1])? as f32,
                r.get(names[2])? as f32,
            ])
        };
        let gyro = three(GYRO, FORMAT_GYRO).map(|v| {
            let k = self.gyro_full_dps / 32768.0 * std::f32::consts::PI / 180.0;
            apply(&self.gyro_matrix, [v[0] * k, v[1] * k, v[2] * k])
        });
        let accel = three(ACCEL, FORMAT_ACCEL).map(|v| {
            let k = self.accel_full_g / 32768.0;
            apply(&self.accel_matrix, [v[0] * k, v[1] * k, v[2] * k])
        });
        Sample {
            t_ns: r.timestamp_ns.unwrap_or(fallback_t_ns),
            gyro,
            accel,
        }
    }
}

/// Which sensors stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Channels {
    pub accel: bool,
    pub gyro: bool,
}

impl Channels {
    pub fn any(&self) -> bool {
        self.accel || self.gyro
    }
}

/// Finds the IIO device directory named `name` under `root`.
pub fn find(root: impl AsRef<Path>, name: &str) -> io::Result<Option<PathBuf>> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("iio:device"))
        })
        .collect();
    dirs.sort();
    Ok(dirs
        .into_iter()
        .find(|d| fs::read_to_string(d.join("name")).is_ok_and(|n| n.trim() == name)))
}

struct Stream {
    file: File,
    layout: Layout,
    partial: Vec<u8>,
}

/// The IMU: configuration through sysfs, samples from the character device.
pub struct Imu {
    dir: PathBuf,
    dev: PathBuf,
    name: String,
    cal: Calibration,
    channels: Channels,
    stream: Option<Stream>,
}

impl std::fmt::Debug for Imu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Imu")
            .field("dir", &self.dir)
            .field("dev", &self.dev)
            .field("channels", &self.channels)
            .finish()
    }
}

impl Imu {
    /// `dir` is the IIO device directory; `dev` the character device (default
    /// `/dev/<directory name>`).
    pub fn open(dir: impl Into<PathBuf>, dev: Option<PathBuf>) -> io::Result<Imu> {
        let dir = dir.into();
        let base = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("iio:device0")
            .to_string();
        let dev = dev.unwrap_or_else(|| PathBuf::from("/dev").join(&base));
        let name = fs::read_to_string(dir.join("name"))?.trim().to_string();
        let cal = Calibration::read(&dir);
        Ok(Imu {
            dir,
            dev,
            name,
            cal,
            channels: Channels::default(),
            stream: None,
        })
    }

    pub fn calibration(&self) -> &Calibration {
        &self.cal
    }

    pub fn channels(&self) -> Channels {
        self.channels
    }

    fn write(&self, attr: &str, value: &str) -> io::Result<()> {
        if !allowed(attr) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("IIO attribute {attr:?} is not written by glassd"),
            ));
        }
        let p = self.dir.join(attr);
        let mut f = OpenOptions::new().write(true).truncate(true).open(&p)?;
        f.write_all(format!("{value}\n").as_bytes())
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", p.display())))
    }

    fn write_if_present(&self, attr: &str, value: &str) -> io::Result<()> {
        if self.dir.join(attr).exists() {
            self.write(attr, value)
        } else {
            Ok(())
        }
    }

    fn stop(&mut self) -> io::Result<()> {
        self.stream = None;
        self.write_if_present("buffer/enable", "0")
    }

    /// Starts, reconfigures or stops the stream. With no channel the chip is powered down.
    pub fn set_channels(&mut self, ch: Channels) -> io::Result<()> {
        if ch == self.channels && (self.stream.is_some() || !ch.any()) {
            return Ok(());
        }
        // Settings are refused while the buffer runs: stop it first.
        let r = self.stop();
        self.channels = Channels::default();
        r?;
        if !ch.any() {
            self.write_if_present("gyro_enable", "0")?;
            self.write_if_present("accl_enable", "0")?;
            return self.write_if_present("power_state", "0");
        }
        self.write_if_present("power_state", "1")?;
        let rate = if ch.gyro { RATE_HZ } else { LOOK_UP_RATE_HZ };
        self.write_if_present("sampling_frequency", &rate.to_string())?;
        self.write_if_present("gyro_enable", if ch.gyro { "1" } else { "0" })?;
        self.write_if_present("accl_enable", if ch.accel { "1" } else { "0" })?;
        self.write_if_present("compass_enable", "0")?;
        let scan = self.dir.join("scan_elements");
        for entry in fs::read_dir(&scan)? {
            let file = entry?.file_name();
            let Some(name) = file.to_str().and_then(|n| n.strip_suffix("_en")) else {
                continue;
            };
            let on = name == TIMESTAMP
                || (ch.gyro && GYRO.contains(&name))
                || (ch.accel && ACCEL.contains(&name));
            self.write(
                &format!("scan_elements/{name}_en"),
                if on { "1" } else { "0" },
            )?;
        }
        let trig = self.dir.join("trigger/current_trigger");
        if trig.exists() && fs::read_to_string(&trig)?.trim().is_empty() {
            let n = self
                .dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("iio:device"))
                .unwrap_or("0")
                .to_string();
            self.write("trigger/current_trigger", &format!("{}-dev{n}", self.name))?;
        }
        let layout = Layout::read_dir(&scan)?;
        if layout.size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no scan element enabled",
            ));
        }
        self.write_if_present("buffer/enable", "1")?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&self.dev)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", self.dev.display())))?;
        self.stream = Some(Stream {
            file,
            layout,
            partial: Vec::new(),
        });
        self.channels = ch;
        Ok(())
    }

    /// The descriptor to poll while streaming.
    pub fn fd(&self) -> Option<RawFd> {
        self.stream.as_ref().map(|s| s.file.as_raw_fd())
    }

    /// Reads what is available; `now_ns` stamps records without a timestamp.
    pub fn read(&mut self, now_ns: i64, out: &mut Vec<Sample>) -> io::Result<()> {
        let Some(s) = &mut self.stream else {
            return Ok(());
        };
        let mut buf = [0u8; 4096];
        loop {
            match s.file.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    s.partial.extend_from_slice(&buf[..n]);
                    let size = s.layout.size;
                    let whole = s.partial.len() / size * size;
                    for rec in s.partial[..whole].chunks_exact(size) {
                        out.push(self.cal.sample(&s.layout.decode(rec), now_ns));
                    }
                    s.partial.drain(..whole);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Imu {
    fn drop(&mut self) {
        let _ = self.set_channels(Channels::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(s: &str) -> ScanType {
        ScanType::parse(s).unwrap()
    }

    fn el(name: &str, index: u32, ty: &str) -> Element {
        Element {
            name: name.into(),
            index,
            ty: st(ty),
        }
    }

    #[test]
    fn scan_types() {
        assert_eq!(
            st("le:s16/16>>0"),
            ScanType {
                big_endian: false,
                signed: true,
                realbits: 16,
                storagebits: 16,
                shift: 0
            }
        );
        assert!(st("be:u12/16>>4").big_endian);
        for bad in [
            "",
            "xx:s16/16>>0",
            "le:q16/16>>0",
            "le:s16/12>>0",
            "le:s20/16>>0",
        ] {
            assert!(ScanType::parse(bad).is_none(), "{bad}");
        }
        assert_eq!(st("le:s16/16>>0").decode(&[0xff, 0xff]), -1);
        assert_eq!(st("be:s16/16>>0").decode(&[0x80, 0x00]), -32768);
        assert_eq!(st("be:u12/16>>4").decode(&[0xab, 0xcd]), 0xabc);
        assert_eq!(st("le:s12/16>>0").decode(&[0x00, 0x08]), -2048);
    }

    /// Gyro + accel + timestamp: 12 data bytes, the format byte at 15, the timestamp at 16
    /// (as the Glass driver packs it, kernel-interfaces.md §3.2).
    fn gyro_accel() -> Layout {
        Layout::new(vec![
            el(TIMESTAMP, 13, "le:s64/64>>0"),
            el("in_accel_x", 7, "le:s16/16>>0"),
            el("in_accel_y", 8, "le:s16/16>>0"),
            el("in_accel_z", 9, "le:s16/16>>0"),
            el("in_anglvel_x", 4, "le:s16/16>>0"),
            el("in_anglvel_y", 5, "le:s16/16>>0"),
            el("in_anglvel_z", 6, "le:s16/16>>0"),
        ])
    }

    pub(crate) fn record(gyro: [i16; 3], accel: [i16; 3], format: u8, t: i64) -> Vec<u8> {
        let mut r = Vec::new();
        for v in gyro.iter().chain(&accel) {
            r.extend_from_slice(&v.to_le_bytes());
        }
        r.extend_from_slice(&[0, 0, 0, format]);
        r.extend_from_slice(&t.to_le_bytes());
        r
    }

    #[test]
    fn layout_with_format_byte() {
        let l = gyro_accel();
        assert_eq!(l.size, 24);
        assert_eq!(l.format_byte, Some(15));
        assert_eq!(l.timestamp.as_ref().unwrap().1, 16);
        assert_eq!(l.fields[0].0.name, "in_anglvel_x");
        assert_eq!(l.fields[3], (el("in_accel_x", 7, "le:s16/16>>0"), 6));
        let r = l.decode(&record([1, -2, 3], [4, 5, -6], 0x06, 123_456_789));
        assert_eq!(r.get("in_anglvel_y"), Some(-2));
        assert_eq!(r.get("in_accel_z"), Some(-6));
        assert_eq!(r.format, Some(0x06));
        assert_eq!(r.timestamp_ns, Some(123_456_789));
    }

    #[test]
    fn layout_accel_only_and_no_padding() {
        let l = Layout::new(vec![
            el("in_accel_x", 7, "le:s16/16>>0"),
            el("in_accel_y", 8, "le:s16/16>>0"),
            el("in_accel_z", 9, "le:s16/16>>0"),
            el(TIMESTAMP, 13, "le:s64/64>>0"),
        ]);
        assert_eq!((l.size, l.format_byte), (16, Some(7)));
        // Four 16-bit values fill 8 bytes: no padding, so no format byte.
        let l = Layout::new(vec![
            el("in_anglvel_x", 4, "le:s16/16>>0"),
            el("in_anglvel_y", 5, "le:s16/16>>0"),
            el("in_accel_x", 7, "le:s16/16>>0"),
            el("in_accel_y", 8, "le:s16/16>>0"),
            el(TIMESTAMP, 13, "le:s64/64>>0"),
        ]);
        assert_eq!((l.size, l.format_byte), (16, None));
        // No timestamp.
        let l = Layout::new(vec![el("in_accel_x", 7, "le:s16/16>>0")]);
        assert_eq!((l.size, l.format_byte), (2, None));
    }

    #[test]
    fn matrices_and_units() {
        assert_eq!(parse_matrix("0,1,0,0,0,1,1,0,0\n"), Some(BOARD_MATRIX));
        assert_eq!(parse_matrix("1,2"), None);
        // out.x = chip.y, out.y = chip.z, out.z = chip.x.
        assert_eq!(apply(&BOARD_MATRIX, [1.0, 2.0, 3.0]), [2.0, 3.0, 1.0]);
        let c = Calibration::default();
        let l = gyro_accel();
        // Chip z = 16384 counts = 1 g at ±2 g -> out.y = 1 g.
        let r = l.decode(&record([0, 0, 16384], [0, 0, 16384], 0x06, 7));
        let s = c.sample(&r, 0);
        assert_eq!(s.t_ns, 7);
        let a = s.accel.unwrap();
        assert!(
            (a[1] - 1.0).abs() < 1e-6 && a[0] == 0.0 && a[2] == 0.0,
            "{a:?}"
        );
        // 16384 counts at ±2000 dps = 1000 dps on out.y.
        let g = s.gyro.unwrap();
        assert!((g[1] - 1000f32.to_radians()).abs() < 1e-3, "{g:?}");
        // A format byte without the gyro bit: no gyro in this record.
        let s = c.sample(&l.decode(&record([1, 1, 1], [1, 1, 1], 0x04, 7)), 0);
        assert!(s.gyro.is_none() && s.accel.is_some());
    }

    fn fake_device(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("glass-head-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let d = root.join("iio:device0");
        fs::create_dir_all(d.join("scan_elements")).unwrap();
        fs::create_dir_all(d.join("buffer")).unwrap();
        fs::create_dir_all(d.join("trigger")).unwrap();
        fs::write(d.join("name"), "mpu9150\n").unwrap();
        fs::write(d.join("in_accel_scale"), "2\n").unwrap();
        fs::write(d.join("in_anglvel_scale"), "2000\n").unwrap();
        fs::write(d.join("gyro_matrix"), "0, 1, 0, 0, 0, 1, 1, 0, 0\n").unwrap();
        fs::write(d.join("accl_matrix"), "0,1,0,0,0,1,1,0,0\n").unwrap();
        fs::write(d.join("trigger/current_trigger"), "\n").unwrap();
        for f in [
            "power_state",
            "sampling_frequency",
            "gyro_enable",
            "accl_enable",
            "buffer/enable",
            "dmp_firmware",
            "self_test",
        ] {
            fs::write(d.join(f), "").unwrap();
        }
        let scan = [
            ("in_quaternion_r", 0, "be:s32/32>>0"),
            ("in_anglvel_x", 4, "le:s16/16>>0"),
            ("in_anglvel_y", 5, "le:s16/16>>0"),
            ("in_anglvel_z", 6, "le:s16/16>>0"),
            ("in_accel_x", 7, "le:s16/16>>0"),
            ("in_accel_y", 8, "le:s16/16>>0"),
            ("in_accel_z", 9, "le:s16/16>>0"),
            ("in_magn_x", 10, "le:s16/16>>0"),
            (TIMESTAMP, 13, "le:s64/64>>0"),
        ];
        for (n, i, t) in scan {
            fs::write(d.join(format!("scan_elements/{n}_en")), "1\n").unwrap();
            fs::write(d.join(format!("scan_elements/{n}_index")), format!("{i}\n")).unwrap();
            fs::write(d.join(format!("scan_elements/{n}_type")), format!("{t}\n")).unwrap();
        }
        root
    }

    #[test]
    fn finds_configures_and_reads_a_fake_device() {
        let root = fake_device("dev");
        fs::create_dir_all(root.join("iio:device1")).unwrap();
        fs::write(root.join("iio:device1/name"), "other\n").unwrap();
        let dir = find(&root, IMU_NAME).unwrap().unwrap();
        assert!(dir.ends_with("iio:device0"));
        assert!(find(&root, "nothing").unwrap().is_none());
        let node = root.join("node");
        let mut bytes = record([0, 0, 0], [0, 0, 16384], 0x06, 1_000);
        bytes.extend(record([0, 0, 0], [0, 0, 16384], 0x06, 21_000_000));
        bytes.extend_from_slice(&[1, 2, 3]); // a partial record
        fs::write(&node, &bytes).unwrap();
        let mut imu = Imu::open(&dir, Some(node.clone())).unwrap();
        imu.set_channels(Channels {
            accel: true,
            gyro: true,
        })
        .unwrap();
        let rd = |f: &str| fs::read_to_string(dir.join(f)).unwrap();
        assert_eq!(rd("power_state"), "1\n");
        assert_eq!(rd("sampling_frequency"), "50\n");
        assert_eq!(rd("gyro_enable"), "1\n");
        assert_eq!(rd("buffer/enable"), "1\n");
        assert_eq!(rd("trigger/current_trigger"), "mpu9150-dev0\n");
        assert_eq!(rd("scan_elements/in_quaternion_r_en"), "0\n");
        assert_eq!(rd("scan_elements/in_magn_x_en"), "0\n");
        assert_eq!(rd("scan_elements/in_anglvel_z_en"), "1\n");
        assert_eq!(rd("dmp_firmware"), "");
        assert!(imu.fd().is_some());
        let mut out = Vec::new();
        imu.read(0, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].t_ns, 21_000_000);
        assert!((out[0].accel.unwrap()[1] - 1.0).abs() < 1e-6);
        // Accel only: the gyro scan elements are switched off.
        imu.set_channels(Channels {
            accel: true,
            gyro: false,
        })
        .unwrap();
        assert_eq!(rd("gyro_enable"), "0\n");
        assert_eq!(rd("sampling_frequency"), "10\n");
        assert_eq!(rd("scan_elements/in_anglvel_z_en"), "0\n");
        assert_eq!(rd("scan_elements/in_accel_z_en"), "1\n");
        // Stop: buffer off, chip powered down.
        imu.set_channels(Channels::default()).unwrap();
        assert!(imu.fd().is_none());
        assert_eq!(rd("buffer/enable"), "0\n");
        assert_eq!(rd("power_state"), "0\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuses_other_attributes() {
        let root = fake_device("refuse");
        let imu = Imu::open(root.join("iio:device0"), None).unwrap();
        for bad in [
            "dmp_firmware",
            "self_test",
            "glu_trigger",
            "scan_elements/../x_en",
            "reg_dump",
        ] {
            assert_eq!(
                imu.write(bad, "1").unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
                "{bad}"
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
