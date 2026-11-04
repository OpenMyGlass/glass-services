//! The on-head state machine (docs/specs/wake-policy.md §4, docs/api.md "Wearing and wink"),
//! pure: inputs carry the time (`CLOCK_MONOTONIC`, µs), outputs are [`WearState`] changes.
//!
//! The reported state combines a *sensor state* (the glasshub's `don_doff`) with a *manual
//! mode*:
//!
//! | Mode | Reported |
//! |---|---|
//! | defer to the sensor | the sensor state; nothing is reported while it is unknown |
//! | forced off | off the head |
//! | timed forced on (30 s) | on the head |
//! | forced on | on the head |
//!
//! Transitions:
//! - start: timed forced on (the 30 s after boot, source `FORCED`);
//! - the timer runs out: defer to the sensor if on-head detection is on, else forced on;
//! - power key short press: defer or forced on or timed on -> forced off; forced off -> timed
//!   on if detection is on, else forced on (source `POWER_KEY`);
//! - hinge closed -> forced off; opened -> timed on (source `HINGE`);
//! - on-head detection turned on -> defer; turned off -> forced on (source `FORCED`).
//!
//! The sensor is **trusted only after it has reported on the head once** (since boot). A
//! `don_doff` of 0 before any 1 is kept out of the state: on the Model B glasshub firmware
//! (1.22) `don_doff` can sit at 0 while the Glass is worn, and honouring it turned on-head
//! detection into a lock-out (screen off, touchpad asleep, no way to navigate back to the
//! setting). Stock Glass has the same effect in another form: its sensor state is UNKNOWN
//! until the first DON/DOFF *event*, and a stuck sensor never produces one.
//!
//! A Glass that was never worn since boot would then never count as off the head, and never
//! sleep. So after [`NEVER_WORN_IDLE_MS`] with the sensor at 0 and no user input it is assumed
//! off the head. A key press takes that back, so a stuck sensor cannot lock the owner out.
//!
//! The source of a manual mode is what caused it. A [`WearState`] is reported when the wear
//! or its source changes; [`Update::wear_changed`] says whether the wear itself changed (the
//! off-head and don actions run only then).

use glass_api::proto::{Wear, WearSource, WearState};

/// The forced on-head period after boot, and after a power-key or hinge on-head
/// (wake-policy.md §1, `force_donned_timeout_ms`).
pub const FORCED_ON_MS: u64 = 30_000;

/// With an untrusted off-head reading, how long without user input before the Glass is assumed
/// off the head. Same as the default screen timeout: the screen is off by then anyway.
pub const NEVER_WORN_IDLE_MS: u64 = 120_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Defer,
    ForceOff,
    TimedOn { until_us: u64 },
    ForceOn,
}

/// A change of the reported state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Update {
    pub state: WearState,
    /// The wear changed (not only its source).
    pub wear_changed: bool,
}

#[derive(Debug, Clone)]
pub struct WearMachine {
    mode: Mode,
    cause: WearSource,
    detection: bool,
    sensor: Option<bool>,
    /// The sensor has reported on the head at least once: its off-head readings count.
    sensor_trusted: bool,
    /// An off-head reading was kept out because the sensor is not trusted yet.
    untrusted_off: bool,
    /// Off the head by [`NEVER_WORN_IDLE_MS`], not because the sensor is trusted.
    assumed_off: bool,
    last_input_us: u64,
    reported: WearState,
}

fn state(on: bool, source: WearSource, at_us: u64) -> WearState {
    WearState {
        wear: if on { Wear::OnHead } else { Wear::OffHead } as i32,
        snapshot: false,
        timestamp_us: at_us,
        source: source as i32,
    }
}

impl WearMachine {
    /// At boot: forced on the head for 30 s. `detection` is the effective on-head detection
    /// (the setting, and a glasshub present).
    pub fn new(now_us: u64, detection: bool) -> Self {
        WearMachine {
            mode: Mode::TimedOn {
                until_us: now_us + FORCED_ON_MS * 1000,
            },
            cause: WearSource::Forced,
            detection,
            sensor: None,
            sensor_trusted: false,
            untrusted_off: false,
            assumed_off: false,
            last_input_us: now_us,
            reported: state(true, WearSource::Forced, now_us),
        }
    }

    /// The reported state (for snapshots).
    pub fn state(&self) -> WearState {
        self.reported
    }

