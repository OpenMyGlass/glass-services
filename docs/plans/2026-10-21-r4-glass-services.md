# R4: Glass system services (plan)

Roadmap phase R4 of OpenMyGlass ([glass-rom roadmap](https://github.com/OpenMyGlass/glass-rom/blob/main/docs/plans/2026-10-21-open-source-roadmap.md)): touchpad gestures, head gestures, on-head and wink detection, and the screen wake and sleep policy, written as open, clean-room components.

## Constraints

- **No Android dependency in the services.** The same code must run on route 1 (open Android) and route 2 (plain Linux, Alpine on the stock 3.4 kernel). Only the Linux kernel interfaces are shared by both routes: evdev (`/dev/input/event*`), sysfs, and character devices. The services use only those.
- **Android glue in its own layer** (`android/`, later milestone): an init service entry, SELinux policy if needed, and a small Java system service or app that turns our events into what Android expects (key events for legacy apps, `PowerManager` wake and sleep, settings). It talks to the daemon over the same socket as any other client.
- **Clean room.** Facts come from:
  1. the GPL kernel drivers (read directly, `docs/specs/kernel-interfaces.md`);
  2. the reverse-engineering specs in `docs/specs/` (to be written by a separate agent who studied the stock libraries; the implementers read only the specs);
  3. Glass behaviour as publicly documented (GDK reference, Glass design guidelines).
- **Hardware:** OMAP4430, two Cortex-A9 at up to 1 GHz. The daemon must be idle when nothing happens (block on `poll`, no busy timers), and cheap when the touchpad reports at its full rate.
- **Safety (rule 6):** nothing here touches voltages, frequencies, thermal limits or charging. The screen policy only blanks and unblanks the display and holds wake locks.

## Decisions

### Language: Rust

| Option | For | Against |
|---|---|---|
| **Rust** (chosen) | Memory safety in a long-running daemon that parses device input and socket frames; unit tests built in; one `cargo` command cross-builds to `armv7-linux-androideabi` and `armv7-unknown-linux-musleabihf`; a static musl binary also runs on Android, since it does not need bionic | Larger binaries than C (about 1 MB stripped); NDK linking for Android targets API 21 and up |
| C | Smallest binaries; AOSP builds it natively | Parsing and state machines without memory safety; tests need a separate framework; two build systems (Android.mk and meson) |

Android 4.4 (API 19) is not a target: the open ROM is built on Android 5.1 or 6.0 (API 22 or 23). If a KitKat build is ever needed, the static musl build of the daemon runs there unchanged.

### Process model

- One daemon, `glassd`, with one module per source: touchpad, head (IMU), glasshub, screen policy. The modules are libraries (crates) with pure logic separated from device I/O, so the logic is tested on the host with recorded or synthetic event streams.
- Clients (the launcher, apps, the Android glue) connect to a Unix stream socket. The interface is in [`../api.md`](../api.md).
- Framing and encoding follow OpenMyGlass's bridge protocol: a varint32 length followed by one protobuf message. Rust generates its types from the `.proto` at build time with `prost-build` and `protox` (a pure-Rust protobuf compiler, so no `protoc` is needed), so the Rust types cannot drift from the `.proto`; Java glue uses protobuf-lite with the same file.
- `glassd --mock` (and the in-process mock in `glass-api`) serves the same socket with scripted or keyboard-driven events, so apps are built and tested without a Glass.

### Crates

| Crate | Content | Milestone |
|---|---|---|
| `glass-api` | Protocol messages, framing codec, a blocking client, and the in-process mock server | M1 |
| `glass-gesture` | Touchpad gesture recognizer: evdev events in, gestures out; pure, no I/O | M1 |
| `glass-evdev` | Minimal evdev reader (`input_event` parsing, `EVIOCGNAME`, `EVIOCGABS`) over `libc` | M1 |
| `glassd` | The daemon: `poll` loop, device discovery, socket server, `--mock` | M1 (touchpad only) |
| `glass-head` | Head gestures (look up to wake, nudge, relative look-up) from IMU samples | M2 |
| `glass-hub` | glasshub: on-head, wink, through the kernel driver's interface | M2 |
| `glass-screen` | Wake and sleep policy (timeouts, wake sources, off-head) as a pure state machine, plus the display back ends | M2 |
| `android/` | Android glue | M3 |

### CI (GitHub Actions)

- Host: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on Linux.
- Cross-build: `armv7-linux-androideabi` (NDK, API 22) and `armv7-unknown-linux-musleabihf` (static). The cross-built binaries are not run in CI.
- Required checks in the `protect-main` ruleset: the job names of the workflow.

## Milestone 1 (this plan)

1. `docs/api.md`: the services and apps interface, version 1.
2. `docs/specs/`: kernel interfaces (GPL source) and the RE specs.
3. `glass-api`: messages, codec, client and mock, with tests (round trips, partial frames, oversize frames).
4. `glass-gesture`: recognizer with tests for tap, long press, swipe forward, back and down, two-finger tap and swipe, three-finger tap, scroll displacement, cancellation, and noise near the thresholds.
5. `glass-evdev` and `glassd` reading a touchpad, recognizing gestures and serving them over the socket; host test with a fake device stream.
6. CI as above, and the ruleset.

## Later milestones

- **M2:** head gestures, glasshub (on-head, wink), screen policy; device checklist.
- **M3:** Android glue (init service, system app, key event injection for legacy apps, `PowerManager` wake and sleep).
- **M4:** location feed from the phone (owner decision 2026-10-22: the GPS chip stays asleep and no GPS HAL is written): relay of the bridge's location messages through the companion link provider, `TOPIC_LOCATION` in glassd, mock commands; in the Android glue, a location provider fed from it.
- **M5:** route 2 packaging (OpenRC service for Alpine, udev-free device discovery).

## Device checklist (not run: no Glass available)

- [ ] `glassd` finds the touchpad by name and reads its axis ranges.
- [ ] Taps, swipes in both directions, swipe down, two-finger tap and swipe are recognized with the stock feel.
- [ ] The daemon uses no CPU when idle (`top`), and less than 2 % of one core while a finger moves.
- [ ] Two clients: the focus moves to the second and back when it disconnects; the first gets a cancelled `Scroll` if the hand-off happens mid-touch.
- [ ] A tap on a dark screen wakes it and selects nothing.
- [ ] evdev timestamps are monotonic after `EVIOCSCLOCKID`.
