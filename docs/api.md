# Glass services interface (API version 1)

How the launcher, apps and the Android glue talk to `glassd`. The messages are defined in [`proto/glass/services/v1/services.proto`](../proto/glass/services/v1/services.proto); this page gives their meaning. The Rust types, codec, client and in-process mock are in the `glass-api` crate.

Nothing in this interface depends on Android: it is the same on route 1 (open Android) and route 2 (plain Linux).

## Transport

| | |
|---|---|
| Socket | Unix stream socket. Path from `$GLASS_SERVICES_SOCKET`, else `/dev/socket/glassd` on Android (created by init) and `/run/glassd/glassd.sock` on Linux. |
| Framing | varint32 length, then one protobuf message: `ClientFrame` from client to `glassd`, `ServiceFrame` back. The same framing as the OpenMyGlass bridge protocol. |
| Frame limit | 64 KiB in both directions; both sides enforce it. A longer length, or a varint longer than 5 bytes, is a protocol error: `glassd` sends `ServiceError{ERROR_CODE_BAD_FRAME}` and closes the connection; a client closes the connection. |
| Time | Every `timestamp_us` is `CLOCK_MONOTONIC` in microseconds (it does not advance during suspend). For input, `glassd` sets `EVIOCSCLOCKID(CLOCK_MONOTONIC)` on each evdev device (available since Linux 3.4) so the kernel's event time is used directly. |
| Backpressure | Each client has an outgoing queue of 256 KiB. When it fills, consecutive `Scroll` frames are merged (displacement and velocity of the newest, deltas summed); a `Scroll` with `ended` is never merged away and no other frame is ever dropped; if the queue stays full for 2 s the client is disconnected. A slow client never delays the others. |

### Who may connect

- **Linux (route 2):** socket mode `0660`, group `glass`. The launcher and apps run as users in that group.
- **Android (route 1):** socket `/dev/socket/glassd`, mode `0660`, owner `system`, group `glass` (a group added by our ROM). Our own native apps (the launcher and the other glass-apps, started by the launcher wrapper) run with that group. A regular Android app cannot connect: it goes through the Android glue, which is a client itself and turns events into Android input and broadcasts. The SELinux policy needs a `glassd` domain, a `glassd_socket` type and `connectto` rules for those clients (milestone 3).
- Peers are not authenticated beyond the socket permissions in version 1. `glassd` logs each peer's uid and pid (`SO_PEERCRED`).

## Session

1. The client connects and sends `Hello{api_version, client_name}`.
2. `glassd` answers `Welcome{api_version, service_version, capabilities}`, where `api_version` is the lower of the client's and the service's: an older client gets the older behaviour, a newer client learns which version it is talking to and decides whether to continue. `api_version` 0 is `ERROR_CODE_BAD_FRAME`.
   - A frame before `Hello` is `ERROR_CODE_BAD_FRAME` and the connection is closed.
   - A second `Hello` is answered with `Welcome` again and changes nothing else.
3. The client sends `Subscribe{topics}`. Each `Subscribe` replaces the previous set. After every `Subscribe`, `glassd` sends a **snapshot** for each topic in the new set that has state: `WearState` and `Settings` (`snapshot: true`), `ScreenState` (`reason: "snapshot"`), and for `TOPIC_LOCATION` the current `LocationStatus` plus the last fix if there is one (`Location{snapshot: true}`). Snapshot timestamps are the time of the last change. A topic whose source does not exist sends no snapshot.
4. `GetState` sends the same snapshot for the currently subscribed topics, plus `FocusState` for this client.

Capabilities say which sources exist on this device or build: `touchpad`, `head`, `wear`, `wink`, `screen`, `button-camera`, `button-power`, `hinge`, `dictation`, `location`, and `mock` when the server is the mock. A topic whose source is missing is accepted and stays silent. `glassd` advertises a hardware source only when it found it at start: `touchpad`, `button-camera` and `button-power` by input device name, `wink` when the glasshub is present, `head` when the IMU is present; `screen` and `wear` are always there (without a glasshub the wear is forced or set with the power key); `hinge` only in the mock until the hinge switch is identified on a device.

### Requests and errors

