//! How long the Glass has been off the head, and what follows from it (Model B).
//!
//! Off the head the system suspends. For [`WATCH_WINDOW_US`] an RTC alarm wakes it every
//! [`WATCH_PERIOD_S`] seconds to read the on-head sensor; after that only a key or the sensor's
//! own interrupt wakes it. After `off_head_power_off_min` minutes it powers off, and the power
//! button is needed to turn it on again.
//!
//! Times are `CLOCK_BOOTTIME`, because `CLOCK_MONOTONIC` stops during a suspend. This module only
//! decides; the daemon arms the alarm and powers off.

/// How long the sensor is read periodically after leaving the head.
pub const WATCH_WINDOW_US: u64 = 10 * 60 * 1_000_000;
/// The period of those readings.
pub const WATCH_PERIOD_S: u32 = 10;
/// The shortest alarm: the RTC counts whole seconds, so a shorter one could be already past.
const MIN_ALARM_S: u32 = 2;

const MIN_US: u64 = 60 * 1_000_000;

/// See the module doc.
#[derive(Debug)]
pub struct OffHeadTimer {
    since_us: Option<u64>,
    /// Minutes before powering off; 0 = never.
    power_off_min: u32,
    /// Whether the sensor's reading can change the wear; forced off the head it cannot.
    watching: bool,
}

impl OffHeadTimer {
    pub fn new(power_off_min: u32) -> OffHeadTimer {
        OffHeadTimer {
            since_us: None,
            power_off_min,
            watching: true,
        }
    }

    pub fn set_power_off_min(&mut self, minutes: u32) {
        self.power_off_min = minutes;
    }

    /// Whether reading the sensor can change anything. If not, the alarms are saved.
    pub fn set_watching(&mut self, watching: bool) {
        self.watching = watching;
    }

    /// The wear changed.
    pub fn set_off_head(&mut self, boot_us: u64, off: bool) {
        self.since_us = off.then_some(boot_us);
    }

    /// The owner pressed a key while off the head: it is in use, so both windows start again.
    pub fn activity(&mut self, boot_us: u64) {
        if self.since_us.is_some() {
            self.since_us = Some(boot_us);
        }
    }

    /// When the power-off is due (`CLOCK_BOOTTIME` µs), if it ever is.
    pub fn power_off_at(&self) -> Option<u64> {
        let since = self.since_us?;
        (self.power_off_min > 0).then(|| since + u64::from(self.power_off_min) * MIN_US)
    }

    pub fn power_off_due(&self, boot_us: u64) -> bool {
        self.power_off_at().is_some_and(|at| boot_us >= at)
    }

    /// Seconds until the next alarm should wake the system; `None` when nothing needs one.
    pub fn next_alarm_s(&self, boot_us: u64) -> Option<u32> {
        let since = self.since_us?;
        let watch = (self.watching && boot_us.saturating_sub(since) < WATCH_WINDOW_US)
            .then_some(WATCH_PERIOD_S);
        let off = self.power_off_at().map(|at| {
            let left = at.saturating_sub(boot_us).div_ceil(1_000_000);
            u32::try_from(left).unwrap_or(u32::MAX)
        });
        let next = match (watch, off) {
            (Some(a), Some(b)) => a.min(b),
            (a, b) => a.or(b)?,
        };
        Some(next.max(MIN_ALARM_S))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    #[test]
    fn on_the_head_nothing_is_due() {
        let t = OffHeadTimer::new(30);
        assert_eq!(t.next_alarm_s(1_000 * S), None);
        assert!(!t.power_off_due(u64::MAX));
    }

    #[test]
    fn reads_every_10_s_then_only_waits_for_the_power_off() {
        let mut t = OffHeadTimer::new(30);
        t.set_off_head(100 * S, true);
        assert_eq!(t.next_alarm_s(100 * S), Some(10));
        assert_eq!(t.next_alarm_s(100 * S + WATCH_WINDOW_US - 1), Some(10));
        // Past the window the only alarm is the power-off, 20 minutes later.
        assert_eq!(t.next_alarm_s(100 * S + WATCH_WINDOW_US), Some(20 * 60));
    }

    #[test]
    fn powers_off_at_the_set_minutes() {
        let mut t = OffHeadTimer::new(30);
        t.set_off_head(0, true);
        assert!(!t.power_off_due(1_800 * S - 1));
        assert!(t.power_off_due(1_800 * S));
        // A late wake does not ask for an alarm in the past.
        assert_eq!(t.next_alarm_s(1_900 * S), Some(MIN_ALARM_S));
    }

    #[test]
    fn forced_off_the_head_only_the_power_off_needs_an_alarm() {
        let mut t = OffHeadTimer::new(30);
        t.set_off_head(0, true);
        t.set_watching(false);
        assert_eq!(t.next_alarm_s(0), Some(30 * 60));
        t.set_power_off_min(0);
        assert_eq!(t.next_alarm_s(0), None);
    }

    #[test]
    fn a_short_setting_shortens_the_alarm() {
        let mut t = OffHeadTimer::new(1);
        t.set_off_head(0, true);
        assert_eq!(t.next_alarm_s(55 * S), Some(5));
    }

    #[test]
    fn zero_minutes_never_powers_off() {
        let mut t = OffHeadTimer::new(0);
        t.set_off_head(0, true);
        assert!(!t.power_off_due(u64::MAX));
        assert_eq!(t.next_alarm_s(0), Some(10));
        assert_eq!(t.next_alarm_s(WATCH_WINDOW_US), None);
    }

    #[test]
    fn a_key_restarts_the_windows_and_donning_clears_them() {
        let mut t = OffHeadTimer::new(30);
        t.activity(5 * S); // on the head: no effect
        assert_eq!(t.power_off_at(), None);
        t.set_off_head(0, true);
        t.activity(1_000 * S);
        assert_eq!(t.power_off_at(), Some(2_800 * S));
        assert_eq!(t.next_alarm_s(1_000 * S), Some(10));
        t.set_off_head(1_001 * S, false);
        assert_eq!(t.power_off_at(), None);
    }
}
