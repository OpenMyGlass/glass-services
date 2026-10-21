use glass_api::proto::{GestureKind, Scroll};
use glass_gesture::event::{RawEvent, abs, ev, syn};
use glass_gesture::synth::{Direction, Synth};
use glass_gesture::{Config, Output, Recognizer, ScreenOffGesture};

const T0: u64 = 1_000_000_000;

fn cfg() -> Config {
    Config::default()
}

fn run(cfg: &Config, build: impl FnOnce(&mut Synth)) -> Vec<Output> {
    let mut s = Synth::new(cfg, T0);
    build(&mut s);
    let mut r = Recognizer::new(cfg.clone());
    r.feed(&s.take())
}

fn gestures(out: &[Output]) -> Vec<(GestureKind, u32)> {
    out.iter()
        .filter_map(|o| match o {
            Output::Gesture(g) => Some((GestureKind::try_from(g.kind).unwrap(), g.fingers)),
            _ => None,
        })
        .collect()
}

fn scrolls(out: &[Output]) -> Vec<Scroll> {
    out.iter()
        .filter_map(|o| match o {
            Output::Scroll(s) => Some(*s),
            _ => None,
        })
        .collect()
}

fn counts(out: &[Output]) -> Vec<(u32, u32)> {
    out.iter()
        .filter_map(|o| match o {
            Output::FingerCount(f) => Some((f.previous, f.current)),
            _ => None,
        })
        .collect()
}

fn gesture_velocity(out: &[Output]) -> f32 {
    out.iter()
        .find_map(|o| match o {
            Output::Gesture(g) => Some(g.velocity),
            _ => None,
        })
        .unwrap()
}

#[test]
fn single_tap() {
    let out = run(&cfg(), |s| s.tap(1, 80));
    assert_eq!(gestures(&out), vec![(GestureKind::Tap, 1)]);
    assert_eq!(counts(&out), vec![(0, 1), (1, 0)]);
    // Order: finger down, tap, finger up; tap time is the lift.
    assert!(matches!(out[0], Output::FingerCount(_)));
    let Output::Gesture(g) = &out[1] else {
        panic!("{out:?}")
    };
    assert_eq!(g.timestamp_us, T0 + 80_000);
    assert!(matches!(out[2], Output::FingerCount(_)));
    assert!(scrolls(&out).is_empty());
}

#[test]
fn two_and_three_finger_taps() {
    assert_eq!(
        gestures(&run(&cfg(), |s| s.tap(2, 80))),
        vec![(GestureKind::Tap, 2)]
    );
    assert_eq!(
        gestures(&run(&cfg(), |s| s.tap(3, 80))),
        vec![(GestureKind::Tap, 3)]
    );
}

#[test]
fn second_finger_landing_late_makes_a_two_finger_tap() {
    let out = run(&cfg(), |s| {
        s.frame(&[(0.0, 0.0)]);
        s.advance_ms(30);
        s.frame(&[(0.0, 0.0), (60.0, 0.0)]);
        s.advance_ms(30);
        s.frame(&[(0.0, 0.0)]);
        s.advance_ms(30);
        s.lift();
    });
    assert_eq!(gestures(&out), vec![(GestureKind::Tap, 2)]);
    assert_eq!(counts(&out), vec![(0, 1), (1, 2), (2, 1), (1, 0)]);
}

#[test]
fn tap_noise_near_the_slop() {
    let c = cfg();
    let jitter = |d: f32| {
        run(&c, move |s| {
            s.frame(&[(0.0, 0.0)]);
            s.advance_ms(20);
            s.frame(&[(d * 0.7, d * 0.7)]);
            s.advance_ms(20);
            s.frame(&[(d, 0.0)]);
            s.advance_ms(20);
            s.lift();
        })
    };
    assert_eq!(
        gestures(&jitter(c.touch_slop - 1.0)),
        vec![(GestureKind::Tap, 1)]
    );
    assert!(gestures(&jitter(c.touch_slop + 2.0)).is_empty());
}

