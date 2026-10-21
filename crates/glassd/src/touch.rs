//! Between the recognizer and the clients: applies the screen policy to touches
//! (docs/api.md "Waking touch" and "Screen").
//!
//! - Screen on: the touch is delivered and counts as user activity, except a two-finger
//!   `SWIPE_DOWN`, which turns the screen off and is not delivered.
//! - Screen dim: the touch restores the screen and is consumed.
//! - Screen off: nothing is delivered. Encoded screen-off frames (route 1) wake on tap and
//!   forward or backward swipe; with [`ScreenOffInput::Touches`] (route 2, no encoded
//!   frames) the recognizer's own tap and horizontal swipes do the same.
//! - Off the head: touches are ignored.

use crate::screen::{Change, Now, ScreenPolicy, TouchStart};
use glass_api::proto::{GestureKind, ServiceFrame};
use glass_gesture::{Output, ScreenOffGesture};

/// How screen-off touches are recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenOffInput {
    /// Only the kernel's encoded frames (Android early suspend).
    Encoded,
    /// The recognizer classifies the real contacts (no early suspend).
    Touches,
}

/// What the daemon must do.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Publish this frame to the focus holder.
    Deliver(ServiceFrame),
    /// The screen changed.
    Screen(Change),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    Deliver,
    Consume,
    ScreenOff,
}

#[derive(Debug)]
pub struct TouchGate {
    mode: Mode,
    input: ScreenOffInput,
}

impl TouchGate {
    pub fn new(input: ScreenOffInput) -> Self {
        TouchGate {
            mode: Mode::Idle,
            input,
        }
    }

    /// Whether a touch is in progress.
    pub fn touching(&self) -> bool {
        self.mode != Mode::Idle
    }

