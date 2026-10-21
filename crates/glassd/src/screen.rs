//! Screen wake and sleep policy (docs/api.md "Screen", docs/specs/wake-policy.md §1-3), as a
//! pure state machine. Time comes in as [`Now`]; changes go out as [`Change`]. The caller
//! applies them to the display and publishes them.

use glass_api::ClientId;
use glass_api::proto::{Screen, ScreenState, screen_request::Action};
use std::collections::BTreeMap;

/// One-shot timeout after a wake by look-up or don, with no dim step (wake-policy.md §1).
pub const QUICK_TIMEOUT_MS: u32 = 3_000;
/// Longest keep-on hold; 0 also means this (docs/api.md).
pub const MAX_HOLD_MS: u32 = 600_000;
/// Upper bound of the dim step (wake-policy.md §1: min(3000 ms, 20 % of the timeout)).
pub const MAX_DIM_MS: u32 = 3_000;

/// The dim step before the screen goes off.
pub fn dim_ms(timeout_ms: u32) -> u32 {
    MAX_DIM_MS.min(timeout_ms / 5)
}

/// The current time on both clocks, µs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    /// `CLOCK_MONOTONIC` (timers, timestamps).
    pub mono_us: u64,
    /// `CLOCK_BOOTTIME` (counts suspend; for `off_ms`).
    pub boot_us: u64,
}

impl Now {
    /// Both clocks equal (tests).
    pub fn at(us: u64) -> Now {
        Now {
            mono_us: us,
            boot_us: us,
        }
    }
}

/// A screen state change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub screen: Screen,
    pub reason: &'static str,
    pub at_us: u64,
    /// With a wake from `SCREEN_OFF`: why the screen last went off.
    pub last_sleep_reason: &'static str,
    /// With a wake from `SCREEN_OFF`: how long it was off (`CLOCK_BOOTTIME`), ms.
    pub off_ms: u32,
}

impl Change {
    pub fn to_state(&self) -> ScreenState {
        ScreenState {
            screen: self.screen as i32,
            reason: self.reason.into(),
            timestamp_us: self.at_us,
            last_sleep_reason: self.last_sleep_reason.into(),
            off_ms: self.off_ms,
        }
    }
}

/// What to do with a touch that starts now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchStart {
    /// Screen on: deliver the touch.
    Deliver,
    /// Screen dim: the touch restored the screen and is consumed.
    Consume,
    /// Screen off: the touch is consumed; a tap or horizontal swipe in it may wake the screen.
    ScreenOff,
    /// Off the head: the touchpad is asleep; ignore the touch.
    Ignore,
}

#[derive(Debug)]
pub struct ScreenPolicy {
    screen: Screen,
    changed: Change,
    timeout_ms: u32,
    quick: bool,
    last_activity_us: u64,
    /// Keep-on holds: client -> end time (mono µs).
    holds: BTreeMap<ClientId, u64>,
    last_sleep_reason: &'static str,
    slept_at_boot_us: u64,
    on_head: bool,
}

impl ScreenPolicy {
    /// Starts with the screen on.
    pub fn new(now: Now, timeout_ms: u32) -> Self {
        ScreenPolicy {
            screen: Screen::On,
            changed: Change {
                screen: Screen::On,
                reason: "request",
                at_us: now.mono_us,
                last_sleep_reason: "",
                off_ms: 0,
            },
            timeout_ms,
            quick: false,
            last_activity_us: now.mono_us,
            holds: BTreeMap::new(),
            last_sleep_reason: "",
            slept_at_boot_us: now.boot_us,
            on_head: true,
        }
    }

    pub fn screen(&self) -> Screen {
        self.screen
    }

    pub fn is_off(&self) -> bool {
        self.screen == Screen::Off
    }

    /// The last change (for snapshots).
    pub fn state(&self) -> ScreenState {
        self.changed.to_state()
    }