- `ClientFrame.seq` is optional; `glassd` puts it in `ServiceError.seq` when a frame causes an error, so a client can tell which request failed.
- A frame with no known body (an empty frame, or a request newer than this `glassd`) gets `ERROR_CODE_UNSUPPORTED`.
- A known request with an invalid value gets `ERROR_CODE_INVALID_ARGUMENT`: `ACTION_UNSPECIFIED`, an unknown dictation `purpose`, a missing `notification_key` for `"notification-reply"`. In `Subscribe`, `TOPIC_UNSPECIFIED` and unknown topic values are ignored.
- `SetSettings` is always answered with the full `Settings` to the sender, once even if it is subscribed to `TOPIC_SETTINGS`; the other subscribers get it too.
- Unknown fields in a known message are ignored (normal protobuf rules), so fields can be added without a version bump. A change that alters the meaning of an existing field goes into a new package `glass.services.v2`; `glassd` may serve both. This rule applies from the first release; until then version 1 is still being aligned with the specs.

| `ErrorCode` | Meaning |
|---|---|
| `ERROR_CODE_BAD_FRAME` | malformed or oversize frame, no `Hello` first, `api_version` 0; the connection is closed |
| `ERROR_CODE_UNSUPPORTED` | a request this build does not know or cannot serve |
| `ERROR_CODE_INVALID_ARGUMENT` | a known request with an invalid value |
| `ERROR_CODE_UNAVAILABLE` | the source exists but failed (for example the device was removed) |
| `ERROR_CODE_NO_COMPANION` | dictation requested while no companion link is registered (location reports this through `LocationStatus` instead) |

## Input focus

Touchpad events (`Gesture`, `Scroll`, `FingerCount`) go to **one** client: the focus holder.
- `AcquireInputFocus` puts the sender on top of the focus stack (moving it there if it is already in the stack). The previous holder gets `FocusState{focused:false}`, the new one `FocusState{focused:true}`.
- `ReleaseInputFocus` or a disconnect removes the client from the stack wherever it is; if it was on top, the next client down gets `FocusState{focused:true}`. A client removed from the middle of the stack is told nothing.
- With nobody holding the focus, touchpad events are dropped. A focus holder that is not subscribed to `TOPIC_GESTURE` gets nothing, and the events are not passed down the stack.
- When the focus changes during a touch, the old holder gets a final `Scroll{ended:true, cancelled:true}` (if it had received any `Scroll`) and no gesture for that touch; the new holder receives events from the next touch down.
- Any client may take the focus in version 1. The launcher takes it at start; an app that it opens takes it while it is in front. On Android, the glue holds the focus while a regular Android activity is in front.

All other topics are broadcast to every subscriber.

## Touchpad gestures (`TOPIC_GESTURE`)

The touchpad is on the right temple. Directions are named as on Glass (GDK `SWIPE_RIGHT` is forward, `SWIPE_LEFT` is backward):

| Kind | Finger motion | Typical use (Glass design guidelines) |
|---|---|---|
| `GESTURE_KIND_TAP` | touch and lift without moving | select, open the menu |
| `GESTURE_KIND_LONG_PRESS` | touch and hold without moving | reserved by the stock system; delivered to the app here |
| `GESTURE_KIND_SWIPE_FORWARD` | horizontal stroke toward the front of the frame (the eye) | move through the timeline to older cards |
| `GESTURE_KIND_SWIPE_BACKWARD` | horizontal stroke toward the ear | move through the timeline to newer cards and settings |
| `GESTURE_KIND_SWIPE_DOWN` | downward stroke anywhere on the pad | back, dismiss; on the home card, turn the screen off. With two fingers it is handled by `glassd` (see Screen) and not delivered |
| `GESTURE_KIND_SWIPE_UP` | upward stroke | reserved |

