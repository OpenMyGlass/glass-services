# Glass system services: JNI layer and the interfaces exposed to apps (stock XE24)

Clean-room specification of `/system/lib/libglass_services_jni.so` and of the
Java services that use it (`/system/framework/glass-services.jar`, client
side in `/system/framework/glass-core.jar`). Legend: **[C]** certain,
**[I]** inferred, **[?]** open. Names below are interface identifiers (Java
class, method and binder names) and are listed for compatibility only.

## 1. Library overview **[C]**

- `libglass_services_jni.so` is loaded by `System.loadLibrary("glass_services_jni")`
  from `HeadGestureService`, `EyeGestureService` and `BoostingNudgeDetector`.
- `JNI_OnLoad` registers two native tables with `jniRegisterNativeMethods`:
  `com/google/android/glass/eye/EyeGestureService` and
  `com/google/android/glass/head/HeadGestureService`.
  `BoostingNudgeDetector` uses static `Java_...` symbol names instead.
- Dependencies: `libglassgaze.so` (eye gestures, see `glasshub.md`),
  `libglass_sensorsutils.so` (look-up parameter encoding), `libandroid.so`
  (NDK sensor API, including the vendor call
  `ASensorEventQueue_setParameters`), `libglassutils.so`, and standard
  Android libraries.
- The `int` "handle" passed back and forth is the native object pointer.

## 2. `EyeGestureService` natives **[C]**

| Java static native | JNI signature | Behaviour |
|---|---|---|
| `nativeInit(EyeGestureService)` | `(Lcom/google/android/glass/eye/EyeGestureService;)I` | create the native bridge, keep a global ref to the service object; look up the gaze service if the glasshub hardware exists ("Found gaze hardware!") |
| `nativeStart(int)` | `(I)V` | start the gaze service (`GazeService::start`) |
| `nativeDump(int)` | `(I)Ljava/lang/String;` | text dump for `dumpsys eye_gesture` |
| `nativeEnableGazeService(int, boolean)` | `(IZ)V` | master enable (pause/resume of the hub) |
| `nativeStartGazeLogging(int)` / `nativeStopGazeLogging(int)` | `(I)V` | raw data log on/off |
| `nativeIsGazeLogging(int)` | `(I)Z` | |
| `nativeIsGestureSupported(int, int gesture)` | `(II)Z` | |
| `nativeStartEyeGestureDetector(int, int gesture, boolean oneShot, int timeout)` | `(IIZI)Z` | Java always passes `oneShot = false`, `timeout = -1` |
| `nativeStopEyeGestureDetector(int, int gesture)` | `(II)Z` | |
| `nativeLoadCalibrationForEyeGesture` | `(II)Z` | |
| `nativeApplyAndSaveCalibrationForEyeGesture` | `(II)Z` | |
| `nativeStartCalibrationIntervalForEyeGesture` | `(II)Z` | |
| `nativeEndCalibrationIntervalForEyeGesture` | `(II)Z` | |
| `nativeClearCalibrationForEyeGesture` | `(II)Z` | |
| `nativeIsCalibrationCompleteForEyeGesture` | `(II)Z` | |

Callback: instance method `handleEyeGestureDetected(int gestureId, long
timestamp)` (`(IJ)V`), called from the gaze worker thread (attached to the
VM) for each detected eye gesture. The timestamp is the native detection time
**[I]** (the log line pairs it with "up time now"; units probably ms).
Exceptions thrown by the callback are logged and cleared.

## 3. `HeadGestureService` natives **[C]**

See `head-gestures.md` §8. Summary:
`nativeInit(HeadGestureService)I`, `nativeEnableDetector(IIZ)Z`,
`nativeSetGlobalLookUpGestureParameters(IFF)Z`; callback
`handleGestureDetected(IJII)V`. Sensor types `0x110000` (look-up) and
`0x110001` (tap). Look-up parameters go to the HAL as a
`KEY=VALUE;...` string through `ASensorEventQueue_setParameters`.

`BoostingNudgeDetector` (static natives): `nativeInit()Z`,
`nativeReset()Z`, `nativeUpdate(DF)Z`; model
`/system/etc/gaze/nudge-detector.txt`.

## 4. Binder services registered by the Glass system server **[C]**

All are added with `ServiceManager.addService` inside `system_server` by
`SystemServerHub.initialize()` (glass-services.jar), which is started by
`com.android.server.glass.GlassSystemServer` in `services.jar`.

| Binder name | Interface descriptor | Implementation |
|---|---|---|
| `head_gesture` | `com.google.android.glass.head.IHeadGestureManager` | `HeadGestureService` |
| `eye_gesture` | `com.google.android.glass.eye.IEyeGestureManager` | `EyeGestureService` |
| `touchpad_manager` | `com.google.android.glass.touchpad.ITouchpadManager` | `TouchpadService` |
| `glass_system_server` | plain `Binder` (dump only) | `SystemServerHub` |
| (others, out of scope) | timeline, directory, lock screen | `TimelineService`, `DirectoryService`, `LockScreenController` |

### 4.1 `IHeadGestureManager` (transaction codes) **[C]**

| Code | Method | Notes |
|---|---|---|
| 1 | `boolean register(HeadGesture, IHeadGestureCallback)` | permission `com.google.android.glass.permission.HEAD_GESTURES` |
| 2 | `boolean unregister(HeadGesture, IHeadGestureCallback)` | same permission |
| 3 | `boolean disableAll(boolean disable)` | master switch; same permission |
| 4 | `boolean setGlobalLookUpGestureParameters(float triggerDeg, float hysteresisDeg)` | **no** permission check |
| 5 | `void setGlobalLookUpInhibitionTimer(long ms)` | no permission check |
| 6 | `boolean isWithinGlobalLookUpInhibitionWindow()` | |

