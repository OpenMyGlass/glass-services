//! Camera and power buttons (docs/api.md "Buttons"), as a pure tracker per button: key
//! events in, `Button` frames out.
//!
//! - `PRESS` on release after a short press;
//! - `LONG_PRESS` once when the key has been held for [`LONG_PRESS_MS`], then `RELEASE` on
//!   release;
//! - `screen_off` is whether the screen was off when the key went down, for every frame of
//!   that press.
//!
//! The hold time is ours (the stock value is not in the specs); it is Android's default
//! long-press time.

use glass_api::proto::{Button, ButtonAction, ButtonId};

/// Hold time of a long press.
pub const LONG_PRESS_MS: u64 = 500;

/// `EV_KEY` value of a key release, press and autorepeat.
pub const KEY_UP: i32 = 0;
pub const KEY_DOWN: i32 = 1;

#[derive(Debug, Clone, Copy)]
struct Down {
    at_us: u64,
    screen_off: bool,
    long_sent: bool,
}

#[derive(Debug, Clone)]
pub struct ButtonTracker {
    id: ButtonId,
    down: Option<Down>,
}

impl ButtonTracker {
    pub fn new(id: ButtonId) -> Self {
        ButtonTracker { id, down: None }
    }

    pub fn id(&self) -> ButtonId {
        self.id
    }

    fn frame(&self, action: ButtonAction, d: &Down, at_us: u64) -> Button {
        Button {
            button: self.id as i32,
            action: action as i32,
            screen_off: d.screen_off,
            timestamp_us: at_us,
        }
    }

    /// One `EV_KEY` event of this button (`value` 1 down, 0 up; autorepeat is ignored).
    /// Returns whether this was a new key down, and the frames to publish.
    pub fn key(&mut self, value: i32, at_us: u64, screen_off: bool) -> (bool, Vec<Button>) {
        match value {
            KEY_DOWN if self.down.is_none() => {
                self.down = Some(Down {
                    at_us,
                    screen_off,
                    long_sent: false,
                });
                (true, Vec::new())
            }
            KEY_UP => match self.down.take() {
                Some(d) => {
                    let action = if d.long_sent {
                        ButtonAction::Release
                    } else {
                        ButtonAction::Press
                    };
                    (false, vec![self.frame(action, &d, at_us)])
                }
                None => (false, Vec::new()),
            },
            _ => (false, Vec::new()),
        }
    }

    /// When [`on_timeout`](Self::on_timeout) must run.
    pub fn next_deadline(&self) -> Option<u64> {
        self.down
            .filter(|d| !d.long_sent)
            .map(|d| d.at_us + LONG_PRESS_MS * 1000)
    }

    /// The long press, once the hold time has passed.
    pub fn on_timeout(&mut self, now_us: u64) -> Option<Button> {
        let d = self.down.as_mut()?;
        if d.long_sent || now_us < d.at_us + LONG_PRESS_MS * 1000 {
            return None;
        }
        d.long_sent = true;
        let d = *d;
        Some(self.frame(ButtonAction::LongPress, &d, now_us))
    }

    /// The device went away: forget a press in progress.
    pub fn cancel(&mut self) {
        self.down = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000;

    fn actions(v: &[Button]) -> Vec<ButtonAction> {
        v.iter()
            .map(|b| ButtonAction::try_from(b.action).unwrap())
            .collect()
    }

    #[test]
    fn short_press_is_reported_on_release() {
        let mut t = ButtonTracker::new(ButtonId::Camera);
        let (down, f) = t.key(KEY_DOWN, 0, true);
        assert!(down && f.is_empty());
        assert_eq!(t.next_deadline(), Some(500 * MS));
        assert_eq!(t.on_timeout(499 * MS), None);
        let (down, f) = t.key(KEY_UP, 200 * MS, false);
        assert!(!down);
        assert_eq!(actions(&f), vec![ButtonAction::Press]);
        assert!(f[0].screen_off, "screen state at key down");
        assert_eq!(f[0].button, ButtonId::Camera as i32);
        assert_eq!(f[0].timestamp_us, 200 * MS);
        assert_eq!(t.next_deadline(), None);
    }

    #[test]
    fn long_press_then_release() {
        let mut t = ButtonTracker::new(ButtonId::Power);
        t.key(KEY_DOWN, 0, false);
        let l = t.on_timeout(500 * MS).unwrap();
        assert_eq!(l.action, ButtonAction::LongPress as i32);
        assert_eq!(t.on_timeout(900 * MS), None, "sent once");
        assert_eq!(t.next_deadline(), None);
        // Autorepeat is ignored.
        assert!(t.key(2, 600 * MS, false).1.is_empty());
        let (_, f) = t.key(KEY_UP, 2000 * MS, false);
        assert_eq!(actions(&f), vec![ButtonAction::Release]);
    }

    #[test]
    fn stray_events_are_ignored() {
        let mut t = ButtonTracker::new(ButtonId::Camera);
        assert!(t.key(KEY_UP, 0, false).1.is_empty());
        t.key(KEY_DOWN, 0, false);
        // A second down without an up does not restart the press.
        assert!(!t.key(KEY_DOWN, 300 * MS, false).0);
        assert_eq!(t.next_deadline(), Some(500 * MS));
        t.cancel();
        assert!(t.key(KEY_UP, 400 * MS, false).1.is_empty());
    }
}