    pub fn set_timeout(&mut self, timeout_ms: u32) {
        self.timeout_ms = timeout_ms;
    }

    fn set(&mut self, now: Now, screen: Screen, reason: &'static str) -> Option<Change> {
        if self.screen == screen {
            return None;
        }
        let mut c = Change {
            screen,
            reason,
            at_us: now.mono_us,
            last_sleep_reason: "",
            off_ms: 0,
        };
        if self.screen == Screen::Off {
            c.last_sleep_reason = self.last_sleep_reason;
            c.off_ms = (now.boot_us.saturating_sub(self.slept_at_boot_us) / 1000)
                .min(u32::MAX as u64) as u32;
        }
        if screen == Screen::Off {
            self.last_sleep_reason = reason;
            self.slept_at_boot_us = now.boot_us;
        }
        if screen == Screen::On {
            self.last_activity_us = now.mono_us;
        }
        self.screen = screen;
        self.changed = c.clone();
        Some(c)
    }

    /// User activity: restarts the timer and ends the quick timeout; restores a dim screen.
    pub fn activity(&mut self, now: Now) -> Option<Change> {
        match self.screen {
            Screen::Off | Screen::Unknown => None,
            Screen::On => {
                self.last_activity_us = now.mono_us;
                self.quick = false;
                None
            }
            Screen::Dim => {
                self.quick = false;
                self.set(now, Screen::On, "activity")
            }
        }
    }

    /// A touch starts (first finger down).
    pub fn touch_down(&mut self, now: Now) -> (TouchStart, Option<Change>) {
        if !self.on_head {
            return (TouchStart::Ignore, None);
        }
        match self.screen {
            Screen::Off | Screen::Unknown => (TouchStart::ScreenOff, None),
            Screen::Dim => (TouchStart::Consume, self.activity(now)),
            Screen::On => (TouchStart::Deliver, self.activity(now)),
        }
    }

    /// Wakes the screen (`reason` is a wake reason); `quick` for look-up and don. Nothing
    /// wakes the screen while the Glass is off the head.
    pub fn wake(&mut self, now: Now, reason: &'static str, quick: bool) -> Option<Change> {
        if !self.on_head {
            return None;
        }
        match self.screen {
            Screen::On => {
                self.last_activity_us = now.mono_us;
                self.quick &= quick;
                None
            }
            _ => {
                self.quick = quick && self.holds.is_empty();
                self.set(now, Screen::On, reason)
            }
        }
    }

    /// A screen-off tap or swipe.
    pub fn wake_gesture(&mut self, now: Now, reason: &'static str) -> Option<Change> {
        self.wake(now, reason, false)
    }

    /// An explicit sleep: off now, and every hold is cancelled.
    pub fn sleep(&mut self, now: Now, reason: &'static str) -> Option<Change> {
        self.holds.clear();
        self.quick = false;
        self.set(now, Screen::Off, reason)
    }

    /// Dims now (the mock's `dim` command).
    pub fn force_dim(&mut self, now: Now) -> Option<Change> {
        if self.screen == Screen::On {
            self.set(now, Screen::Dim, "dim")
        } else {
            None
        }
    }

    /// Wearing state changed.
    pub fn set_on_head(&mut self, now: Now, on_head: bool) -> Option<Change> {
        if on_head == self.on_head {
            return None;
        }
        self.on_head = on_head;
        if on_head {
            if self.is_off() {
                self.wake(now, "don", true)
            } else {
                None
            }
        } else {
            self.sleep(now, "off-head")
        }
    }

    pub fn on_head(&self) -> bool {
        self.on_head
    }