    pub fn on_head(&self) -> bool {
        self.reported.wear == Wear::OnHead as i32
    }

    /// Whether the glasshub should be paused (`pause = 1`): while forced off the head.
    pub fn hub_paused(&self) -> bool {
        self.mode == Mode::ForceOff
    }

    pub fn detection(&self) -> bool {
        self.detection
    }

    /// When [`on_timeout`](Self::on_timeout) must run next.
    pub fn next_deadline(&self) -> Option<u64> {
        match self.mode {
            Mode::TimedOn { until_us } => Some(until_us),
            Mode::Defer if self.never_worn_pending() => {
                Some(self.last_input_us + NEVER_WORN_IDLE_MS * 1000)
            }
            _ => None,
        }
    }

    fn never_worn_pending(&self) -> bool {
        self.detection && self.untrusted_off && !self.sensor_trusted && !self.assumed_off
    }

    /// The owner touched the Glass or pressed a key: it is in use. Restarts the never-worn wait,
    /// and takes back an assumed off-head.
    pub fn user_input(&mut self, now_us: u64) -> Option<Update> {
        self.last_input_us = now_us;
        if !self.assumed_off {
            return None;
        }
        self.assumed_off = false;
        self.sensor = Some(true);
        self.report(now_us)
    }

    fn set_mode(&mut self, mode: Mode, cause: WearSource, now_us: u64) -> Option<Update> {
        self.mode = mode;
        self.cause = cause;
        self.report(now_us)
    }

    fn report(&mut self, now_us: u64) -> Option<Update> {
        let (on, source) = match self.mode {
            Mode::Defer => (self.sensor?, WearSource::Sensor),
            Mode::ForceOff => (false, self.cause),
            Mode::TimedOn { .. } | Mode::ForceOn => (true, self.cause),
        };
        let new = state(on, source, now_us);
        if new.wear == self.reported.wear && new.source == self.reported.source {
            return None;
        }
        let wear_changed = new.wear != self.reported.wear;
        self.reported = new;
        Some(Update {
            state: new,
            wear_changed,
        })
    }

    /// The timed on-head ends.
    pub fn on_timeout(&mut self, now_us: u64) -> Option<Update> {
        if self.mode == Mode::Defer
            && self.never_worn_pending()
            && now_us >= self.last_input_us + NEVER_WORN_IDLE_MS * 1000
        {
            self.assumed_off = true;
            self.sensor = Some(false);
            return self.report(now_us);
        }
        match self.mode {
            Mode::TimedOn { until_us } if now_us >= until_us => {
                if self.detection {
                    self.set_mode(Mode::Defer, WearSource::Sensor, now_us)
                } else {
                    self.set_mode(Mode::ForceOn, WearSource::Forced, now_us)
                }
            }
            _ => None,
        }
    }

    /// A reading of the glasshub's `don_doff` (true = on the head). An off-head reading is
    /// ignored until the sensor has reported on the head once (see the module doc); the
    /// caller may log it with [`sensor_trusted`](Self::sensor_trusted).
    pub fn sensor(&mut self, now_us: u64, on: bool) -> Option<Update> {
        if on {
            self.sensor_trusted = true;
            self.assumed_off = false;
        } else if !self.sensor_trusted {
            self.untrusted_off = true;
            return None;
        }
        self.sensor = Some(on);
        if self.mode == Mode::Defer {
            self.report(now_us)
        } else {
            None
        }
    }

    /// Whether the sensor has reported on the head once, so its off-head readings count.
    pub fn sensor_trusted(&self) -> bool {
        self.sensor_trusted
    }

    /// A short press of the power key toggles a manual off-head.
    pub fn power_key(&mut self, now_us: u64) -> Option<Update> {
        let next = match self.mode {
            Mode::ForceOff if self.detection => Mode::TimedOn {
                until_us: now_us + FORCED_ON_MS * 1000,
            },
            Mode::ForceOff => Mode::ForceOn,
            Mode::Defer | Mode::TimedOn { .. } | Mode::ForceOn => Mode::ForceOff,
        };
        self.set_mode(next, WearSource::PowerKey, now_us)
    }

    /// The hinge switch: closed forces off the head, opened gives a timed on-head.
    pub fn hinge(&mut self, now_us: u64, open: bool) -> Option<Update> {
        let next = if open {
            Mode::TimedOn {
                until_us: now_us + FORCED_ON_MS * 1000,
            }
        } else {
            Mode::ForceOff
        };
        self.set_mode(next, WearSource::Hinge, now_us)
    }