    pub fn handle(&mut self, out: &Output, policy: &mut ScreenPolicy, now: Now) -> Vec<Step> {
        let mut steps = Vec::new();
        let push = |c: Option<Change>, steps: &mut Vec<Step>| {
            if let Some(c) = c {
                steps.push(Step::Screen(c));
            }
        };
        if let Output::ScreenOff { gesture, .. } = out {
            if policy.is_off() {
                let reason = match gesture {
                    ScreenOffGesture::Tap => Some("tap"),
                    ScreenOffGesture::SwipeForward => Some("swipe-forward"),
                    ScreenOffGesture::SwipeBackward => Some("swipe-backward"),
                    ScreenOffGesture::LongPress => None,
                };
                if let Some(r) = reason {
                    push(policy.wake_gesture(now, r), &mut steps);
                }
            }
            return steps;
        }
        let Some(frame) = out.to_frame() else {
            return steps;
        };
        let (starts, ends) = match out {
            Output::FingerCount(f) => (f.previous == 0 && f.current > 0, f.current == 0),
            _ => (false, false),
        };
        if starts || self.mode == Mode::Idle {
            let (start, change) = policy.touch_down(now);
            push(change, &mut steps);
            self.mode = match start {
                TouchStart::Deliver => Mode::Deliver,
                TouchStart::Consume | TouchStart::Ignore => Mode::Consume,
                TouchStart::ScreenOff => Mode::ScreenOff,
            };
        }
        match self.mode {
            Mode::Deliver => {
                push(policy.activity(now), &mut steps);
                if let Output::Gesture(g) = out
                    && g.kind == GestureKind::SwipeDown as i32
                    && g.fingers == 2
                {
                    push(policy.sleep(now, "two-finger-swipe-down"), &mut steps);
                } else {
                    steps.push(Step::Deliver(frame));
                }
            }
            Mode::ScreenOff => {
                if self.input == ScreenOffInput::Touches
                    && let Output::Gesture(g) = out
                {
                    let reason = match GestureKind::try_from(g.kind) {
                        Ok(GestureKind::Tap) => Some("tap"),
                        Ok(GestureKind::SwipeForward) => Some("swipe-forward"),
                        Ok(GestureKind::SwipeBackward) => Some("swipe-backward"),
                        _ => None,
                    };
                    if let Some(r) = reason {
                        push(policy.wake_gesture(now, r), &mut steps);
                    }
                }
            }
            Mode::Consume | Mode::Idle => {}
        }
        if ends {
            self.mode = Mode::Idle;
        }
        steps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_api::proto::{Screen, service_frame};
    use glass_gesture::synth::{Direction, Synth};
    use glass_gesture::{Config, Recognizer};

    const S: u64 = 1_000_000;

    struct Rig {
        rec: Recognizer,
        gate: TouchGate,
        policy: ScreenPolicy,
        t: u64,
    }

    impl Rig {
        fn new(input: ScreenOffInput) -> Self {
            Rig {
                rec: Recognizer::new(Config::default()),
                gate: TouchGate::new(input),
                policy: ScreenPolicy::new(Now::at(0), 15_000),
                t: S,
            }
        }

        fn run(&mut self, build: impl FnOnce(&mut Synth)) -> Vec<Step> {
            let mut s = Synth::new(&Config::default(), self.t);
            build(&mut s);
            self.t = s.time_us() + 100_000;
            self.rec.set_screen_off_decoding(self.policy.is_off());
            let mut steps = Vec::new();
            for o in self.rec.feed(&s.take()) {
                steps.extend(self.gate.handle(&o, &mut self.policy, Now::at(self.t)));
            }
            steps
        }
    }

    fn delivered(steps: &[Step]) -> Vec<service_frame::Body> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Deliver(f) => f.body.clone(),
                _ => None,
            })
            .collect()
    }

    fn changes(steps: &[Step]) -> Vec<(Screen, &'static str)> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Screen(c) => Some((c.screen, c.reason)),
                _ => None,
            })
            .collect()
    }

    fn has_gesture(steps: &[Step]) -> bool {
        delivered(steps)
            .iter()
            .any(|b| matches!(b, service_frame::Body::Gesture(_)))
    }

    #[test]
    fn screen_on_delivers() {
        let mut r = Rig::new(ScreenOffInput::Touches);
        let steps = r.run(|s| s.tap(1, 50));
        assert!(has_gesture(&steps));
        assert_eq!(delivered(&steps).len(), 3);
        assert!(changes(&steps).is_empty());
    }

    #[test]
    fn two_finger_swipe_down_sleeps_and_is_not_delivered() {
        let mut r = Rig::new(ScreenOffInput::Touches);
        let steps = r.run(|s| s.swipe(Direction::Down, 2, 80.0, 200));
        assert!(!has_gesture(&steps));
        assert_eq!(
            changes(&steps),
            vec![(Screen::Off, "two-finger-swipe-down")]
        );
        // The finger counts of that touch are still delivered (the touch began on).
        assert!(delivered(&steps).len() >= 2);
        // One finger swipe down is delivered.
        let mut r = Rig::new(ScreenOffInput::Touches);
        assert!(has_gesture(&r.run(|s| s.swipe(
            Direction::Down,
            1,
            80.0,
            200
        ))));
    }

    #[test]
    fn dim_touch_is_consumed() {
        let mut r = Rig::new(ScreenOffInput::Touches);
        r.policy.force_dim(Now::at(S));
        let steps = r.run(|s| s.tap(1, 50));
        assert!(delivered(&steps).is_empty());
        assert_eq!(changes(&steps), vec![(Screen::On, "activity")]);
        // The next touch is delivered.
        assert!(has_gesture(&r.run(|s| s.tap(1, 50))));
    }

    #[test]
    fn screen_off_tap_wakes_and_is_consumed_route2() {
        let mut r = Rig::new(ScreenOffInput::Touches);
        r.policy.sleep(Now::at(S), "request");
        let steps = r.run(|s| s.tap(1, 50));
        assert!(delivered(&steps).is_empty());
        assert_eq!(changes(&steps), vec![(Screen::On, "tap")]);
        // Other touches do not wake.
        r.policy.sleep(Now::at(r.t), "request");
        let steps = r.run(|s| s.swipe(Direction::Down, 1, 80.0, 200));
        assert!(changes(&steps).is_empty() && delivered(&steps).is_empty());
        let steps = r.run(|s| s.swipe(Direction::Backward, 1, 400.0, 100));
        assert_eq!(changes(&steps), vec![(Screen::On, "swipe-backward")]);
    }

    #[test]
    fn screen_off_route1_uses_only_encoded_frames() {
        let mut r = Rig::new(ScreenOffInput::Encoded);
        r.policy.sleep(Now::at(S), "request");
        let steps = r.run(|s| s.tap(1, 50));
        assert!(changes(&steps).is_empty() && delivered(&steps).is_empty());
        let steps = r.run(|s| s.encoded(9, 0));
        assert_eq!(changes(&steps), vec![(Screen::On, "swipe-forward")]);
        assert!(delivered(&steps).is_empty());
        // Encoded long press does not wake.
        r.policy.sleep(Now::at(r.t), "request");
        assert!(changes(&r.run(|s| s.encoded(1, 1))).is_empty());
    }

    #[test]
    fn off_head_ignores_touches() {
        let mut r = Rig::new(ScreenOffInput::Touches);
        r.policy.set_on_head(Now::at(S), false);
        let steps = r.run(|s| s.tap(1, 50));
        assert!(changes(&steps).is_empty() && delivered(&steps).is_empty());
    }

    #[test]
    fn touch_counts_as_activity() {
        let mut r = Rig::new(ScreenOffInput::Touches);
        r.t = 10 * S;
        r.run(|s| s.tap(1, 50));
        assert!(r.policy.next_deadline().unwrap() > 20 * S);
    }
}