#[test]
fn three_fingers_get_twice_the_slop() {
    let c = cfg();
    let moved = |fingers: u32, d: f32| {
        run(&c, move |s| {
            s.stroke(fingers, (0.0, 0.0), (0.0, d), 40);
            s.lift();
        })
    };
    let d = c.touch_slop * 1.5;
    assert_eq!(gestures(&moved(3, d)), vec![(GestureKind::Tap, 3)]);
    // One finger moving that far is no tap (it is a short vertical swipe).
    assert_eq!(gestures(&moved(1, d)), vec![(GestureKind::SwipeDown, 1)]);
    // Beyond 2x slop with three fingers: nothing at all, not even scroll.
    let out = moved(3, c.touch_slop * 2.0 + 5.0);
    assert!(gestures(&out).is_empty());
    assert!(scrolls(&out).is_empty());
}

#[test]
fn long_press_by_timer() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.frame(&[(0.0, 0.0)]);
    let mut r = Recognizer::new(c.clone());
    let out = r.feed(&s.take());
    assert_eq!(counts(&out), vec![(0, 1)]);
    let deadline = r.next_deadline().unwrap();
    assert_eq!(deadline, T0 + 500_000);
    let mut out = Vec::new();
    r.on_timeout(deadline - 1, &mut out);
    assert!(out.is_empty());
    r.on_timeout(deadline, &mut out);
    assert_eq!(gestures(&out), vec![(GestureKind::LongPress, 1)]);
    let Output::Gesture(g) = &out[0] else {
        panic!()
    };
    assert_eq!(g.timestamp_us, deadline);
    assert_eq!(r.next_deadline(), None);
    // The rest of the touch produces no gesture, even when it moves.
    s.advance_ms(600);
    s.stroke(1, (0.0, 0.0), (300.0, 0.0), 50);
    s.lift();
    let out = r.feed(&s.take());
    assert!(gestures(&out).is_empty());
    assert!(scrolls(&out).is_empty());
    assert_eq!(counts(&out).last(), Some(&(1, 0)));
}

#[test]
fn long_press_fires_from_a_late_frame_without_timer() {
    let out = run(&cfg(), |s| s.tap(1, 700));
    assert_eq!(gestures(&out), vec![(GestureKind::LongPress, 1)]);
}

#[test]
fn two_finger_long_press() {
    let out = run(&cfg(), |s| s.tap(2, 700));
    assert_eq!(gestures(&out), vec![(GestureKind::LongPress, 2)]);
}

#[test]
fn second_finger_rearms_the_long_press_timer() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.frame(&[(0.0, 0.0)]);
    s.advance_ms(300);
    s.frame(&[(0.0, 0.0), (60.0, 0.0)]);
    let mut r = Recognizer::new(c);
    r.feed(&s.take());
    assert_eq!(r.next_deadline(), Some(T0 + 800_000));
}

#[test]
fn moving_cancels_the_long_press() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.stroke(1, (0.0, 0.0), (0.0, 40.0), 100);
    let mut r = Recognizer::new(c);
    r.feed(&s.take());
    assert_eq!(r.next_deadline(), None);
}

#[test]
fn swipe_forward_scroll_then_gesture() {
    let out = run(&cfg(), |s| s.swipe(Direction::Forward, 1, 400.0, 100));
    let sc = scrolls(&out);
    assert!(sc.len() >= 3, "{out:?}");
    let last = sc.last().unwrap();
    assert!(last.ended && !last.cancelled);
    assert!(sc[..sc.len() - 1].iter().all(|s| !s.ended));
    // Displacement in pad widths, increasing forward; deltas add up.
    assert!(
        sc.windows(2)
            .all(|w| w[1].displacement >= w[0].displacement)
    );
    assert!(
        last.displacement > 0.3 && last.displacement <= 0.41,
        "{last:?}"
    );
    let sum: f32 = sc.iter().map(|s| s.delta).sum();
    assert!((sum - last.displacement).abs() < 0.02, "{sum} {last:?}");
    assert!(last.velocity > 0.0);
    // Order: ..., final Scroll, Gesture, FingerCount -> 0.
    let n = out.len();
    assert!(matches!(&out[n - 3], Output::Scroll(s) if s.ended));
    assert!(matches!(out[n - 2], Output::Gesture(_)));
    assert!(matches!(&out[n - 1], Output::FingerCount(f) if f.current == 0));
    assert_eq!(gestures(&out), vec![(GestureKind::SwipeForward, 1)]);
    // 4 units/ms over a 1000-unit pad = 4 pad widths per second.
    let v = gesture_velocity(&out);
    assert!((v - 4.0).abs() < 0.3, "{v}");
}