- `Gesture.fingers` is the largest number of fingers seen during the gesture (1, 2 or 3). A two-finger tap is `TAP` with `fingers: 2`.
- `TAP` and `SWIPE_*` are sent when the last finger lifts. `LONG_PRESS` is sent once, when the hold time passes; that touch then produces no other gesture, and if `Scroll` frames were sent, a last `Scroll{ended:true, cancelled:true}` comes first.
- `Gesture.velocity` for swipes is in pad widths per second (horizontal) or pad heights per second (vertical), positive forward or down.
- `Scroll` is sent while fingers move horizontally, once the movement passes the slop threshold. `displacement` and `velocity` are in pad widths (1.0 = the full horizontal travel) and per second, positive forward. The last `Scroll` of a touch has `ended: true`.
- For a horizontal swipe a client gets both: the `Scroll` frames, the final `Scroll{ended:true}`, then the `SWIPE_*` gesture. A list that follows the finger uses `Scroll` while dragging and the gesture (or the final velocity) to decide where to settle.
- `FingerCount` reports changes in the number of fingers on the pad.
- **Waking touch.** While the screen is `SCREEN_OFF`, touchpad events are not delivered. A tap or a forward or backward swipe wakes the screen and is consumed; other touches are ignored. On route 1 (Android early suspend) the touchpad controller adds encoded wake gestures to its reports while the screen is off, and `glassd` uses only those, ignoring the real contacts that still arrive (`docs/specs/kernel-interfaces.md` §1.5, `docs/specs/touchpad-gestures.md` §5). On route 2 there is no early suspend and no encoded frames, so `glassd` classifies screen-off touches itself with the same recognizer. A tap on a dark screen never selects anything. While the screen is `SCREEN_DIM`, a touch restores `SCREEN_ON` and is consumed too (as on stock Glass): only the next touch reaches the app.
- Thresholds (slop, hold time, minimum swipe distance and velocity) are tuned to match the stock feel; their values come from `docs/specs/touchpad-gestures.md` §4.2 and live in the `glass-gesture` crate.

## Buttons (`TOPIC_BUTTON`)