    /// A `ScreenRequest` from a client.
    pub fn request(
        &mut self,
        now: Now,
        client: ClientId,
        action: Action,
        duration_ms: u32,
    ) -> Option<Change> {
        match action {
            Action::Unspecified => None,
            Action::Wake => {
                self.quick = false;
                self.wake(now, "request", false)
            }
            Action::Sleep => self.sleep(now, "request"),
            // Off the head, holds are not accepted (taking the Glass off cancelled them).
            Action::KeepOn if !self.on_head => None,
            Action::KeepOn => {
                let d = if duration_ms == 0 || duration_ms > MAX_HOLD_MS {
                    MAX_HOLD_MS
                } else {
                    duration_ms
                };
                self.holds.insert(client, now.mono_us + u64::from(d) * 1000);
                self.quick = false;
                self.wake(now, "request", false)
            }
            Action::ReleaseKeepOn => {
                if self.holds.remove(&client).is_some() {
                    self.last_activity_us = self.last_activity_us.max(now.mono_us);
                }
                None
            }
            Action::UserActivity => self.activity(now),
        }
    }

    /// A client disconnected: its hold ends.
    pub fn client_gone(&mut self, now: Now, client: ClientId) {
        self.request(now, client, Action::ReleaseKeepOn, 0);
    }

    fn timeout(&self) -> (u32, u32) {
        if self.quick {
            (QUICK_TIMEOUT_MS, 0)
        } else {
            (self.timeout_ms, dim_ms(self.timeout_ms))
        }
    }

    /// When [`on_timeout`](Self::on_timeout) must run next (mono µs).
    pub fn next_deadline(&self) -> Option<u64> {
        if let Some(&end) = self.holds.values().min() {
            return Some(end);
        }
        let (timeout, dim) = self.timeout();
        let off_at = self.last_activity_us + u64::from(timeout) * 1000;
        match self.screen {
            Screen::On if dim > 0 => Some(off_at - u64::from(dim) * 1000),
            Screen::On | Screen::Dim => Some(off_at),
            Screen::Off | Screen::Unknown => None,
        }
    }