#[test]
fn swipe_backward_has_negative_velocity_and_displacement() {
    let out = run(&cfg(), |s| s.swipe(Direction::Backward, 1, 400.0, 100));
    assert_eq!(gestures(&out), vec![(GestureKind::SwipeBackward, 1)]);
    assert!(gesture_velocity(&out) < 0.0);
    let last = *scrolls(&out).last().unwrap();
    assert!(last.displacement < -0.3 && last.velocity < 0.0);
}

#[test]
fn horizontal_velocity_thresholds_are_asymmetric() {
    let c = cfg();
    // distance / duration = speed in units per ms.
    let swipe = |dir, speed: f32| gestures(&run(&c, move |s| s.swipe(dir, 1, speed * 200.0, 200)));
    assert_eq!(
        swipe(Direction::Forward, 0.8),
        vec![(GestureKind::SwipeForward, 1)]
    );
    assert!(swipe(Direction::Forward, 0.6).is_empty());
    assert!(swipe(Direction::Backward, 0.9).is_empty());
    assert_eq!(
        swipe(Direction::Backward, 1.2),
        vec![(GestureKind::SwipeBackward, 1)]
    );
}

#[test]
fn slow_drag_scrolls_without_a_gesture() {
    let out = run(&cfg(), |s| s.drag(1, 300.0, 1000));
    assert!(gestures(&out).is_empty(), "{out:?}");
    let sc = scrolls(&out);
    assert!(sc.len() > 10);
    let last = sc.last().unwrap();
    assert!(last.ended);
    assert!((last.displacement - 0.3).abs() < 0.02);
    assert!(last.velocity.abs() < 0.1, "{last:?}");
}

#[test]
fn vertical_swipes_have_no_scroll_and_no_velocity_threshold() {
    let down = run(&cfg(), |s| s.swipe(Direction::Down, 1, 80.0, 300));
    assert_eq!(gestures(&down), vec![(GestureKind::SwipeDown, 1)]);
    assert!(scrolls(&down).is_empty());
    assert!(gesture_velocity(&down) > 0.0);
    let up = run(&cfg(), |s| s.swipe(Direction::Up, 1, 80.0, 300));
    assert_eq!(gestures(&up), vec![(GestureKind::SwipeUp, 1)]);
    assert!(gesture_velocity(&up) < 0.0);
}

#[test]
fn swipe_angle_limits() {
    let c = cfg();
    let angled = |dx: f32, deg: f32| {
        let dy = dx.abs() * deg.to_radians().tan();
        gestures(&run(&c, move |s| {
            s.stroke(1, (0.0, 0.0), (dx, dy), 100);
            s.lift();
        }))
    };
    assert_eq!(angled(400.0, 15.0), vec![(GestureKind::SwipeForward, 1)]);
    // 20 degrees: too steep forward (18), fine backward (25).
    assert!(angled(400.0, 20.0).is_empty());
    assert_eq!(angled(-400.0, 20.0), vec![(GestureKind::SwipeBackward, 1)]);
    assert!(angled(-400.0, 30.0).is_empty());
    // Mostly vertical from the start: a vertical swipe.
    assert_eq!(angled(30.0, 80.0), vec![(GestureKind::SwipeDown, 1)]);
}

