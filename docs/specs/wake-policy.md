# Screen wake and sleep policy, on-head handling (stock XE24)

Clean-room specification of the policy in `SystemServerHub`, `DonManager`
and `InputHub` (`/system/framework/glass-services.jar`), the Glass changes
in `PowerManagerService` (`/system/framework/services.jar`) and the
resources in `/system/framework/glass-framework-res.apk`. Legend: **[C]**
certain, **[I]** inferred, **[?]** open. Behaviour is described in our own
words.

## 1. Timeouts and constants **[C]**

| Name | Value | Where |
|---|---|---|
| Default screen-off timeout | 15000 ms | `PowerManagerService` constant `DEFAULT_SCREEN_OFF_TIMEOUT` |
| Minimum screen-off timeout | 3000 ms | `MINIMUM_SCREEN_OFF_TIMEOUT` |
| Dim duration before off | `min(3000 ms, 20 % of timeout)` | XE24 `PowerManagerService` (`SCREEN_DIM_DURATION = 3000`; AOSP 4.4 uses 7000 ms with the same 20 % cap, so Glass lowered it) |
| **Quick timeout** | 3000 ms, with **no dim phase** | `DEFAULT_SCREEN_OFF_QUICK_TIMEOUT`, `SCREEN_DIM_DURATION_FOR_QUICK_TIMEOUT = 0` |
| Home reset timeout | 5000 ms | resource `integer/home_reset_timeout_ms` |
| Forced-donned timer | 30000 ms | resource `integer/force_donned_timeout_ms` |
| Screen-off launch wake lock | 5000 ms | resource `integer/screen_off_launch_lock_ms` |
| Lock screen delayed activation | 15000 ms | resource `integer/activation_delay_ms` **[I]** (name only; consumer not traced) |
| Camera preload on camera key | 2000 ms | `SystemServerHub` |

Quick timeout mechanics (Glass addition to `PowerManagerService`):
`PowerManager.useQuickScreenTimeout()` sets a one-shot request; the next
user activity consumes it and switches the current screen-on period to the
3 s / no-dim timeout. Any later user activity without a new request returns
to the normal timeout. Going to sleep clears both flags.

Sleep reasons: `PowerManager.goToSleep(time, reason)` with reason **2**
(timeout) and a Glass reason **3** ("gesture"). `screenTimedOut()` and
`gestureTurnedScreenOff()` report whether the last sleep had reason 2 or 3;
`timeSinceScreenWasLastOn()` is also added. The display request gets a
"passive off" flag when the last sleep was a timeout or a gesture. **[C]**

## 2. Wake sources and what they do **[C]**

"Input active" below means: device provisioned (`Settings.Global
device_provisioned != 0`) **and** not locked.

"Wake with home" means: start the HOME intent (`ACTION_MAIN`,
`CATEGORY_HOME`, flags `0x10220000` = NEW_TASK | REORDER_TO_FRONT |
RESET_TASK_IF_NEEDED), extras `screen_turned_on = true`, `locked`,
`wake_up_reason` (enum `com.google.android.glass.os.WakeUpReason`, serialized),
`wake_up_time` (uptime ms), and hold the 5 s screen-off launch wake lock;
the activity start turns the screen on. "Wake in place" means
`PowerManager.wakeUp(now)`. Home is only started when input is active,
otherwise the device wakes in place.

