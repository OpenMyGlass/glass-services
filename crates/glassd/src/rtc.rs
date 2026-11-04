//! The wake alarm of `/dev/rtc0`.
//!
//! The file descriptor turns readable when the alarm fires, which is what gets glassd running
//! after a resume: `poll`'s timeout counts `CLOCK_MONOTONIC`, which stops during a suspend, and
//! the system suspends again within about half a second if nobody takes a wakelock.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// `struct rtc_time` from `<linux/rtc.h>`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RtcTime {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
}

/// `struct rtc_wkalrm` from `<linux/rtc.h>`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RtcWkalrm {
    enabled: u8,
    pending: u8,
    time: RtcTime,
}

const fn ioc(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((b'p' as u32) << 8) | nr) as libc::c_ulong
}
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const RTC_RD_TIME: libc::c_ulong = ioc(IOC_READ, 0x09, size_of::<RtcTime>());
const RTC_WKALM_SET: libc::c_ulong = ioc(IOC_WRITE, 0x0f, size_of::<RtcWkalrm>());

/// A civil date from days since 1970-01-01 (Howard Hinnant's algorithm), for `rtc_time`: going
/// through `gmtime` would need a libc `tm` that differs between targets.
fn civil_from_days(days: i64) -> (i32, i32, i32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year as i32, month as i32, day as i32)
}

/// The inverse of [`civil_from_days`].
fn days_from_civil(year: i32, month: i32, day: i32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn add_seconds(t: RtcTime, seconds: u32) -> RtcTime {
    let days = days_from_civil(t.year + 1900, t.mon + 1, t.mday);
    let total = days * 86_400
        + i64::from(t.hour) * 3600
        + i64::from(t.min) * 60
        + i64::from(t.sec)
        + i64::from(seconds);
    let (year, month, day) = civil_from_days(total.div_euclid(86_400));
    let secs = total.rem_euclid(86_400) as i32;
    RtcTime {
        sec: secs % 60,
        min: secs / 60 % 60,
        hour: secs / 3600,
        mday: day,
        mon: month - 1,
        year: year - 1900,
        wday: -1,
        yday: -1,
        isdst: -1,
    }
}

/// See the module doc.
#[derive(Debug)]
pub struct RtcAlarm {
    file: File,
}

impl RtcAlarm {
    pub fn open(path: &Path) -> io::Result<RtcAlarm> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        Ok(RtcAlarm { file })
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Replaces the alarm with one `seconds` from now.
    pub fn arm(&self, seconds: u32) -> io::Result<()> {
        let mut now = RtcTime::default();
        // SAFETY: the file is open and `now` is the struct this request fills.
        if unsafe { libc::ioctl(self.fd(), RTC_RD_TIME as _, &mut now) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.set(RtcWkalrm {
            enabled: 1,
            pending: 0,
            time: add_seconds(now, seconds),
        })
    }

    pub fn disarm(&self) -> io::Result<()> {
        let mut now = RtcTime::default();
        // SAFETY: as in `arm`.
        if unsafe { libc::ioctl(self.fd(), RTC_RD_TIME as _, &mut now) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.set(RtcWkalrm {
            enabled: 0,
            pending: 0,
            time: add_seconds(now, 60),
        })
    }

    fn set(&self, alarm: RtcWkalrm) -> io::Result<()> {
        // SAFETY: the file is open and `alarm` is the struct this request reads.
        if unsafe { libc::ioctl(self.fd(), RTC_WKALM_SET as _, &alarm) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Clears the readable state after the alarm fired.
    pub fn acknowledge(&mut self) {
        let mut buf = [0u8; size_of::<libc::c_ulong>()];
        let _ = self.file.read(&mut buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: i32, mon: i32, mday: i32, hour: i32, min: i32, sec: i32) -> RtcTime {
        RtcTime {
            sec,
            min,
            hour,
            mday,
            mon,
            year: year - 1900,
            ..Default::default()
        }
    }

    fn fields(t: RtcTime) -> (i32, i32, i32, i32, i32, i32) {
        (t.year + 1900, t.mon, t.mday, t.hour, t.min, t.sec)
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_header() {
        assert_eq!(RTC_RD_TIME, 0x8024_7009);
        assert_eq!(RTC_WKALM_SET, 0x4028_700f);
    }

    #[test]
    fn adds_seconds_within_a_minute() {
        let t = add_seconds(at(2026, 10, 4, 13, 30, 10), 10);
        assert_eq!(fields(t), (2026, 10, 4, 13, 30, 20));
    }

    #[test]
    fn carries_over_midnight_and_year_end() {
        let t = add_seconds(at(2026, 11, 31, 23, 59, 55), 10);
        assert_eq!(fields(t), (2027, 0, 1, 0, 0, 5));
    }

    #[test]
    fn handles_a_leap_day() {
        let t = add_seconds(at(2028, 1, 28, 23, 59, 59), 1);
        assert_eq!(fields(t), (2028, 1, 29, 0, 0, 0));
        let t = add_seconds(at(2028, 1, 29, 12, 0, 0), 86_400);
        assert_eq!(fields(t), (2028, 2, 1, 12, 0, 0));
    }

    #[test]
    fn thirty_minutes_later() {
        let t = add_seconds(at(2026, 10, 4, 23, 45, 0), 1_800);
        assert_eq!(fields(t), (2026, 10, 5, 0, 15, 0));
    }
}