#[test]
fn two_finger_swipe() {
    let out = run(&cfg(), |s| s.swipe(Direction::Forward, 2, 400.0, 100));
    assert_eq!(gestures(&out), vec![(GestureKind::SwipeForward, 2)]);
    assert!(scrolls(&out).iter().all(|s| s.fingers == 2 || s.ended));
    let down = run(&cfg(), |s| s.swipe(Direction::Down, 2, 80.0, 200));
    assert_eq!(gestures(&down), vec![(GestureKind::SwipeDown, 2)]);
}

#[test]
fn two_finger_swipe_needs_both_lifts_within_100ms() {
    let c = cfg();
    let staggered = |gap_ms: u64| {
        gestures(&run(&c, move |s| {
            s.stroke(2, (-200.0, 0.0), (400.0, 0.0), 100);
            s.advance_ms(gap_ms.min(10));
            s.frame(&[(200.0, 0.0)]);
            if gap_ms > 10 {
                s.advance_ms(gap_ms - 10);
            }
            s.lift();
        }))
    };
    assert_eq!(staggered(50), vec![(GestureKind::SwipeForward, 2)]);
    assert!(staggered(200).is_empty());
}

#[test]
fn finger_count_changes() {
    let out = run(&cfg(), |s| {
        s.frame(&[(0.0, 0.0)]);
        s.advance_ms(10);
        s.frame(&[(0.0, 0.0), (60.0, 0.0)]);
        s.advance_ms(10);
        s.frame(&[(0.0, 0.0), (60.0, 0.0), (120.0, 0.0)]);
        s.advance_ms(10);
        s.frame(&[(0.0, 0.0)]);
        s.advance_ms(10);
        s.lift();
    });
    assert_eq!(counts(&out), vec![(0, 1), (1, 2), (2, 3), (3, 1), (1, 0)]);
    assert_eq!(gestures(&out), vec![(GestureKind::Tap, 3)]);
}

#[test]
fn four_fingers_end_the_sequence() {
    let out = run(&cfg(), |s| s.tap(4, 50));
    assert!(gestures(&out).is_empty());
    assert_eq!(counts(&out), vec![(0, 4), (4, 0)]);
}

#[test]
fn orientation_flag_flips_forward() {
    // A stream where raw X increases...
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.swipe(Direction::Forward, 1, 400.0, 100);
    let events = s.take();
    // ...is backward on a pad mounted the other way round.
    let flipped = Config {
        forward_is_positive_x: false,
        ..cfg()
    };
    let out = Recognizer::new(flipped).feed(&events);
    assert_eq!(gestures(&out), vec![(GestureKind::SwipeBackward, 1)]);
    // Vertical flag likewise.
    let mut s = Synth::new(&c, T0);
    s.swipe(Direction::Down, 1, 80.0, 300);
    let events = s.take();
    let flipped = Config {
        down_is_positive_y: false,
        ..cfg()
    };
    let out = Recognizer::new(flipped).feed(&events);
    assert_eq!(gestures(&out), vec![(GestureKind::SwipeUp, 1)]);
}

fn ev_at(t: u64, type_: u16, code: u16, value: i32) -> RawEvent {
    RawEvent {
        time_us: t,
        type_,
        code,
        value,
    }
}

#[test]
fn protocol_b_slots() {
    let mut events = Vec::new();
    let mut t = T0;
    // Two fingers in slots 0 and 1 moving forward fast, then both lift.
    for i in 0..=10 {
        let x = 300 + i * 40;
        if i == 0 {
            events.push(ev_at(t, ev::ABS, abs::MT_SLOT, 0));
            events.push(ev_at(t, ev::ABS, abs::MT_TRACKING_ID, 17));
        }
        events.push(ev_at(t, ev::ABS, abs::MT_SLOT, 0));
        events.push(ev_at(t, ev::ABS, abs::MT_POSITION_X, x));
        events.push(ev_at(t, ev::ABS, abs::MT_POSITION_Y, 90));
        if i == 0 {
            events.push(ev_at(t, ev::ABS, abs::MT_SLOT, 1));
            events.push(ev_at(t, ev::ABS, abs::MT_TRACKING_ID, 18));
        }
        events.push(ev_at(t, ev::ABS, abs::MT_SLOT, 1));
        events.push(ev_at(t, ev::ABS, abs::MT_POSITION_X, x + 60));
        events.push(ev_at(t, ev::ABS, abs::MT_POSITION_Y, 90));
        events.push(ev_at(t, ev::SYN, syn::REPORT, 0));
        t += 10_000;
    }
    events.push(ev_at(t, ev::ABS, abs::MT_SLOT, 0));
    events.push(ev_at(t, ev::ABS, abs::MT_TRACKING_ID, -1));
    events.push(ev_at(t, ev::ABS, abs::MT_SLOT, 1));
    events.push(ev_at(t, ev::ABS, abs::MT_TRACKING_ID, -1));
    events.push(ev_at(t, ev::SYN, syn::REPORT, 0));
    let out = Recognizer::new(cfg()).feed(&events);
    assert_eq!(counts(&out), vec![(0, 2), (2, 0)]);
    assert_eq!(gestures(&out), vec![(GestureKind::SwipeForward, 2)]);
}

