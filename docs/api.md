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
| Backpressure | Each client has an outgoing queue of 256 KiB. When it fills, consecutive `Scroll` frames are merged (displacement and velocity of the newest, deltas summed); if the queue stays full for 2 s the client is disconnected. A slow client never delays the others. |

### Who may connect

- **Linux (route 2):** socket mode `0660`, group `glass`. The launcher and apps run as users in that group.
- **Android (route 1):** socket `/dev/socket/glassd`, mode `0660`, owner `system`, group `glass` (a group added by our ROM). Our own native apps (the launcher and the other glass-apps, started by the launcher wrapper) run with that group. A regular Android app cannot connect: it goes through the Android glue, which is a client itself and turns events into Android input and broadcasts. The SELinux policy needs a `glassd` domain, a `glassd_socket` type and `connectto` rules for those clients (milestone 3).
- Peers are not authenticated beyond the socket permissions in version 1. `glassd` logs each peer's uid and pid (`SO_PEERCRED`).

## Session

1. The client connects and sends `Hello{api_version, client_name}`.
2. `glassd` answers `Welcome{api_version, service_version, capabilities}`, where `api_version` is the lower of the client's and the service's: an older client gets the older behaviour, a newer client learns which version it is talking to and decides whether to continue. `api_version` 0 is `ERROR_CODE_BAD_FRAME`.
   - A frame before `Hello` is `ERROR_CODE_BAD_FRAME` and the connection is closed.
   - A second `Hello` is answered with `Welcome` again and changes nothing else.
3. The client sends `Subscribe{topics}`. Each `Subscribe` replaces the previous set. After every `Subscribe`, `glassd` sends a **snapshot** for each topic in the new set that has state: `WearState` (`snapshot: true`), `ScreenState` (`reason: "snapshot"`), `Settings`. Snapshot timestamps are the time of the last change. A topic whose source does not exist sends no snapshot.
4. `GetState` sends the same snapshot for the currently subscribed topics, plus `FocusState` for this client.

Capabilities say which sources exist on this device or build: `touchpad`, `head`, `wear`, `wink`, `screen`, `button`, `dictation`, and `mock` when the server is the mock. A topic whose source is missing is accepted and stays silent.

### Requests and errors

- `ClientFrame.seq` is optional; `glassd` puts it in `ServiceError.seq` when a frame causes an error, so a client can tell which request failed.
- A frame with no known body (an empty frame, or a request newer than this `glassd`) gets `ERROR_CODE_UNSUPPORTED`.
- A known request with an invalid value gets `ERROR_CODE_INVALID_ARGUMENT`: `ACTION_UNSPECIFIED`, an unknown dictation `purpose`, a missing `notification_key` for `"notification-reply"`. In `Subscribe`, `TOPIC_UNSPECIFIED` and unknown topic values are ignored.
- `SetSettings` is always answered with the full `Settings` to the sender (and to subscribers of `TOPIC_SETTINGS`).
- Unknown fields in a known message are ignored (normal protobuf rules), so fields can be added without a version bump. A change that alters the meaning of an existing field goes into a new package `glass.services.v2`; `glassd` may serve both.

| `ErrorCode` | Meaning |
|---|---|
| `ERROR_CODE_BAD_FRAME` | malformed or oversize frame, no `Hello` first, `api_version` 0; the connection is closed |
| `ERROR_CODE_UNSUPPORTED` | a request this build does not know or cannot serve |
| `ERROR_CODE_INVALID_ARGUMENT` | a known request with an invalid value |
| `ERROR_CODE_UNAVAILABLE` | the source exists but failed (for example the device was removed) |
| `ERROR_CODE_NO_COMPANION` | dictation requested while no companion link is registered |

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
| `GESTURE_KIND_SWIPE_DOWN` | downward stroke anywhere on the pad | back, dismiss; on the home card, turn the screen off |
| `GESTURE_KIND_SWIPE_UP` | upward stroke | reserved |

- `Gesture.fingers` is the largest number of fingers seen during the gesture (1, 2 or 3). A two-finger tap is `TAP` with `fingers: 2`.
- `TAP` and `SWIPE_*` are sent when the last finger lifts. `LONG_PRESS` is sent once, when the hold time passes; that touch then produces no other gesture and no more `Scroll`.
- `Gesture.velocity` for swipes is in pad widths per second (horizontal) or pad heights per second (vertical), positive forward or down.
- `Scroll` is sent while fingers move horizontally, once the movement passes the slop threshold. `displacement` and `velocity` are in pad widths (1.0 = the full horizontal travel) and per second, positive forward. The last `Scroll` of a touch has `ended: true`.
- For a horizontal swipe a client gets both: the `Scroll` frames, the final `Scroll{ended:true}`, then the `SWIPE_*` gesture. A list that follows the finger uses `Scroll` while dragging and the gesture (or the final velocity) to decide where to settle.
- `FingerCount` reports changes in the number of fingers on the pad.
- **Waking touch.** While the screen is `SCREEN_OFF`, touchpad events are not delivered: the touch that wakes the screen is consumed, so a tap on a dark screen never selects anything. While the screen is `SCREEN_DIM`, a touch restores `SCREEN_ON` and is delivered normally.
- Thresholds (slop, hold time, minimum swipe distance and velocity) are tuned to match the stock feel; their values come from the specs in `docs/specs/` (added by separate PRs) and live in the `glass-gesture` crate.