    /// The effective on-head detection changed (the setting, with a glasshub present).
    pub fn set_detection(&mut self, now_us: u64, enabled: bool) -> Option<Update> {
        if enabled == self.detection {
            return None;
        }
        self.detection = enabled;
        if enabled {
            self.set_mode(Mode::Defer, WearSource::Sensor, now_us)
        } else {
            self.set_mode(Mode::ForceOn, WearSource::Forced, now_us)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;

    fn ws(u: Option<Update>) -> Option<(Wear, WearSource, bool)> {
        u.map(|u| {
            (
                Wear::try_from(u.state.wear).unwrap(),
                WearSource::try_from(u.state.source).unwrap(),
                u.wear_changed,
            )
        })
    }

    #[test]
    fn boot_is_forced_on_for_30_s_then_forced_on_without_detection() {
        let mut m = WearMachine::new(0, false);
        assert!(m.on_head());
        assert_eq!(m.state().source, WearSource::Forced as i32);
        assert_eq!(m.next_deadline(), Some(30 * S));
        assert_eq!(m.on_timeout(29 * S), None);
        // Same wear and source: nothing reported.
        assert_eq!(m.on_timeout(30 * S), None);
        assert_eq!(m.next_deadline(), None);
        // Sensor readings are ignored with detection off.
        assert_eq!(m.sensor(31 * S, false), None);
        assert!(m.on_head());
    }

    #[test]
    fn boot_with_detection_defers_to_the_sensor_after_30_s() {
        let mut m = WearMachine::new(0, true);
        // During the forced period the sensor is stored, not reported.
        assert_eq!(m.sensor(S, true), None);
        assert_eq!(m.sensor(2 * S, false), None);
        assert_eq!(
            ws(m.on_timeout(30 * S)),
            Some((Wear::OffHead, WearSource::Sensor, true))
        );
        assert_eq!(
            ws(m.sensor(40 * S, true)),
            Some((Wear::OnHead, WearSource::Sensor, true))
        );
        assert_eq!(m.sensor(41 * S, true), None);
    }

    #[test]
    fn a_never_worn_glass_is_assumed_off_the_head_after_idle_and_a_key_takes_it_back() {
        let idle = NEVER_WORN_IDLE_MS * 1000;
        let mut m = WearMachine::new(0, true);
        assert_eq!(m.sensor(S, false), None);
        assert_eq!(m.on_timeout(30 * S), None);
        assert!(m.on_head());
        // Input restarts the wait.
        assert_eq!(m.user_input(60 * S), None);
        assert_eq!(m.next_deadline(), Some(60 * S + idle));
        assert_eq!(m.on_timeout(60 * S + idle - 1), None);
        assert_eq!(
            ws(m.on_timeout(60 * S + idle)),
            Some((Wear::OffHead, WearSource::Sensor, true))
        );
        assert_eq!(m.next_deadline(), None);
        assert!(!m.sensor_trusted());
        // A key takes it back, and the wait starts again.
        let t = 300 * S;
        assert_eq!(
            ws(m.user_input(t)),
            Some((Wear::OnHead, WearSource::Sensor, true))
        );
        assert_eq!(m.next_deadline(), Some(t + idle));
        // Worn after an assumed off-head: an ordinary don, and the sensor is trusted.
        assert_eq!(ws(m.on_timeout(t + idle)).map(|u| u.0), Some(Wear::OffHead));
        assert_eq!(
            ws(m.sensor(t + idle + S, true)),
            Some((Wear::OnHead, WearSource::Sensor, true))
        );
        assert!(m.sensor_trusted());
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn an_off_head_reading_is_ignored_until_the_sensor_has_seen_on_head() {
        let mut m = WearMachine::new(0, true);
        assert!(!m.sensor_trusted());
        // A stuck 0 before any 1 (Model B glasshub 1.22): not the sensor state.
        assert_eq!(m.sensor(S, false), None);
        assert_eq!(m.on_timeout(30 * S), None);
        assert!(m.on_head());
        assert_eq!(m.sensor(31 * S, false), None);
        assert!(m.on_head());
        // The first on-head reading makes the sensor trusted; from then on 0 counts.
        assert_eq!(
            ws(m.sensor(32 * S, true)),
            Some((Wear::OnHead, WearSource::Sensor, false))
        );
        assert!(m.sensor_trusted());
        assert_eq!(
            ws(m.sensor(33 * S, false)),
            Some((Wear::OffHead, WearSource::Sensor, true))
        );
    }

    #[test]
    fn unknown_sensor_reports_nothing() {
        let mut m = WearMachine::new(0, true);
        assert_eq!(m.on_timeout(30 * S), None);
        // Still the boot state.
        assert_eq!(m.state().source, WearSource::Forced as i32);
        assert_eq!(
            ws(m.sensor(31 * S, true)),
            Some((Wear::OnHead, WearSource::Sensor, false))
        );
    }

    #[test]
    fn power_key_toggles_without_detection() {
        let mut m = WearMachine::new(0, false);
        assert_eq!(
            ws(m.power_key(S)),
            Some((Wear::OffHead, WearSource::PowerKey, true))
        );
        assert!(m.hub_paused());
        assert_eq!(
            ws(m.power_key(2 * S)),
            Some((Wear::OnHead, WearSource::PowerKey, true))
        );
        assert!(!m.hub_paused());
        // Forced on, not timed.
        assert_eq!(m.next_deadline(), None);
        assert_eq!(
            ws(m.power_key(3 * S)),
            Some((Wear::OffHead, WearSource::PowerKey, true))
        );
    }

    #[test]
    fn power_key_with_detection_gives_a_timed_on_head() {
        let mut m = WearMachine::new(0, true);
        m.sensor(0, true);
        m.on_timeout(30 * S);
        assert_eq!(
            ws(m.power_key(40 * S)),
            Some((Wear::OffHead, WearSource::PowerKey, true))
        );
        // The sensor cannot undo a manual off-head.
        assert_eq!(m.sensor(41 * S, true), None);
        assert_eq!(
            ws(m.power_key(50 * S)),
            Some((Wear::OnHead, WearSource::PowerKey, true))
        );
        assert_eq!(m.next_deadline(), Some(80 * S));
        // Back to the sensor (on head): only the source changes.
        assert_eq!(
            ws(m.on_timeout(80 * S)),
            Some((Wear::OnHead, WearSource::Sensor, false))
        );
    }

    #[test]
    fn power_key_during_the_boot_period_forces_off() {
        let mut m = WearMachine::new(0, false);
        assert_eq!(
            ws(m.power_key(5 * S)),
            Some((Wear::OffHead, WearSource::PowerKey, true))
        );
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn hinge_closed_forces_off_opened_gives_timed_on() {
        let mut m = WearMachine::new(0, false);
        assert_eq!(
            ws(m.hinge(S, false)),
            Some((Wear::OffHead, WearSource::Hinge, true))
        );
        assert_eq!(
            ws(m.hinge(2 * S, true)),
            Some((Wear::OnHead, WearSource::Hinge, true))
        );
        assert_eq!(m.next_deadline(), Some(32 * S));
        // Without detection: forced on afterwards.
        assert_eq!(
            ws(m.on_timeout(32 * S)),
            Some((Wear::OnHead, WearSource::Forced, false))
        );
    }

    #[test]
    fn detection_setting_switches_modes() {
        let mut m = WearMachine::new(0, false);
        m.on_timeout(30 * S);
        m.sensor(30 * S, true); // the sensor has seen on-head: trusted
        m.sensor(31 * S, false); // stored even with detection off
        assert_eq!(
            ws(m.set_detection(32 * S, true)),
            Some((Wear::OffHead, WearSource::Sensor, true))
        );
        assert_eq!(m.set_detection(33 * S, true), None);
        assert_eq!(
            ws(m.set_detection(34 * S, false)),
            Some((Wear::OnHead, WearSource::Forced, true))
        );
        // Turning detection off ends a manual off-head too.
        m.power_key(35 * S);
        m.set_detection(36 * S, true);
        m.power_key(37 * S); // defer -> forced off
        assert!(!m.on_head());
        assert_eq!(
            ws(m.set_detection(38 * S, false)),
            Some((Wear::OnHead, WearSource::Forced, true))
        );
    }

    #[test]
    fn timestamps_are_the_change_time() {
        let mut m = WearMachine::new(7, false);
        assert_eq!(m.state().timestamp_us, 7);
        let u = m.power_key(9 * S).unwrap();
        assert_eq!(u.state.timestamp_us, 9 * S);
        assert_eq!(m.state().timestamp_us, 9 * S);
    }
}