#[test]
fn protocol_b_tap_with_unchanged_positions() {
    // In protocol B, unchanged values are not repeated.
    let events = vec![
        ev_at(T0, ev::ABS, abs::MT_TRACKING_ID, 5),
        ev_at(T0, ev::ABS, abs::MT_POSITION_X, 500),
        ev_at(T0, ev::ABS, abs::MT_POSITION_Y, 90),
        ev_at(T0, ev::SYN, syn::REPORT, 0),
        ev_at(T0 + 20_000, ev::ABS, abs::MT_PRESSURE, 70),
        ev_at(T0 + 20_000, ev::SYN, syn::REPORT, 0),
        ev_at(T0 + 60_000, ev::ABS, abs::MT_TRACKING_ID, -1),
        ev_at(T0 + 60_000, ev::SYN, syn::REPORT, 0),
    ];
    // Without an ABS_MT_SLOT event the stream looks like protocol A with no SYN_MT_REPORT;
    // a single-slot B device starts in slot 0 and never sends ABS_MT_SLOT, so this must work.
    let out = Recognizer::new(cfg()).feed(&events);
    assert_eq!(gestures(&out), vec![(GestureKind::Tap, 1)]);
}

#[test]
fn contacts_without_tracking_id_are_matched_by_order() {
    let mut events = Vec::new();
    for (i, x) in [480, 482, 481].into_iter().enumerate() {
        let t = T0 + i as u64 * 20_000;
        events.push(ev_at(t, ev::ABS, abs::MT_POSITION_X, x));
        events.push(ev_at(t, ev::ABS, abs::MT_POSITION_Y, 90));
        events.push(ev_at(t, ev::SYN, syn::MT_REPORT, 0));
        events.push(ev_at(t, ev::SYN, syn::REPORT, 0));
    }
    events.push(ev_at(T0 + 80_000, ev::SYN, syn::MT_REPORT, 0));
    events.push(ev_at(T0 + 80_000, ev::SYN, syn::REPORT, 0));
    let out = Recognizer::new(cfg()).feed(&events);
    assert_eq!(gestures(&out), vec![(GestureKind::Tap, 1)]);
}

#[test]
fn screen_off_encoded_frames() {
    let c = cfg();
    let decode = |dx: i32, dy: i32| {
        let mut s = Synth::new(&c, T0);
        s.encoded(dx, dy);
        let mut r = Recognizer::new(c.clone());
        r.set_screen_off_decoding(true);
        r.feed(&s.take())
    };
    let only = |out: Vec<Output>| -> Option<ScreenOffGesture> {
        assert!(counts(&out).is_empty(), "{out:?}");
        assert!(gestures(&out).is_empty());
        out.iter().find_map(|o| match o {
            Output::ScreenOff { gesture, .. } => Some(*gesture),
            _ => None,
        })
    };
    assert_eq!(only(decode(0, 0)), Some(ScreenOffGesture::Tap));
    assert_eq!(only(decode(-1, -1)), Some(ScreenOffGesture::Tap));
    assert_eq!(only(decode(1, 1)), Some(ScreenOffGesture::LongPress));
    assert_eq!(only(decode(8, 2)), Some(ScreenOffGesture::SwipeForward));
    assert_eq!(only(decode(-9, 3)), Some(ScreenOffGesture::SwipeBackward));
    assert_eq!(only(decode(3, 3)), None);
    assert_eq!(only(decode(2, 9)), None);

    // Without decoding (screen on), the same frame is an ordinary touch.
    let mut s = Synth::new(&c, T0);
    s.encoded(0, 0);
    s.advance_ms(10);
    s.lift();
    let out = Recognizer::new(c.clone()).feed(&s.take());
    assert_eq!(gestures(&out), vec![(GestureKind::Tap, 1)]);
}