## Camera button (`TOPIC_BUTTON`)

`Button{BUTTON_ID_CAMERA, action}`: `BUTTON_ACTION_PRESS` on release after a short press, `BUTTON_ACTION_LONG_PRESS` once when the hold time passes, then `BUTTON_ACTION_RELEASE`. On Glass a press takes a picture and a long press records video; that is the app's decision. The button wakes the screen and, unlike the touchpad, the waking press is also delivered. Broadcast to all subscribers.

## Head gestures (`TOPIC_HEAD`, milestone 2)

- Pitch is the head's angle above the horizon, from the accelerometer's gravity vector, positive up.
- `HEAD_GESTURE_KIND_LOOK_UP` when the pitch rises past `Settings.head_wake_angle_deg`; `HEAD_GESTURE_KIND_LOOK_DOWN` when it falls below the wake angle minus 5°. When `head_wake_enabled` is true, `glassd`'s screen policy wakes the screen on `LOOK_UP`; clients do not need to.
- `HEAD_GESTURE_KIND_NOD` for a quick down-up movement.

## Wearing and wink (`TOPIC_WEAR`, milestone 2)

- `WearState` from the glasshub proximity sensor: `WEAR_ON_HEAD`, `WEAR_OFF_HEAD`, or `WEAR_UNKNOWN` when detection is disabled or unavailable.
- `Wink` when the glasshub reports a deliberate wink and `wink_enabled` is true. What a wink does (on Glass, take a picture) is the app's decision.
- Whether disabling wink or on-head detection also powers the sensor down, and how wink calibration is exposed, is decided in milestone 2 from the glasshub spec.

## Screen (`TOPIC_SCREEN`)

`glassd` owns the screen policy, so every app does not need to:
- **Wakes** on a touchpad touch, on `LOOK_UP` when head wake is on, on the camera button, and on `ACTION_WAKE`.
- **Sleeps** after `screen_timeout_ms` without user activity (with a dim step before), on `WEAR_OFF_HEAD` when on-head detection is on, and on `ACTION_SLEEP` (the launcher sends it on `SWIPE_DOWN` at the home card).
- **Keep-on holds:** `ACTION_KEEP_ON` wakes the screen if needed and holds it on, without dimming, for `duration_ms` (0, or more than 10 minutes, means 10 minutes: a client renews it to hold longer). A repeated `ACTION_KEEP_ON` from the same client replaces its hold. The screen stays on while any client holds it. A hold ends on `ACTION_RELEASE_KEEP_ON`, when its time runs out, or when the client disconnects.
- **Overrides:** `ACTION_SLEEP` and `WEAR_OFF_HEAD` turn the screen off even with holds, and cancel every hold. Taking the Glass off always wins, for battery and for heat against the temple.
- `ACTION_USER_ACTIVITY` restarts the timer, for activity `glassd` cannot see.
- `ScreenState` reports every change with a `reason`.

The default timeout and the dim step follow the stock behaviour described in `docs/specs/`.

## Settings (`TOPIC_SETTINGS`)

`SetSettings` changes only the fields that are present; `glassd` stores them, applies them, and sends the full `Settings` back. Values out of range are clamped, and the reply shows what was applied:

| Field | Range |
|---|---|
| `head_wake_angle_deg` | 10 to 60 (degrees above the horizon) |
| `screen_timeout_ms` | 3000 to 600000; there is no "never" |

## Dictation (`TOPIC_DICTATION`, milestone 3)

Speech is not transcribed on Glass: it goes to the phone, which transcribes it offline, through the OpenMyGlass companion and its bridge protocol ([`openmyglass.v1`](https://github.com/OpenMyGlass/OpenMyGlass/blob/main/protocol/openmyglass/v1/bridge.proto): `DictationStart`, `AudioChunk`, `DictationEnd`, `Transcript`, `ReplyConfirm`, `ReplyResult`).
- An app sends `DictationStart{request_id, purpose}`. `purpose` is `"notification-reply"` (with the notification key and action index, as in the bridge protocol) or `"free"` (any text, for example a note). Bridge v1 supports only notification replies: `"free"` needs a small bridge change, proposed in OpenMyGlass.
- `request_id` is per connection; `glassd` maps it to its own id on the companion link, so two clients cannot collide.
- `glassd` relays the request to the companion link provider, the process that holds the phone connection. The provider captures the microphone and streams audio to the phone, so audio never passes through `glassd`. How the provider registers is defined in milestone 3.
- The app receives `DictationState` and `Transcript` for its own request. The bridge's `Transcript.error` becomes `DictationState{STATE_FAILED, error}`.
- For `"notification-reply"`, after the final transcript the app sends `DictationConfirm{send}` (the bridge's `ReplyConfirm`); the bridge's `ReplyResult` becomes `STATE_SENT` or `STATE_FAILED`.
- `DictationCancel` stops a request. Without a provider, `DictationStart` gets `ERROR_CODE_NO_COMPANION`.

## Mock

`glassd --mock` serves the same socket with `capabilities` including `mock`, and with no hardware. It reads commands on standard input, one per line: `tap [fingers]`, `longpress`, `swipe forward|backward|down|up [fingers]`, `scroll <displacement>`, `camera [long]`, `lookup`, `lookdown`, `wear on|off`, `wink`, `wait <ms>`. A developer or a test script can drive an app with it. The `glass-api` crate also has an in-process mock server for unit tests.