`IHeadGestureCallback` (`com.google.android.glass.head.IHeadGestureCallback`):
code 1 `onHeadGestureDetected(HeadGesture, long timestampMillis)`; two-way.
Timestamps are `SystemClock.elapsedRealtime()` ms.

### 4.2 `IEyeGestureManager` **[C]**

| Code | Method |
|---|---|
| 1 | `boolean register(EyeGesture, IEyeGestureCallback)` |
| 2 | `boolean unregister(EyeGesture, IEyeGestureCallback)` |
| 3 | `void enableGazeService(boolean)` |
| 4 | `void activateGazeLogging(boolean)` |
| 5 | `boolean isGazeLogging()` |
| 6 | `boolean isSupported(EyeGesture)` |
| 7 | `boolean loadCalibration(EyeGesture)` |
| 8 | `boolean applyAndSaveCalibration(EyeGesture)` |
| 9 | `boolean startCalibrationInterval(EyeGesture)` |
| 10 | `boolean endCalibrationInterval(EyeGesture)` |
| 11 | `boolean clearCalibration(EyeGesture)` |
| 12 | `boolean isCalibrationComplete(EyeGesture)` |

The eye gesture service performs **no permission check** (its check method
is empty). `IEyeGestureCallback` code 1
`onEyeGestureDetected(EyeGesture, long timestamp)`.

### 4.3 `ITouchpadManager` **[C]**

See `touchpad-gestures.md` §6.

### 4.4 Parcel encoding of the enums **[C]**

`HeadGesture` and `EyeGesture` are `Parcelable` enums written as their
ordinal (`writeInt(ordinal)`); in AIDL calls they are preceded by the usual
non-null marker `int 1` (or `int 0` for null). Ordinals are listed in
`head-gestures.md` §1 and `glasshub.md` §6.1.

### 4.5 Registration semantics (both gesture services) **[C]**

- One `RemoteCallbackList` per gesture. A detector runs only while the
  master switch is on and at least one callback is registered; it is started
  on the first registration and stopped when the last one leaves or dies.
- Detected gestures are delivered to every registered callback of that
  gesture on the service's handler thread.
- The client library (`HeadGestureManager` / `EyeGestureManager` in
  glass-core.jar) multiplexes local listeners onto a single binder callback
  per gesture, supports one-shot listeners (removed after first delivery),
  and is obtained with `HeadGestureManager.from(Context)` /
  `EyeGestureManager.from(Context)`.
- On the emulator (`Build.DEVICE == "generic_x86"`) registration is refused.

## 5. Settings migration done by the services at boot **[C]**

If a Glass setting is still unset, the services seed it from an old file in
`/data/system/gaze/` (file holds an integer; > 0 means true), else from the
default:

| Setting key | Old file | Default |
|---|---|---|
| `head_wake_up_enabled` | `glu_enable.txt` | true |
| `notification_glance_enabled` | `laser_enable.txt` | false |
| `wink_for_picture_enabled` | `wink_enable.txt` | false |
| `on_head_detection_enabled` | `don_doff_enable_state` | false |

## 6. Glass settings provider **[C]**

- Authority `com.google.android.glass.settings`, URI
  `content://com.google.android.glass.settings/system`; package
  GlassSettingsProvider.apk. SQLite `settings.db` (version 2), table
  `system (name TEXT PRIMARY KEY, value TEXT NOT NULL)`.
- Values are strings; booleans are `"true"`/`"false"`, ints decimal, the
  head-wake angle a float string.
- Writes require `android.permission.WRITE_SECURE_SETTINGS`. Per-key change
  notification URI: `content://com.google.android.glass.settings/system/<key>`.
- Keys (constants in `com.google.android.glass.provider.Settings`):
  `glu_angle_degs`, `head_wake_up_enabled`, `nudge_screen_off_enabled`,
  `wink_for_picture_enabled`, `on_head_detection_enabled`,
  `notification_glance_enabled`, `nowtown_enabled`, `lockscreen_enabled`,
  `lockscreen_attempt_count`, `layout_overlay_type`, `checkin_time`, and
  three tips flags `GLASS_TIPS_SAY_OK_GLASS`, `GLASS_TIPS_TAP_TO_EXPAND`,
  `GLASS_TIPS_TAP_TO_VIEW_MENU`.

## 7. Native parameter text format (`glass::SensorParamsBase`) **[C]**

Used for the look-up parameters (`libglass_sensorsutils.so`):

- Flattened form: `KEY=VALUE` pairs joined by `;`, keys in sorted order, no
  trailing separator.
- File form (`load`/`store`): one `KEY=VALUE` per line (Android
  `PropertyMap` syntax). No stock caller loads a GLU file. **[I]**
- Float values are printed with `%f`, ints with `%d`.

## 8. Things a replacement must provide for app compatibility

1. Binder services `head_gesture`, `eye_gesture`, `touchpad_manager` with the
   descriptors and transaction codes above, if stock Glass apps (camera,
   home, settings) must keep working on the open Android route.
2. The settings provider authority and keys of §6.
3. Broadcasts and wake behaviour in `wake-policy.md`.

On the plain-Linux route none of the binder names matter; the `glassd`
socket API replaces them, and these tables only document the semantics to
mirror.

## 9. Open questions

- Units of the eye gesture timestamp passed to Java (ms uptime assumed).
- Whether any third-party (GDK) app calls `eye_gesture` directly; the GDK
  public API did not expose it. **[?]**