#[test]
fn screen_off_flick_follows_the_orientation_flag() {
    let c = Config {
        forward_is_positive_x: false,
        ..cfg()
    };
    let mut s = Synth::new(&c, T0);
    s.encoded(8, 0);
    let mut r = Recognizer::new(c);
    r.set_screen_off_decoding(true);
    let out = r.feed(&s.take());
    assert!(matches!(
        out[..],
        [Output::ScreenOff {
            gesture: ScreenOffGesture::SwipeBackward,
            ..
        }]
    ));
}

#[test]
fn syn_dropped_cancels_the_touch() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.stroke(1, (-200.0, 0.0), (200.0, 0.0), 50);
    let mut events = s.take();
    let t = s.time_us() + 5_000;
    events.push(ev_at(t, ev::ABS, abs::MT_POSITION_X, 999));
    events.push(ev_at(t, ev::SYN, syn::DROPPED, 0));
    events.push(ev_at(t, ev::ABS, abs::MT_POSITION_X, 1));
    events.push(ev_at(t, ev::SYN, syn::REPORT, 0));
    s.advance_ms(20);
    s.lift();
    events.extend(s.take());
    let out = Recognizer::new(c).feed(&events);
    let last = *scrolls(&out).last().unwrap();
    assert!(last.ended && last.cancelled);
    assert!(gestures(&out).is_empty());
    assert_eq!(counts(&out), vec![(0, 1), (1, 0)]);
}

#[test]
fn scroll_reports_only_when_delta_is_nonzero() {
    let out = run(&cfg(), |s| {
        s.stroke(1, (0.0, 0.0), (100.0, 0.0), 50);
        for _ in 0..5 {
            s.advance_ms(10);
            s.frame(&[(100.0, 0.0)]);
        }
        s.lift();
    });
    let sc = scrolls(&out);
    assert!(sc[..sc.len() - 1].iter().all(|s| s.delta != 0.0));
}

fn screen_off(out: &[Output]) -> Vec<ScreenOffGesture> {
    out.iter()
        .filter_map(|o| match o {
            Output::ScreenOff { gesture, .. } => Some(*gesture),
            _ => None,
        })
        .collect()
}

fn decoding(c: &Config, events: &[RawEvent]) -> Vec<Output> {
    let mut r = Recognizer::new(c.clone());
    r.set_screen_off_decoding(true);
    r.feed(events)
}

#[test]
fn encoded_frame_between_real_contacts_is_not_an_all_up() {
    // Early suspend: real contacts keep arriving, the encoded frame comes in between.
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.frame(&[(0.0, 0.0)]);
    s.advance_ms(10);
    s.encoded(0, 0);
    s.advance_ms(10);
    s.frame(&[(0.0, 0.0)]);
    s.advance_ms(10);
    s.lift();
    let out = decoding(&c, &s.take());
    assert_eq!(screen_off(&out), vec![ScreenOffGesture::Tap]);
    assert_eq!(counts(&out), vec![(0, 1), (1, 0)], "{out:?}");
    assert!(scrolls(&out).is_empty());
}