"Should reset to home" = true, unless the last sleep was a timeout or a
gesture **and** (the top activity is immersive **or** at most 5000 ms passed
since the screen was last on) - in that case the user goes back to what was
showing. (`timeSinceScreenWasLastOn` is read as "time the screen has been
off" **[I]**.)

| Source | Condition | Action | `WakeUpReason` |
|---|---|---|---|
| Touchpad TAP while screen off | not intercepted by a `touchpad_manager` client | wake; with home if "should reset to home" | `TOUCHPAD_TAP` |
| Touchpad SWIPE_LEFT / SWIPE_RIGHT while screen off | | wake **with home** (always) | `TOUCHPAD_SWIPE_LEFT` / `_RIGHT` |
| Other touchpad gestures while off | | ignored | |
| GLOBAL_LOOK_UP (head wake) | screen off, `head_wake_up_enabled`, on head | **quick timeout** + wake; home if "should reset to home" | `HEAD_LOOK_UP` |
| Don (put on) | screen off | quick timeout + wake; home if "should reset to home" | `DEVICE_DONNED` |
| Eye gesture other than WINK and LOOK_AT_SCREEN | after the ordered broadcast completes | wake in place | `UNCATEGORIZED` |

Note on the eye-gesture row: the system server registers only WINK (when
`wink_for_picture_enabled`), and WINK is excluded from waking, so in the
stock configuration **no eye gesture wakes the screen** through this path;
the rule only matters if more gestures are registered. DON/DOFF go through
`DonManager` (§4), not this path. **[C]**
| Power key long press | | wake in place, close system dialogs, then shut down | `UNCATEGORIZED` |

Enum order of `WakeUpReason`: TOUCHPAD_TAP, TOUCHPAD_SWIPE_LEFT,
TOUCHPAD_SWIPE_RIGHT, DEVICE_DONNED, HEAD_LOOK_UP, UNCATEGORIZED.

## 3. Sleep sources **[C]**

| Source | Condition | Action |
|---|---|---|
| Timeout | normal / quick | AOSP power manager, values in §1 |
| Two-finger swipe down | screen on | sound effect 15, `goToSleep` |
| NUDGE head gesture | screen on, no touch sequence in progress, nudge-to-sleep enabled | `goToSleep(reason 3)` |
| Doff (taken off) | | screen off immediately, lock screen activation (delayed, or immediate if the doff was manual) |

Nudge-to-sleep enable rule: the setting `nudge_screen_off_enabled`
(default false) is honoured only when the lab property `persist.lab.ndg` is
true (its default is **true**); if the property is false, nudge-to-sleep is
**always on**. NUDGE is registered on SCREEN_ON and unregistered on
SCREEN_OFF.

## 4. On-head state machine (`DonManager`) **[C]**

States: DONNED, DOFFED, UNKNOWN. UNKNOWN exists internally (sensor state
before the first DON/DOFF event) but is never passed to the policy: the
manager does not notify while it defers to sensors with an unknown sensor
state, and the policy also ignores UNKNOWN if it ever arrived. Internally the
manager combines a *sensor state* (from DON/DOFF eye gestures) with a
*manual mode*:

| Manual mode | Reported state |
|---|---|
| DEFER_TO_SENSORS | the sensor state (UNKNOWN until the first sensor event; UNKNOWN is never reported) |
| FORCE_DOFFED | DOFFED |
| TIMED_FORCED_DONNED | DONNED |
| FORCE_DONNED | DONNED |

Transitions:

- **Boot**: TIMED_FORCED_DONNED (30 s timer; a partial wake lock is held for
  up to 10 x the timer).
- **Timer expiry** (in TIMED_FORCED_DONNED): to DEFER_TO_SENSORS if on-head
  detection is enabled, else FORCE_DONNED.
- **Power key short press** (only when provisioned):
  DEFER_TO_SENSORS -> FORCE_DOFFED; FORCE_DOFFED -> TIMED_FORCED_DONNED if
  detection is enabled, else FORCE_DONNED; TIMED_FORCED_DONNED or
  FORCE_DONNED -> FORCE_DOFFED.
- **Hinge** (switch code 1): closed -> FORCE_DOFFED; opened ->
  TIMED_FORCED_DONNED.
- **Setting `on_head_detection_enabled`** changes: on -> register DON/DOFF
  and go to DEFER_TO_SENSORS; off -> unregister and go to FORCE_DONNED.
- **Broadcast `com.google.glass.action.DON_CONTROL`** (sender needs
  `android.permission.FACTORY_TEST`), string extra `command`:
  `FORCE_DON` -> FORCE_DONNED, `FORCE_DOFF` -> FORCE_DOFFED,
  `RESUME_NORMAL` -> TIMED_FORCED_DONNED.
- Entering FORCE_DOFFED disables the gaze service (hub `pause = 1`);
  leaving it re-enables it.

Each change of the reported state (and only a change):

1. plays `/system/media/audio/ui/sound_don.ogg` or `sound_doff.ogg`
   (SoundPool, volume = max of stream 2, ring);
2. calls the policy (§5) with `manual = (mode != DEFER_TO_SENSORS)`;
3. writes event-log entries (donned / doffed with on-head-detection flag,
   time since last change, manual flag).

## 5. Policy on a don-state change **[C]**

- DONNED: enable input in the filter; touchpad `sleepmode = 0`; if the screen
  is off, quick timeout + wake (home if "should reset to home") with reason
  `DEVICE_DONNED`; cancel any pending lock-screen activation; register
  GLOBAL_LOOK_UP if `head_wake_up_enabled`.
- DOFFED: disable input in the filter (all in-flight sequences cancelled;
  touchpad, switch and other non-key events are no longer dispatched; the
  volume, camera and power keys are still processed, so the power key can
  force the on-head state); touchpad `sleepmode = 1`; screen
  off; activate the lock screen (immediately if manual, else delayed);
  unregister GLOBAL_LOOK_UP.
- Both: send a **sticky ordered broadcast**
  `com.google.android.glass.action.ON_HEAD_STATE_CHANGED` with boolean extra
  `is_on_head`, to all users.

## 6. Broadcasts sent **[C]**

| Action | Extras | When |
|---|---|---|
| `com.google.android.glass.action.ON_HEAD_STATE_CHANGED` (sticky, ordered) | `is_on_head` (boolean) | §5 |
| `com.google.android.glass.action.EYE_GESTURE` (ordered) | `gesture` (enum name string, e.g. `WINK`), `screen_off` (boolean) | an eye gesture the system server registered for: WINK when `wink_for_picture_enabled` is true |
| `android.intent.action.CAMERA_BUTTON` (ordered) | `android.intent.extra.KEY_EVENT` (a KEYCODE_CAMERA down event, long-press flag 128 when long), `screen_off`, `locked` | camera key released (short) or held (long) while the system handles the camera |
| `com.google.android.glass.action.CAMERA_BUTTON_DOWN` (ordered) | none | camera key down, device provisioned (camera preload 2 s) |
| `android.intent.action.BUG_REPORT` | none | THREE_LONG_PRESS on non-user builds |

All ordered broadcasts hold a partial wake lock until the last receiver
finishes. The system handles the camera key when the device is not set up,
the screen is off, or the device is locked; otherwise the key goes to apps.
When not provisioned a camera press only plays sound effect 10.

Other intents used: the system UI service
`com.google.android.glass.systemui/.SystemUIService` is started at boot;
home candidates `com.google.glass.home/.timeline.MainTimelineActivity`
(preferred) and `com.google.glass.nowtown/.home.HomeActivity` (setting
`nowtown_enabled` seeded false). The default IME is forced to
`com.google.glass.frameworkstubs/.SimpleIme`.

## 7. Wake locks **[C]**

| Tag | Kind | Use |
|---|---|---|
| `GlassSystemServer::Broadcast` | partial | ordered broadcasts |
| `GlassSystemServer::ScreenOffLaunch` | partial, not reference-counted, 5 s | starting home / handling an eye gesture from screen-off; released on SCREEN_ON |
| `GlassInputFilter::InputEvents` | partial, reference-counted | held for each input sequence (camera key, power key, touch, camera switch) that **starts with the screen off**, released at the end of the sequence |
| `DonManager_WakeLock` | partial | forced-donned timer |
| `HeadGestureService` | partial | while software head detectors run |
| `EyeGestureService::BroadcastWakeLock` | partial | eye gesture service |

## 8. System properties **[C]**

| Property | Default | Effect |
|---|---|---|
| `glass.gestureservice.start` | `1` (in `build.prop`) | start of the gesture services **[I]** (consumer not traced) |
| `glass.usersim` | false | use the XE screen-off gesture decoding on other products |
| `persist.lab.ndg` | true | see §3 nudge rule |
| `persist.lab.chvr` | false | camera switch preloads the camera |
| `persist.lab.at_l` | false | event-log DMP taps |
| `persist.lab.laser_rlu` | true | allow look-at-screen glance for notifications |
| `persist.lab.nf` | true | notification UI feature in GlassSystemUI |
| `persist.sys.lastrebootreason` | - | read by the reboot reporter |
| `ro.build.version.glass` | `XE24` | |

## 9. Open questions

- Screen-off timeout actually configured on a stock device
  (`Settings.System.screen_off_timeout`), which overrides the 15 s default.
- Lock screen timing details (`activation_delay_ms` consumer) were not
  studied; out of scope for R4.
- Whether any stock app consumes `ON_HEAD_STATE_CHANGED` for more than UI.