- `Button{BUTTON_ID_CAMERA, action, screen_off}`: `BUTTON_ACTION_PRESS` on release after a short press, `BUTTON_ACTION_LONG_PRESS` once when the hold time passes (500 ms, our value: Android's long-press time), then `BUTTON_ACTION_RELEASE`. `screen_off` is the screen state when the button went down, for every frame of that press. On Glass a press takes a picture and a long press records video; that is the app's decision. The camera button wakes the screen when it goes down and, unlike the touchpad, the waking press is delivered, with `screen_off: true`, so a camera app can start at once.
- `Button{BUTTON_ID_POWER, ...}` is reported for information. `glassd` itself handles a short press (manual off-head, see Wearing). On route 1, the Android glue keeps Android's own power-key handling out of the short press (a keylayout change for the `twl6030_pwrbutton` device, milestone 3) and leaves the long press to Android (shut-down menu); on route 2, `glassd` handles the long press by asking the init system to power off (milestone 5).
- The camera button still works while the Glass is off the head, as on stock Glass: the press is delivered (`screen_off: true`), but it does not wake the screen.
- Broadcast to all subscribers.

## Head gestures (`TOPIC_HEAD`, milestone 2)

- Pitch is the head's angle above the horizon, from the accelerometer's gravity vector, positive up. On stock Glass the look-up is detected by the IMU's DMP, which reports no angle; `pitch_deg` and `LOOK_DOWN` are ours, computed in software from the accelerometer (low-passed at 5 /s, samples outside 0.5 g to 1.5 g ignored). The DMP firmware image is not redistributable, so `glassd` reads the raw accelerometer instead.
- `HEAD_GESTURE_KIND_LOOK_UP` when the pitch rises past `Settings.head_wake_angle_deg` (default 30°) with the roll under 10°. As on stock Glass it is detected only while the screen is off, the Glass is on the head and `head_wake_enabled` is true (the sensor is not run otherwise). `glassd` then wakes the screen with the quick timeout (see Screen); clients do not need to. When detection starts with the head already above the wake angle minus 7°, the head must come down first.
- `HEAD_GESTURE_KIND_LOOK_DOWN` (ours): the pitch fell back below the wake angle minus 7°, which re-arms look-up. After a look-up the detector keeps running, with the screen in any state, until this look-down; then it stops if the screen is on.
- `HEAD_GESTURE_KIND_NUDGE`: a quick upward head nudge that comes back, while the screen is on. It is detected only while `nudge_sleep_enabled` is true or a client subscribed to `TOPIC_HEAD` asked for it with `HeadRequest{nudge: true}` (the gyroscope is not run otherwise). When `nudge_sleep_enabled` is true (default false), `glassd` turns the screen off, unless a touch is in progress. The stock detector is a trained classifier whose model is not available; ours is a threshold detector on the pitch rate with the stock limits on the other axes (`crates/glass-head/src/nudge.rs`), to be tuned on a device.
- `HEAD_GESTURE_KIND_RELATIVE_LOOK_UP`: the pitch rose by 17° or more within 5 s, from any start angle (stock uses it to reveal a notification). It needs the gyroscope read every 20 ms, so it runs only while at least one client subscribed to `TOPIC_HEAD` has asked for it with `HeadRequest{relative_look_up: true}`, and while it runs it replaces the global look-up, as on stock Glass.
- Inhibition windows: no nudge within 1000 ms of a look-up, relative look-up or nudge; no look-up within 500 ms of a nudge. A client can also inhibit look-up for a while with `HeadRequest{inhibit_look_up_ms}` (stock: the home card does it for 5 s while a notification is shown); 0 clears it. The inhibition is global.
- `HeadRequest` replaces the sender's previous wishes (`relative_look_up`, `nudge`); they count while the client is subscribed to `TOPIC_HEAD` and end when it disconnects. `inhibit_look_up_ms` acts only when it is present.
- `HeadRequest` is fire-and-forget: there is no answer, and with no head source it is accepted and has no effect.
- Nothing runs off the head. Rates: 10 Hz for the accelerometer alone (look-up with the screen off; each sample wakes the CPU, see the device checklist), 50 Hz when the gyroscope runs. After a look-up the accelerometer keeps running until the look-down, so a user who keeps the head up keeps it streaming with the screen on; the same holds when detection starts with the head already up.
- **Who owns the IMU (open, owner to confirm):** on route 2 `glassd` reads it through IIO. On route 1 the IMU belongs to Android's sensors HAL, so `glassd` does not open it by default on Android (`--head off`, the `head` capability is absent); head gestures there would come through the Android glue from Android's sensors (milestone 3), unless the owner drops the HAL and `glassd` takes the IMU (`--head on`).

## Wearing and wink (`TOPIC_WEAR`, milestone 2)

- `WearState{wear, source}`:
  - `WEAR_SOURCE_SENSOR`: the glasshub on-head detection, when `on_head_detection_enabled` is true (default false).
  - `WEAR_SOURCE_FORCED`: detection off, so the Glass is assumed on the head (`WEAR_ON_HEAD`); also during the 30 s after boot.
  - `WEAR_SOURCE_POWER_KEY`: a short power-key press toggles a manual off-head; the next press gives a timed on-head (30 s, then back to the sensor if detection is on).
  - `WEAR_SOURCE_HINGE`: closing the hinge forces off-head; opening it gives a timed on-head. The Linux input code of the hinge switch is not confirmed yet (device needed), so this source may be missing (`hinge` capability).
  - `WEAR_UNKNOWN` only before the first state is known. In practice `glassd` starts forced on the head, so it is never sent.
  - A `WearState` is sent when the wear or its source changes (for example `FORCED` to `SENSOR` when the 30 s after boot end on the head); the off-head and don actions below run only when the wear itself changes. While detection is on but the sensor has not reported yet, the previous state stays.
  - Turning `on_head_detection_enabled` on hands over to the sensor at once; turning it off forces on-head. A sensor reading taken while it did not count (detection off, forced or timed periods) is kept and applies when the sensor counts again.
- On off-head (any source), `glassd` turns the screen off, cancels every keep-on hold, cancels a touch in progress, puts the touchpad to sleep (`sleepmode` 1) and stops head wake. While forced off the head (power key or hinge), the glasshub is paused. While off the head nothing wakes the screen: touches are ignored, `ACTION_WAKE` and `ACTION_KEEP_ON` are ignored (no hold is recorded), and the camera button is delivered without a wake. On don it re-enables them and, if the screen is off, wakes it with the quick timeout.
- `Wink` when the glasshub reports a wink, `wink_enabled` is true (default false) and the Glass is on the head. A wink does not wake the screen. What it does (on Glass, take a picture) is the app's decision.
  - The wink is the glasshub firmware's low-power wink interrupt, armed only while `wink_enabled` is true; a read of `wink` that gives 0 is not a wink. Stock Glass confirms the interrupt with a CPU classifier on the raw proximity stream and a per-user calibration; `glassd` does not reproduce that classifier, so false positives are likely until an open classifier and calibration exist (`docs/specs/glasshub.md` §6.4, §8).
  - After it arms on-head detection or resumes the hub, `glassd` reads `don_doff` again, since the state may have changed without a notification. If the glasshub fails (a read error), `glassd` stops using it: the Glass is forced on the head and winks stop.
  - At start `glassd` writes the firmware thresholds (on-head detection off while it does): don 450, doff 400, don max 2000, gain 2, or a user's calibration from the stock parameter file `/data/system/gaze/gaze_params.txt` if it is there (`--gaze-params`). It never writes `calibrate`, `update_fw_*` or anything outside a fixed list of attributes.

## Screen (`TOPIC_SCREEN`)

`glassd` owns the screen policy, so every app does not need to:
- **Wakes** on a touchpad tap or forward or backward swipe, on `LOOK_UP` when head wake is on, on don, on the camera button, and on `ACTION_WAKE` or `ACTION_KEEP_ON`, but only while the Glass is on the head (see Wearing). `ScreenState` for a wake carries `reason`, `last_sleep_reason` and `off_ms`, so the launcher can decide whether to return to the home card (stock: it returns home unless the last sleep was a timeout or a gesture and either the screen was off for 5 s or less or an immersive app was in front; a swipe always returns home). The sleep reasons that count as a gesture are `nudge` (as on stock) and `two-finger-swipe-down` (our choice). `off_ms` counts time in suspend too (`CLOCK_BOOTTIME`), so a Glass left off overnight reports the real time.
- **Quick timeout:** a wake by look-up or by don uses a one-shot 3 s timeout with no dim step; the next user activity switches back to the normal timeout.
- **Sleeps** after `screen_timeout_ms` (default 15 s) without user activity, with a dim step before; on a **two-finger swipe down anywhere** (handled by `glassd`, not delivered to apps, as on stock Glass); on a nudge when `nudge_sleep_enabled`; on `WEAR_OFF_HEAD`; and on `ACTION_SLEEP` (the launcher sends it on a one-finger `SWIPE_DOWN` at the home card).
- **Keep-on holds:** `ACTION_KEEP_ON` wakes the screen if needed and holds it on, without dimming, for `duration_ms` (0, or more than 10 minutes, means 10 minutes: a client renews it to hold longer). A repeated `ACTION_KEEP_ON` from the same client replaces its hold. The screen stays on while any client holds it. A hold ends on `ACTION_RELEASE_KEEP_ON`, when its time runs out, or when the client disconnects. Off the head `ACTION_KEEP_ON` is ignored.
- **Overrides:** every explicit sleep (`ACTION_SLEEP`, the two-finger swipe down, a nudge with `nudge_sleep_enabled`, `WEAR_OFF_HEAD`) turns the screen off even with holds, and cancels every hold; only the timeout respects holds. An `ACTION_KEEP_ON` during the quick timeout wins over it. Taking the Glass off always wins, for battery and for heat against the temple.
- `ACTION_USER_ACTIVITY` restarts the timer, for activity `glassd` cannot see.
- `ScreenState` reports every change with a `reason`; `dim` for the dim step and `activity` when a touch or `ACTION_USER_ACTIVITY` restores `SCREEN_ON` from `SCREEN_DIM`.

  `ScreenState.reason` currently takes these values:

  | Change | Reasons |
  |---|---|
  | wake (→ `SCREEN_ON`, from `SCREEN_OFF` or `SCREEN_DIM`) | `tap`, `swipe-forward`, `swipe-backward` (touchpad), `look-up`, `don`, `button` (camera), `request` (`ACTION_WAKE` or `ACTION_KEEP_ON`) |
  | sleep (→ `SCREEN_OFF`) | `timeout`, `two-finger-swipe-down`, `nudge`, `off-head`, `request` (`ACTION_SLEEP`) |
  | dim and undim | `dim` (`SCREEN_ON` → `SCREEN_DIM`); `activity` (`SCREEN_DIM` → `SCREEN_ON` by a touch or `ACTION_USER_ACTIVITY`; other wake sources undim with their wake reason) |
  | snapshot | `snapshot` |

  `last_sleep_reason` is one of the sleep reasons, set only on a change out of `SCREEN_OFF`, and empty otherwise (also before the first sleep). A client compares reasons as strings and treats an unknown value as "other" (new reasons may be added).

The default timeout and the dim step follow the stock behaviour described in `docs/specs/`.

Who switches the panel:
- **Route 2 (Linux):** `glassd`, with `FBIOBLANK` on the panel framebuffer (`--display fb[:path]`, default `/dev/fb0`; it must be a character device): unblank for `SCREEN_ON` and `SCREEN_DIM`, power down for `SCREEN_OFF`.
- **Route 1 (Android):** the framework owns blanking (`PowerManager`, SurfaceFlinger, hwcomposer), so `glassd` defaults to `--display log` there and the Android glue (milestone 3) turns `ScreenState` into `PowerManager` calls. `--display fb` still forces `FBIOBLANK` (for example to test without the framework).
- The dim step is reported but does not change the panel yet: `glassd` never writes the brightness, nor `brightness_limit` or `brightness_multiplier`. `--display log` (also the default with `--mock`) only logs.

## Settings (`TOPIC_SETTINGS`)

`SetSettings` changes only the fields that are present; `glassd` stores them, applies them, and sends the full `Settings` back (before any frame the change causes, such as a `WearState`). They are kept in the file `settings` of the state directory (`--state-dir`, else `$GLASSD_STATE_DIR`, else `/data/glassd` on Android and `/var/lib/glassd` on Linux), written atomically, and read at start over the defaults; the mock keeps them in memory unless `--state-dir` is given. Values out of range are clamped, and the reply shows what was applied:

| Field | Range |
|---|---|
| `head_wake_enabled` | default true |
| `head_wake_angle_deg` | 10 to 40 (degrees above the horizon), default 30 |
| `wink_enabled` | default false |
| `on_head_detection_enabled` | default false |
| `screen_timeout_ms` | 3000 to 600000, default 15000; there is no "never" |
| `nudge_sleep_enabled` | default false |

## Dictation (`TOPIC_DICTATION`, milestone 3)

Speech is not transcribed on Glass: it goes to the phone, which transcribes it offline, through the OpenMyGlass companion and its bridge protocol ([`openmyglass.v1`](https://github.com/OpenMyGlass/OpenMyGlass/blob/main/protocol/openmyglass/v1/bridge.proto): `DictationStart`, `AudioChunk`, `DictationEnd`, `Transcript`, `ReplyConfirm`, `ReplyResult`).
- An app sends `DictationStart{request_id, purpose}`. `purpose` is `"notification-reply"` (with the notification key and action index, as in the bridge protocol) or `"free"` (any text, for example a note). `"free"` maps to the bridge's `DictationStart.free` (added in [OpenMyGlass#83](https://github.com/OpenMyGlass/OpenMyGlass/pull/83)), available when the phone's Hello lists the `dictation-free` capability; without it, `glassd` answers `ERROR_CODE_UNSUPPORTED`. The phone forgets a request 60 s after its start, so `glassd` ends a free dictation after 55 s and reports `STATE_FAILED` with `error: "expired"` if no transcript came.
- `request_id` is per connection; `glassd` maps it to its own id on the companion link, so two clients cannot collide.
- `glassd` relays the request to the companion link provider, the process that holds the phone connection. The provider captures the microphone and streams audio to the phone, so audio never passes through `glassd`. How the provider registers is defined in milestone 3.
- The app receives `DictationState` and `Transcript` for its own request without subscribing to `TOPIC_DICTATION` (the topic exists for symmetry and carries nothing else). The bridge's `Transcript.error` becomes `DictationState{STATE_FAILED, error}`.
- For `"notification-reply"`, after the final transcript the app sends `DictationConfirm{send}` (the bridge's `ReplyConfirm`); the bridge's `ReplyResult` becomes `STATE_SENT` or `STATE_FAILED`.
- `DictationCancel` stops a request. Without a provider, `DictationStart` gets `ERROR_CODE_NO_COMPANION`.

## Location (`TOPIC_LOCATION`, milestone 4)

Glass gets its location from the phone, not from its own GPS chip, which stays asleep (owner decision, 2026-10-22; no GPS HAL is written). The fixes come through the OpenMyGlass companion's bridge protocol (`LocationSubscribe`, `LocationUpdate`, `LocationUnavailable` and the `location` Hello capability, added in [OpenMyGlass#84](https://github.com/OpenMyGlass/OpenMyGlass/pull/84); see its `docs/location.md`), relayed by the companion link provider, as for dictation.
- A client sends `LocationRequest{interval_ms, high_accuracy}` and subscribes to `TOPIC_LOCATION`. `glassd` asks the phone for the shortest interval and the highest accuracy any client wants, and tells the phone to stop when no client wants fixes (`interval_ms: 0`, or the client disconnects). The phone only tracks while asked, to save its battery, and raises intervals under 1000 ms to 1000 ms. `accuracy_m` 0 means unknown.
- `interval_ms` is passed to the phone as asked; the phone applies its 1000 ms floor, so a request for 200 ms gets about one fix per second.
- `Location` frames are broadcast to subscribers; `provider` is copied from the phone's fix. On `Subscribe` and `GetState`, the current `LocationStatus` and the last known fix (if any, with `snapshot: true`) are sent; a client checks `fix_time_ms` and `received_us` to judge the fix's age.
- `LocationStatus` reports whether fixes flow: `STATE_ACTIVE`, `STATE_IDLE`, `STATE_NO_COMPANION`, `STATE_UNSUPPORTED` (the phone's Hello lacks the `location` capability), or `STATE_UNAVAILABLE` with a reason (`permission`, `disabled`, `no-fix`). Every change is broadcast (for example `STATE_ACTIVE` again when fixes resume), and every `LocationRequest` is answered with the current `LocationStatus` to its sender, subscribed or not.
- **Privacy:** fixes are kept in memory only and never logged or written to storage. `glassd` drops the last fix when the companion link goes away. Any client allowed on the socket (group `glass`) can read location; version 1 has no per-client permission.
- **Android glue (route 1):** the glue provides Android's `network` location provider (and possibly `fused`) as a system app built on the platform's location-provider base class, selected through the framework overlay for network location providers; its fixes are then ordinary fixes, not mock ones. The provider's request (interval, accuracy) is mapped to `LocationRequest`, and it sends `interval_ms: 0` when Android no longer needs fixes. The `gps` provider stays absent, since there is no GPS HAL; feeding a `gps` provider from the phone would need either a small GPS HAL fed by `glassd` (it would never wake the chip) or a framework change: an open decision for the owner. Test (mock) providers are not used: they need a developer setting and apps flag or reject their fixes.
- **Route 2:** apps use this topic directly; a small gpsd-compatible shim is a possible later addition.
- The capability `location` means `glassd` has a companion link provider able to relay location; whether the phone supports it is reported by `STATE_UNSUPPORTED`.

## Mock

`glassd --mock` serves the same socket with `capabilities` including `mock`, and with no hardware. `power` and `camera` go through the same button logic as the real keys (`power` toggles the manual off-head), `hinge` drives the hinge source, and `wear on|off` is a reading of the simulated on-head sensor: like the real one, it counts only while `on_head_detection_enabled` is true. `lookup`, `lookdown`, `nudge` and `relative-lookup` are delivered only when the real detector would run and report them (for example `lookup` with the screen off, `nudge` after `HeadRequest{nudge: true}` or with `nudge_sleep_enabled`). It reads commands on standard input, one per line: `tap [fingers]`, `longpress`, `swipe forward|backward|down|up [fingers]`, `scroll <displacement>`, `camera [long]`, `power [long]`, `hinge open|closed`, `lookup`, `lookdown`, `relative-lookup`, `nudge`, `wear on|off`, `wink`, `dim`, `location <lat> <lon> [accuracy_m]` and `location-status <state>` (milestone 4), `wait <ms>`. A developer or a test script can drive an app with it. The `glass-api` crate also has an in-process mock server for unit tests.
