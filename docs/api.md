# Glass services interface (API version 1)

How the launcher, apps and the Android glue talk to `glassd`. The messages are defined in [`proto/glass/services/v1/services.proto`](../proto/glass/services/v1/services.proto); this page gives their meaning. The Rust types, codec and client are in the `glass-api` crate.

Nothing in this interface depends on Android: it is the same on route 1 (open Android) and route 2 (plain Linux).

## Transport

| | |
|---|---|
| Socket | Unix stream socket. Path from `$GLASS_SERVICES_SOCKET`, else `/dev/socket/glassd` on Android (created by init) and `/run/glassd/glassd.sock` on Linux. |
| Framing | varint32 length, then one protobuf message: `ClientFrame` from client to `glassd`, `ServiceFrame` back. The same framing as the OpenMyGlass bridge protocol. |
| Frame limit | 64 KiB. A longer length prefix is a protocol error: `glassd` sends `Error{code:"bad-frame"}` and closes the connection. |
| Access | Socket file mode `0660`, group `glass` (Linux) or `system` (Android). Peers are not otherwise authenticated in version 1. |
| Time | Every `timestamp_us` is `CLOCK_MONOTONIC` in microseconds, taken from the kernel event that caused it when there is one. |

## Session

1. The client connects and sends `Hello{api_version: 1, client_name}`.
2. `glassd` answers `Welcome{api_version, service_version, capabilities}`. A client that sends anything before `Hello` gets `Error{code:"bad-frame"}` and is disconnected. If `api_version` is newer than the service supports, `glassd` answers with its own version and the client decides whether to continue.
3. The client sends `Subscribe{topics}`. Each `Subscribe` replaces the previous set. Right after a `Subscribe`, `glassd` sends the current `WearState`, `ScreenState` and `Settings` for the topics that have one, so a client never waits for a change to learn the state.
4. `GetState` asks for the same snapshot again.

Capabilities say which sources exist on this device or build: `touchpad`, `head`, `wear`, `wink`, `screen`, `dictation`, and `mock` when the server is the mock. A topic whose source is missing is accepted and stays silent.

Unknown fields and unknown `oneof` cases are ignored by both sides (normal protobuf rules), so new messages can be added without a version bump. A change that alters the meaning of an existing field bumps `api_version`.

## Input focus

Touchpad events (`Gesture`, `Scroll`, `FingerCount`) go to **one** client: the focus holder.
- `AcquireInputFocus` makes the sender the focus holder. Focus is a stack: the previous holder gets `FocusState{focused:false}` and gets the focus back when the new holder sends `ReleaseInputFocus` or disconnects.
- The new holder gets `FocusState{focused:true}`.
- A client that subscribed to `TOPIC_GESTURE` without holding the focus receives nothing on that topic.
- The launcher takes the focus at start; an app that it opens takes the focus while it is in front. On Android, the glue holds the focus while a regular Android activity is in front and turns gestures into key events.

All other topics are broadcast to every subscriber.

## Touchpad gestures (`TOPIC_GESTURE`)

The touchpad is on the right temple. Directions are named as on Glass:

| Kind | Finger motion | Typical use (Glass design guidelines) |
|---|---|---|
| `TAP` | touch and lift without moving | select, open the menu, wake the screen |
| `LONG_PRESS` | touch and hold without moving | contextual action |
| `SWIPE_FORWARD` | toward the front of the frame (the eye) | move through the timeline (to older cards) |
| `SWIPE_BACKWARD` | toward the ear | move through the timeline (to newer cards, settings) |
| `SWIPE_DOWN` | from the top edge downward | back, dismiss; on the home card, turn the screen off |
| `SWIPE_UP` | from the bottom edge upward | reserved |

