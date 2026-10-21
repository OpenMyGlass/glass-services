# Reverse-engineering specs: stock Glass system services (XE24)

These specs describe what the stock Glass XE24 system services do, so that
`glassd` and the Android glue can be built as interoperable, behaviour-compatible
replacements **without** reading the stock binaries. They were written by a
separate reverse-engineering agent under the OpenMyGlass clean-room rule:
the binaries were studied only inside the unversioned workspace
`google-glass-project/_re/`; nothing decompiled or disassembled, no copied
comments and no Google file content beyond short interface identifiers is in
this directory.

Implementers: read these files and public sources only (GPL kernel, GDK
documentation). Do not open `_re/`.

## Source image

- Factory image `glass_1-img-5585826` (`ro.build.description = glass_1-user 4.4.4 XRH35 5585826 release-keys`, `ro.build.version.glass = XE24`).
- Kernel facts: `OpenMyGlass/kernel-omap`, branch
  `glass-omap-xrx13b-stable-3.4.113` (GPL-2.0, read directly).

Stock components studied:

| File | Size (bytes) | What it really is |
|---|---|---|
| `/system/lib/libglassinputdevice.so` | 13436 | uinput helper for the virtual (remote) touchpad, not a gesture classifier |
| `/system/lib/libglassgaze.so` | 177356 | eye-gesture service on the glasshub proximity sensor (wink, on-head, look at screen), not head gestures |
| `/system/lib/libglass_services_jni.so` | 46548 | JNI bridge for the eye and head gesture services, plus the nudge classifier |
| `/system/lib/libglass_sensorsutils.so` | 13516 | look-up (GLU) parameter encoding |
| `/system/lib/libinvensense_hal.so` (via `hw/sensors.glass_1.so`) | 46968 | sensor HAL; only its GLU/TAP interface was looked at |
| `/system/framework/glass-services.jar` | 119722 | system-server side Java services |
| `/system/framework/glass-core.jar` | 161662 | client-side managers and the GDK gesture recognizer |
| `/system/framework/services.jar` | 1454474 | only the Glass changes in `PowerManagerService` and `GlassSystemServer` |
| `/system/framework/glass-framework-res.apk` | 1486379 | integer resources (timeouts) |
| `/system/priv-app/GlassSettingsProvider.apk`, `GlassSystemUI.apk`, `GlassSettings.apk`, `GlassHome.apk`, `GlassBluetooth.apk` | - | only the classes that call the services |

## Files

| Spec | Content |
|---|---|
| [touchpad-gestures.md](touchpad-gestures.md) | touchpad device, protocol A, coordinate conventions, the gesture state machine with all thresholds, screen-off encoded gestures, `touchpad_manager`, the uinput virtual touchpad |
| [head-gestures.md](head-gestures.md) | look up to wake (DMP), relative look up, nudge to sleep, tap; sensors, axes, angles, parameter encoding, inhibition windows |
| [glasshub.md](glasshub.md) | what user space does with the glasshub driver: sysfs writes, `/dev/glasshub` stream, poll events, on-head, wink, look at screen, calibration files, firmware check (do not reflash) |
| [glass-services-jni.md](glass-services-jni.md) | JNI methods and callbacks, binder services and AIDL transaction codes, the Glass settings provider and its keys |
| [wake-policy.md](wake-policy.md) | screen wake/sleep rules, quick timeout, on-head state machine, broadcasts, wake locks, system properties |
| [kernel-interfaces.md](kernel-interfaces.md) | kernel-side view of the same devices, from GPL source |

## Certainty legend

- **[C]** certain: directly observed (a symbol, string, constant, class or
  method behaviour, or kernel source line).
- **[I]** inferred: follows from what was observed but was not confirmed in
  detail.
- **[?]** open: needs a device or more study.

## Cross-cutting interface names (quick reference)

Binder services: `head_gesture`, `eye_gesture`, `touchpad_manager`,
`glass_system_server`.

Permissions: `com.google.android.glass.permission.HEAD_GESTURES`,
`com.google.android.glass.permission.TOUCHPAD_GESTURES`.

Glass settings (provider `com.google.android.glass.settings`, table `system`):
`head_wake_up_enabled` (default true), `glu_angle_degs` (default 30),
`nudge_screen_off_enabled` (false), `wink_for_picture_enabled` (false),
`on_head_detection_enabled` (false), `notification_glance_enabled` (false),
`nowtown_enabled`, `lockscreen_enabled`, `lockscreen_attempt_count`.

Android settings used: `Settings.Global.device_provisioned`;
`Settings.Secure.default_input_method`, `enabled_input_methods`,
`selected_input_method_subtype` (forced to the Glass simple IME).

Broadcast actions: `com.google.android.glass.action.ON_HEAD_STATE_CHANGED`
(`is_on_head`), `com.google.android.glass.action.EYE_GESTURE` (`gesture`,
`screen_off`), `com.google.android.glass.action.CAMERA_BUTTON_DOWN`,
`android.intent.action.CAMERA_BUTTON` (`screen_off`, `locked`),
`com.google.glass.action.DON_CONTROL` (`command` = `FORCE_DON` /
`FORCE_DOFF` / `RESUME_NORMAL`), `android.intent.action.BUG_REPORT`.
Home intent extras: `screen_turned_on`, `locked`, `wake_up_reason`,
`wake_up_time`.

System properties: `glass.gestureservice.start`, `glass.usersim`,
`persist.lab.ndg`, `persist.lab.chvr`, `persist.lab.at_l`,
`persist.lab.laser_rlu`, `persist.lab.nf`.

Device paths: `/dev/input/event*` named `sensor00fn11` (touchpad),
`/dev/uinput` (`virtual_touchpad`), `/dev/glasshub`,
`/sys/devices/platform/omap_i2c.4/i2c-4/4-0035/` (glasshub),
`/sys/bus/rmi/devices/touchpad.fn01/sleepmode` and `touchpad.fn11/sleepmode`
(also reachable as `/sys/devices/touchpad/...`),
`/sys/bus/iio/devices/iio:device0/` (MPU-9150, `glu_*`, `tap_*`,
`event_glu`, `event_tap`), `/data/system/gaze/` (calibration and logs).

Stock data files in `/system/etc/gaze/` (Google data; names only, do not
copy): `boost_default_params.txt` (246520), `laser_1000ms_detector.txt`
(6979), `laser_detector_set.txt` (25), `multi_dur_wink_detector.txt` (141),
`nudge-detector.txt` (7355), `two-blink-300-550ms-detector.txt` (36713),
`two-blink-300-900ms-detector.txt` (79076), `two_blink_detector_set.txt`
(65), `wink-300-550ms-detector.txt` (107438), `wink-300-650ms-detector.txt`
(153834), `wink-400-800ms-detector.txt` (238261). The open stack needs its
own classifiers for nudge, wink, double blink and look-at-screen.

## Main open questions (need a device)

1. Touchpad axis maxima and density (to convert slop and velocities to device units).
2. `don_doff` value semantics and debounce; raw proximity levels on/off head.
3. Which IIO nodes the HAL writes when enabling look-up, and boot values of
   the secondary `glu_*` nodes.
4. Linux switch codes behind the hinge and camera switches.
5. Stock `screen_off_timeout` value on a provisioned device.