    /// Ends expired holds, then dims or turns off when the time has come.
    pub fn on_timeout(&mut self, now: Now) -> Option<Change> {
        let t = now.mono_us;
        let expired: Vec<ClientId> = self
            .holds
            .iter()
            .filter(|&(_, &end)| end <= t)
            .map(|(&c, _)| c)
            .collect();
        for c in expired {
            let end = self.holds.remove(&c).expect("present");
            self.last_activity_us = self.last_activity_us.max(end);
        }
        if !self.holds.is_empty() {
            return None;
        }
        let (timeout, dim) = self.timeout();
        let off_at = self.last_activity_us + u64::from(timeout) * 1000;
        let dim_at = off_at - u64::from(dim) * 1000;
        match self.screen {
            Screen::On | Screen::Dim if t >= off_at => {
                self.quick = false;
                self.set(now, Screen::Off, "timeout")
            }
            Screen::On if dim > 0 && t >= dim_at => self.set(now, Screen::Dim, "dim"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000;
    const MS: u64 = 1_000;

    fn policy() -> ScreenPolicy {
        ScreenPolicy::new(Now::at(0), 15_000)
    }

    fn run_to(p: &mut ScreenPolicy, t: u64) -> Vec<Change> {
        let mut out = Vec::new();
        while let Some(d) = p.next_deadline() {
            if d > t {
                break;
            }
            if let Some(c) = p.on_timeout(Now::at(d)) {
                out.push(c);
            } else if p.next_deadline() == Some(d) {
                break;
            }
        }
        out
    }

    #[test]
    fn dim_step_rule() {
        assert_eq!(dim_ms(15_000), 3_000);
        assert_eq!(dim_ms(10_000), 2_000);
        assert_eq!(dim_ms(3_000), 600);
    }

    #[test]
    fn default_timeout_dims_then_turns_off() {
        let mut p = policy();
        assert_eq!(p.next_deadline(), Some(12 * S));
        let c = run_to(&mut p, 20 * S);
        assert_eq!(
            c.iter()
                .map(|c| (c.screen, c.reason, c.at_us))
                .collect::<Vec<_>>(),
            vec![
                (Screen::Dim, "dim", 12 * S),
                (Screen::Off, "timeout", 15 * S)
            ]
        );
        assert_eq!(p.next_deadline(), None);
    }

    #[test]
    fn activity_restarts_the_timer() {
        let mut p = policy();
        assert_eq!(p.activity(Now::at(10 * S)), None);
        assert_eq!(p.next_deadline(), Some(22 * S));
    }

    #[test]
    fn touch_on_dim_screen_restores_and_is_consumed() {
        let mut p = policy();
        run_to(&mut p, 13 * S);
        assert_eq!(p.screen(), Screen::Dim);
        let (what, c) = p.touch_down(Now::at(13 * S));
        assert_eq!(what, TouchStart::Consume);
        let c = c.unwrap();
        assert_eq!((c.screen, c.reason), (Screen::On, "activity"));
        assert_eq!(c.last_sleep_reason, "");
        assert_eq!(p.next_deadline(), Some(25 * S));
    }

    #[test]
    fn touch_on_screen_on_is_delivered() {
        let mut p = policy();
        assert_eq!(p.touch_down(Now::at(S)), (TouchStart::Deliver, None));
    }

    #[test]
    fn wake_from_off_reports_last_sleep_and_off_time() {
        let mut p = policy();
        run_to(&mut p, 20 * S);
        assert_eq!(p.touch_down(Now::at(20 * S)).0, TouchStart::ScreenOff);
        let now = Now {
            mono_us: 21 * S,
            boot_us: 30 * S, // 9 s in suspend
        };
        let c = p.wake_gesture(now, "tap").unwrap();
        assert_eq!((c.screen, c.reason), (Screen::On, "tap"));
        assert_eq!(c.last_sleep_reason, "timeout");
        assert_eq!(c.off_ms, 15_000);
        assert_eq!(p.state().reason, "tap");
    }

    #[test]
    fn quick_timeout_after_look_up_or_don_has_no_dim() {
        let mut p = policy();
        p.sleep(Now::at(S), "request");
        let c = p.wake(Now::at(2 * S), "look-up", true).unwrap();
        assert_eq!(c.reason, "look-up");
        assert_eq!(p.next_deadline(), Some(5 * S));
        let c = run_to(&mut p, 6 * S);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].screen, c[0].reason), (Screen::Off, "timeout"));
    }

    #[test]
    fn activity_ends_the_quick_timeout() {
        let mut p = policy();
        p.sleep(Now::at(S), "request");
        p.wake(Now::at(2 * S), "don", true);
        p.activity(Now::at(3 * S));
        assert_eq!(p.next_deadline(), Some(15 * S)); // 3 + 15 - 3 dim
    }

    #[test]
    fn keep_on_holds_and_expiry() {
        let mut p = policy();
        p.request(Now::at(0), 7, Action::KeepOn, 60_000);
        assert_eq!(p.next_deadline(), Some(60 * S));
        assert!(run_to(&mut p, 59 * S).is_empty());
        assert_eq!(p.screen(), Screen::On);
        // After the hold ends, a full timeout follows.
        let c = run_to(&mut p, 100 * S);
        assert_eq!(c[0].at_us, 72 * S);
        assert_eq!(c[1].at_us, 75 * S);
    }

    #[test]
    fn keep_on_zero_or_too_long_means_ten_minutes() {
        let mut p = policy();
        p.request(Now::at(0), 1, Action::KeepOn, 0);
        assert_eq!(p.next_deadline(), Some(600 * S));
        p.request(Now::at(0), 1, Action::KeepOn, 700_000);
        assert_eq!(p.next_deadline(), Some(600 * S));
        // Repeated keep-on replaces the hold.
        p.request(Now::at(0), 1, Action::KeepOn, 5_000);
        assert_eq!(p.next_deadline(), Some(5 * S));
    }