#[test]
fn encoded_contact_in_the_same_frame_as_real_ones_is_kept_out_of_tracking() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.stroke(1, (-200.0, 0.0), (100.0, 0.0), 50);
    s.advance_ms(10);
    s.frame_with_encoded(&[(-100.0, 0.0)], 9, 0);
    s.advance_ms(10);
    s.frame(&[(-90.0, 0.0)]);
    s.advance_ms(10);
    s.lift();
    let out = decoding(&c, &s.take());
    assert_eq!(screen_off(&out), vec![ScreenOffGesture::SwipeForward]);
    assert_eq!(counts(&out), vec![(0, 1), (1, 0)], "{out:?}");
    // No jump towards the encoded position (raw X 109, far behind the finger).
    let sc = scrolls(&out);
    assert!(
        sc.windows(2)
            .all(|w| w[1].displacement >= w[0].displacement),
        "{sc:?}"
    );
    assert!((sc.last().unwrap().displacement - 0.11).abs() < 0.01);
}

#[test]
fn encoded_frame_alone_starts_no_touch() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.encoded(1, 1);
    s.advance_ms(10);
    s.lift();
    let out = decoding(&c, &s.take());
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(screen_off(&out), vec![ScreenOffGesture::LongPress]);
}

#[test]
fn a_repeated_encoded_frame_is_reported_once() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.encoded(0, 0);
    s.advance_ms(10);
    s.encoded(0, 0);
    s.advance_ms(10);
    s.lift();
    s.advance_ms(10);
    s.encoded(-1, -1);
    let out = decoding(&c, &s.take());
    assert_eq!(
        screen_off(&out),
        vec![ScreenOffGesture::Tap, ScreenOffGesture::Tap]
    );
}

#[test]
fn syn_dropped_ignores_the_rest_of_the_touch() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.frame(&[(0.0, 0.0)]);
    let mut events = s.take();
    let t = s.time_us() + 5_000;
    events.push(ev_at(t, ev::SYN, syn::DROPPED, 0));
    events.push(ev_at(t, ev::SYN, syn::REPORT, 0));
    s.advance_ms(20);
    s.frame(&[(0.0, 0.0)]);
    s.advance_ms(20);
    s.lift();
    events.extend(s.take());
    let out = Recognizer::new(c).feed(&events);
    assert!(gestures(&out).is_empty(), "{out:?}");
    assert_eq!(counts(&out), vec![(0, 1), (1, 0)]);
}

#[test]
fn bare_report_after_an_encoded_frame_is_not_an_all_up() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.frame(&[(0.0, 0.0)]);
    s.advance_ms(10);
    s.encoded(0, 0);
    s.bare_report();
    s.advance_ms(10);
    s.frame(&[(0.0, 0.0)]);
    s.advance_ms(10);
    s.lift();
    let out = decoding(&c, &s.take());
    assert_eq!(screen_off(&out), vec![ScreenOffGesture::Tap]);
    assert_eq!(counts(&out), vec![(0, 1), (1, 0)], "{out:?}");
}

#[test]
fn partial_frame_right_after_an_encoded_frame_is_skipped() {
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.frame(&[(0.0, 0.0), (60.0, 0.0)]);
    s.advance_ms(10);
    s.encoded(1, 1);
    s.frame(&[(0.0, 0.0)]); // partial: one of the two fingers only
    s.advance_ms(10);
    s.frame(&[(0.0, 0.0), (60.0, 0.0)]);
    s.advance_ms(10);
    s.lift();
    let out = decoding(&c, &s.take());
    assert_eq!(counts(&out), vec![(0, 2), (2, 0)], "{out:?}");
}

#[test]
fn bare_report_never_ends_a_protocol_a_touch() {
    // A frame with no SYN_MT_REPORT carries no contact data (a switch event, for example).
    let c = cfg();
    let mut s = Synth::new(&c, T0);
    s.frame(&[(0.0, 0.0)]);
    s.advance_ms(10);
    s.bare_report();
    s.advance_ms(10);
    s.frame(&[(0.0, 0.0)]);
    s.advance_ms(10);
    s.lift();
    let out = Recognizer::new(c).feed(&s.take());
    assert_eq!(counts(&out), vec![(0, 1), (1, 0)], "{out:?}");
    assert_eq!(gestures(&out), vec![(GestureKind::Tap, 1)]);
}