- `Gesture.fingers` is the largest number of fingers seen during the gesture (1, 2 or 3). A two-finger tap is `TAP` with `fingers: 2`. Three-finger gestures are reported as such; what they do is up to the app.
- `TAP`, `SWIPE_*` are sent when the last finger lifts. `LONG_PRESS` is sent once, when the hold time passes, and that touch then produces no other gesture.
- `Scroll` is sent while fingers move horizontally, after the movement passes the slop threshold. `displacement` and `velocity` are in touchpad widths (one width = the full horizontal travel, 1.0) and per second, positive forward. The last `Scroll` of a touch has `ended: true`, so a list that follows the finger can settle or fling.
- `FingerCount` reports changes in the number of fingers on the pad.
- Thresholds (slop, hold time, minimum swipe distance and velocity) are tuned to match the stock feel; their values are in `docs/specs/` and in the `glass-gesture` crate.

## Head gestures (`TOPIC_HEAD`, milestone 2)

- `LOOK_UP` when the head pitch rises past `Settings.head_wake_angle_deg`, `LOOK_DOWN` when it falls back below it with hysteresis. When `head_wake_enabled` is true, `glassd`'s screen policy also wakes the screen on `LOOK_UP`; clients do not need to.
- `NOD` for a quick down-up head movement.

## Wearing and wink (`TOPIC_WEAR`, milestone 2)

- `WearState` from the glasshub proximity sensor: `ON_HEAD`, `OFF_HEAD`, or `WEAR_UNKNOWN` when detection is disabled or unavailable.
- `Wink` when the glasshub reports a deliberate wink and `wink_enabled` is true. What a wink does (on Glass, take a picture) is the app's decision.

## Screen (`TOPIC_SCREEN`)

`glassd` owns the screen policy, so every app does not need to:
- **Wakes** on a touchpad touch, on `LOOK_UP` when head wake is on, on the camera button, and on `ScreenRequest{WAKE}`.
- **Sleeps** after `screen_timeout_ms` without user activity, on `OFF_HEAD` when on-head detection is on, and on `ScreenRequest{SLEEP}` (the launcher sends it on `SWIPE_DOWN` at the home card).
- `KEEP_ON` holds the screen on (a video, navigation); it is released by `RELEASE_KEEP_ON`, by its `duration_ms` running out, or when the client disconnects.
- `USER_ACTIVITY` restarts the timer, for activity `glassd` cannot see.
- `ScreenState` reports every change with a `reason`.

The exact timeouts and the dim step follow the stock behaviour described in `docs/specs/`.

## Settings (`TOPIC_SETTINGS`)

`SetSettings` changes only the fields that are present; `glassd` stores them, applies them, and broadcasts the full `Settings` to subscribers. Values out of range are clamped, and the broadcast shows what was applied.

## Dictation (`TOPIC_DICTATION`, milestone 3)

Dictation is not done on Glass: speech goes to the phone, which transcribes it offline, through the OpenMyGlass companion and its bridge protocol ([`openmyglass.v1`](https://github.com/OpenMyGlass/OpenMyGlass/blob/main/protocol/openmyglass/v1/bridge.proto), `DictationStart`, `AudioChunk`, `DictationEnd`, `Transcript`).
- An app sends `DictationStart{request_id, purpose}`. `purpose` is `"notification-reply"` (with the notification key and action index, as in the bridge protocol) or `"free"` (any text, for example a note or a message to a contact).
- `glassd` relays the request to the companion link provider, the process that holds the phone connection. The provider captures the microphone and streams audio to the phone, so audio never passes through `glassd`. How the provider registers is defined in milestone 3.
- The app receives `DictationState` and `Transcript` frames for its own `request_id`; `DictationCancel` stops it.
- Without a provider (no phone connected), `DictationStart` gets `DictationState{FAILED, error:"no-companion"}`.

## Errors

`Error.code` is one of `bad-frame` (malformed or oversize frame, or no `Hello` first; the connection is closed), `unsupported` (a request this build cannot serve), `not-focused` (reserved), `unavailable` (the source exists but failed, for example the device was removed).

## Mock

`glassd --mock` serves the same socket with `capabilities` including `mock`, and with no hardware. It reads simple commands on standard input (one per line, for example `tap`, `tap 2`, `swipe forward`, `swipe down`, `longpress`, `scroll 0.3`, `lookup`, `wear off`, `wink`), so a developer or a test script can drive an app. The `glass-api` crate also has an in-process mock server for unit tests.