    #[test]
    fn keep_on_wakes_and_beats_the_quick_timeout() {
        let mut p = policy();
        p.sleep(Now::at(0), "request");
        let c = p.request(Now::at(S), 1, Action::KeepOn, 1_000).unwrap();
        assert_eq!(c.reason, "request");
        let mut p = policy();
        p.sleep(Now::at(0), "request");
        p.wake(Now::at(S), "look-up", true);
        p.request(Now::at(S), 1, Action::KeepOn, 1_000);
        // Hold ends at 2 s, then the normal timeout (dim at 2 + 12).
        let c = run_to(&mut p, 100 * S);
        assert_eq!(c[0].at_us, 14 * S);
    }

    #[test]
    fn explicit_sleep_cancels_holds() {
        let mut p = policy();
        p.request(Now::at(0), 1, Action::KeepOn, 0);
        let c = p.sleep(Now::at(S), "two-finger-swipe-down").unwrap();
        assert_eq!((c.screen, c.reason), (Screen::Off, "two-finger-swipe-down"));
        let c = p.wake(Now::at(2 * S), "request", false).unwrap();
        assert_eq!(c.last_sleep_reason, "two-finger-swipe-down");
        assert_eq!(p.next_deadline(), Some(14 * S)); // no hold left
    }

    #[test]
    fn release_and_disconnect_end_holds() {
        let mut p = policy();
        p.request(Now::at(0), 1, Action::KeepOn, 0);
        p.request(Now::at(0), 2, Action::KeepOn, 0);
        p.request(Now::at(10 * S), 1, Action::ReleaseKeepOn, 0);
        assert_eq!(p.next_deadline(), Some(600 * S));
        p.client_gone(Now::at(20 * S), 2);
        assert_eq!(p.next_deadline(), Some(32 * S));
    }

    #[test]
    fn user_activity_request_restores_dim() {
        let mut p = policy();
        run_to(&mut p, 13 * S);
        let c = p
            .request(Now::at(13 * S), 1, Action::UserActivity, 0)
            .unwrap();
        assert_eq!(c.reason, "activity");
        // While off it does nothing.
        p.sleep(Now::at(14 * S), "request");
        assert_eq!(p.request(Now::at(15 * S), 1, Action::UserActivity, 0), None);
        assert!(p.is_off());
    }

    #[test]
    fn off_head_refuses_wake_keep_on_and_button() {
        let mut p = policy();
        p.set_on_head(Now::at(S), false);
        assert_eq!(p.request(Now::at(2 * S), 1, Action::Wake, 0), None);
        assert_eq!(p.request(Now::at(2 * S), 1, Action::KeepOn, 0), None);
        assert_eq!(p.wake(Now::at(2 * S), "button", false), None);
        assert!(p.is_off());
        // The refused hold does not exist after the don: quick timeout applies.
        p.set_on_head(Now::at(3 * S), true);
        assert_eq!(p.next_deadline(), Some(6 * S));
    }

    #[test]
    fn off_head_sleeps_and_ignores_touches_don_wakes_quick() {
        let mut p = policy();
        let c = p.set_on_head(Now::at(S), false).unwrap();
        assert_eq!(c.reason, "off-head");
        assert_eq!(p.touch_down(Now::at(2 * S)).0, TouchStart::Ignore);
        assert_eq!(p.wake_gesture(Now::at(2 * S), "tap"), None);
        let c = p.set_on_head(Now::at(3 * S), true).unwrap();
        assert_eq!((c.reason, c.last_sleep_reason), ("don", "off-head"));
        assert_eq!(p.next_deadline(), Some(6 * S));
    }

    #[test]
    fn late_timer_goes_straight_to_off() {
        let mut p = policy();
        let c = p.on_timeout(Now::at(100 * S)).unwrap();
        assert_eq!((c.screen, c.reason), (Screen::Off, "timeout"));
    }

    #[test]
    fn timeout_setting_applies() {
        let mut p = policy();
        p.set_timeout(5_000);
        assert_eq!(p.next_deadline(), Some(4 * S));
        let _ = MS;
    }
}
