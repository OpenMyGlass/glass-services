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

> **Changed in milestone 3 (2026-10-22):** Android 4.4 is now a target (the roadmap's Track A is the near-term build). The Android build uses NDK r25c, the last NDK with API 19, and targets API 19, so one binary serves Track A (4.4) and Track B (6.0). The API 22 build with the runner's NDK stays in CI as a check of current toolchains.

~~Android 4.4 (API 19) is not a target: the open ROM is built on Android 5.1 or 6.0 (API 22 or 23). If a KitKat build is ever needed, the static musl build of the daemon runs there unchanged.~~

### Process model

- One daemon, `glassd`, with one module per source: touchpad, head (IMU), glasshub, screen policy. The modules are libraries (crates) with pure logic separated from device I/O, so the logic is tested on the host with recorded or synthetic event streams.
- Clients (the launcher, apps, the Android glue) connect to a Unix stream socket. The interface is in [`../api.md`](../api.md).
- Framing and encoding follow OpenMyGlass's bridge protocol: a varint32 length followed by one protobuf message. Rust generates its types from the `.proto` at build time with `prost-build` and `protox` (a pure-Rust protobuf compiler, so no `protoc` is needed), so the Rust types cannot drift from the `.proto`. The Java glue (milestone 3) uses a small hand-written codec for the dozen fields it needs instead of protobuf-lite, so its build needs no `protoc` and no runtime library; an end-to-end test against `glassd --mock` in CI catches drift.
- `glassd --mock` (and the in-process mock in `glass-api`) serves the same socket with scripted or keyboard-driven events, so apps are built and tested without a Glass.

### Crates

| Crate | Content | Milestone |
|---|---|---|
| `glass-api` | Protocol messages, framing codec, a blocking client, and the in-process mock server | M1 |
| `glass-gesture` | Touchpad gesture recognizer: evdev events in, gestures out; pure, no I/O | M1 |
| `glass-evdev` | Minimal evdev reader (`input_event` parsing, `EVIOCGNAME`, `EVIOCGABS`) over `libc` | M1 |
| `glassd` | The daemon: `poll` loop, device discovery, socket server, `--mock` | M1 (touchpad only) |
| `glass-head` | Head gestures (look up to wake, nudge, relative look-up) from IMU samples | M2 |
| `glass-hub` | glasshub: on-head, wink, through the kernel driver's interface; the on-head state machine | M2 |
| `glass-screen` | Wake and sleep policy (timeouts, wake sources, off-head) as a pure state machine, plus the display back ends. Done in M1 and M2 as modules of `glassd` (`screen.rs`, `display.rs`) instead of a crate of its own | M2 |
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

## Milestone 2

Split into pull requests that each build and pass on their own.

1. [x] `glass-hub`: sysfs access (presence by `version`, `poll` on `don_doff` and `wink`, enables, thresholds, an allow-list of writable attributes) and the pure on-head state machine (sensor, forced, power key, hinge; 30 s forced period).
2. [x] Buttons: camera (`gpio-keys`) and power (`twl6030_pwrbutton`) by device name; press, long press, release; a short power press toggles the manual off-head.
3. [x] Back ends: `FBIOBLANK` on the panel framebuffer, touchpad `sleepmode` on off-head and don.
4. [x] Settings stored in a file, written atomically, read at start.
5. [x] `glass-head`: IIO buffer reader for the MPU-9150 (scan elements, the Glass format byte, timestamps, scales, board matrix), look-up and look-down from the accelerometer, our nudge detector and relative look-up from the gyroscope, inhibition windows, sensors run only when needed; `HeadRequest` in the interface.
6. [x] Mock commands for buttons, hinge, wear and wink through the same logic as the hardware; capabilities only for the sources found.

## Milestone 3: Android glue

1. [x] Companion link provider and dictation relay in the interface (`docs/api.md`, `TOPIC_COMPANION`, the `provider_*` frames), as pure state in `glass-api` used by the hub; a simulated provider in `glassd --mock`.
2. [x] Android specifics in `glassd`: the socket from init (`ANDROID_SOCKET_glassd`), the peer check by uid with `packages.list` (Android has no group we can add without changing `system/core`; decision below), `logcat` output.
3. [x] The glue (`android/glue/`): `ScreenState` to `PowerManager`, framework screen changes back to `glassd`, key events for regular apps while it holds the focus, shutdown on a long power-key press. JVM tests, end to end against `glassd --mock`.
4. [x] Packaging: a prebuilt directory for `vendor/openmyglass/glass-services` (`Android.mk`, product makefile, init rc, key layout, IDC, 6.0 SELinux policy), built by `tools/android-dist.sh` and CI's `android-dist` job. The device tree includes it (lines in `android/README.md`); this repository does not edit the device tree.
5. Not in this milestone: head gestures from Android's sensors (the IMU ownership decision is still open), the stock binder services for stock Glass apps, a location provider (M4).

Decisions for the owner:
- **Socket access on Android:** the socket is 0666 and `glassd` checks the peer's uid (root, system, the launcher and the OpenMyGlass Glass app by package name). The alternative is an AID `glass` added to `system/core` (a fork change in the manifest), which would let the socket be 0660 again.
- **The provider:** the OpenMyGlass Glass app (`org.openmyglass.glass`) must register as the companion link provider for dictation to work; that change belongs to the OpenMyGlass repository.

## Later milestones

- **M4:** location feed from the phone (owner decision 2026-10-22: the GPS chip stays asleep and no GPS HAL is written): relay of the bridge's location messages through the companion link provider, `TOPIC_LOCATION` in glassd, mock commands; in the Android glue, a location provider fed from it.
- **M5:** route 2 packaging (OpenRC service for Alpine, udev-free device discovery).

## Device checklist (not run: no Glass available)

- [ ] `glassd` finds the touchpad by name and reads its axis ranges.
- [ ] Taps, swipes in both directions, swipe down, two-finger tap and swipe are recognized with the stock feel.
- [ ] The daemon uses no CPU when idle (`top`), and less than 2 % of one core while a finger moves.
- [ ] Two clients: the focus moves to the second and back when it disconnects; the first gets a cancelled `Scroll` if the hand-off happens mid-touch.
- [ ] A tap on a dark screen wakes it and selects nothing.
- [ ] evdev timestamps are monotonic after `EVIOCSCLOCKID`.

### Milestone 2 (not run: no Glass available)

- [ ] The glasshub is at `/sys/bus/i2c/devices/4-0035/` and `version` reads 1.12 or later; the thresholds are written at start (read them back).
- [ ] `don_doff` reads 1 on the head and 0 off it, and `poll` wakes on each change (`on_head_detection_enabled` true); taking the Glass off turns the screen off and writes touchpad `sleepmode` 1, putting it on wakes the screen with the 3 s timeout.
- [ ] With `wink_enabled`, a wink raises the `wink` notification and a `Wink` frame; note the false-positive rate without a CPU classifier.
- [ ] The camera key (`gpio-keys`, `KEY_CAMERA`) gives press, long press and release, and wakes the screen; the power key (`twl6030_pwrbutton`) short press toggles off-head and on-head (`WEAR_SOURCE_POWER_KEY`). On route 1 Android's own power-key handling must be kept out of the short press first (milestone 3).
- [ ] `FBIOBLANK` (`--display fb`) turns the panel off and on (find which fb drives the panel); nothing writes the brightness attributes. Decision: on route 1 Android's framework owns blanking and `glassd` defaults to `--display log`; the M3 glue maps `ScreenState` to `PowerManager`.
- [ ] Does `don_doff` already hold the right state right after `don_doff_enable` = 1 or `pause` = 0, or does the firmware need a moment (and then notify)? `glassd` reads it again at once.
- [ ] The settings survive a restart of `glassd` (`/data/glassd/settings`).
- [ ] The daemon still uses no CPU when idle with the glasshub and buttons open.
- [ ] The IMU is the IIO device `mpu9150`; dump a few records with gyro and accel enabled and check the layout (24 bytes, format byte 0x06 at offset 15, timestamp at 16) and the scales.
- [ ] Axes: with the board matrix applied, +y is up with the head level, looking up gives positive pitch, and gyro x is positive when tilting the head up; roll stays under 10° when looking straight up.
- [ ] Look-up wakes the screen at the set angle (10° to 40°) and not when starting with the head up; look-down re-arms it.
- [ ] Nudge: count detections and false positives over normal use (walking, talking, turning the head) and tune `NudgeConfig`.
- [ ] Relative look-up fires at about 17° with a client's `HeadRequest`.
- [ ] Power: with head wake on and the screen off, the 10 Hz accelerometer interrupt wakes the CPU; measure the battery drain and whether Android still suspends. Without the DMP firmware this is the cost of look-up to wake (owner decision if it is too high).
- [ ] The IMU stops (`buffer/enable` 0, `power_state` 0) off the head, when no detector runs, at start and when `glassd` exits.
- [ ] The scales read back after `power_state` 1 match the driver defaults (±2 g, ±2000 dps).
- [ ] Milestone 3: the Android checklist in [`android/README.md`](../../android/README.md) (glassd at boot, peer check, screen sync with `PowerManager`, key injection, power key, SELinux on 6.0).
- [ ] Open decision for the owner: on route 1 the IMU belongs to Android's sensors HAL, so `glassd` defaults to `--head off` on Android and head gestures would come through the M3 glue from Android sensors; alternatively the HAL is dropped and `glassd` takes the IMU (`--head on`). On route 2 `glassd` owns it.
