# Head gestures (stock XE24 behaviour)

Clean-room specification from the XE24 factory image. No stock code is
reproduced. Legend: **[C]** certain, **[I]** inferred, **[?]** open.

Important correction to the task brief: **`libglassgaze.so` is not a head
library.** It implements the *eye* gestures (wink, blink, on-head, look at
screen) on the glasshub proximity sensor; see `glasshub.md`. Head gestures are
implemented in `glass-services.jar` (Java), `libglass_services_jni.so`
(native bridge plus a small boosted classifier), `libglass_sensorsutils.so`
(parameter encoding) and the InvenSense sensor HAL `libinvensense_hal.so`
together with the MPU-9150 DMP firmware in the kernel driver.

## 1. Gestures

Enum `com.google.android.glass.head.HeadGesture` (ordinal = wire id) **[C]**:

| Id | Name | Implemented by | Purpose in the stock system |
|---|---|---|---|
| 0 | GLOBAL_LOOK_UP | DMP firmware on the MPU-9150 ("hardware") | wake the screen by tilting the head up past an absolute angle |
| 1 | RELATIVE_LOOK_UP | Java, gyroscope integration | look up by at least 17 deg relative to where you were; used by notifications |
| 2 | NUDGE | Java plus native boosted classifier on gyroscope X | quick upward head nudge turns the screen off |
| 3 | ACCEL_TAP | DMP tap detector ("hardware") | experimental; effectively never delivered (§6) |

There is no "nod" gesture in use. Unused Java detectors exist in the jar
(nod-once, nudge-up threshold detector, accelerometer look-up); their
parameters are listed in §7 for reference only. **[C]**

## 2. Sensor data and conventions

### 2.1 Sensors used **[C]**

- Software detectors use the Android `SensorManager`, preferring sensors whose
  vendor is `Invensense` (else the default): gyroscope (type 4), rotation
  vector (11) and accelerometer (1). Only the **gyroscope** is actually
  required by the two software detectors in use. Rate: `SENSOR_DELAY_GAME`
  (constant 1). Samples are kept in a 1024-entry ring per sensor, with
  timestamps converted to seconds.
- Each running software detector is polled on its own thread every **20 ms**;
  while at least one detector runs, the service holds a partial wake lock
  (tag `HeadGestureService`).
- Hardware gestures use two vendor sensor types exposed by the InvenSense HAL
  (`sensors.glass_1.so` -> `libinvensense_hal.so`):

| Sensor | Android type | HAL name | Gesture |
|---|---|---|---|
| Glass look-up | `0x110000` (1114112) | `MPL GLU` | GLOBAL_LOOK_UP |
| DMP tap | `0x110001` (1114113) | `MPL TAP` | ACCEL_TAP |

  The native service looks them up with `ASensorManager_getDefaultSensor`
  on its own looper and event queue and enables/disables them with
  `ASensorEventQueue_enableSensor` / `disableSensor`. **[C]**

### 2.2 Axes and angles **[C]/[I]**

- Gyroscope X is the **pitch rate**; positive X rate = head tilting up.
  Both RELATIVE_LOOK_UP (integrates +X) and NUDGE (classifies X, checks
  Y and Z are quiet) rely on this. **[C]** The kernel orientation matrix that
  produces this frame is in `board-notle.c:1608-1621` (MPU-9150
  `orientation = {0,1,0, 0,0,1, 1,0,0}`). **[C]**
- Head pitch from gravity, as used by the settings UI that picks the wake
  angle: `pitch_deg = -atan2(gz, sqrt(gx^2 + gy^2))` in degrees, using the
  InvenSense gravity sensor (type 9, vendor `Invensense`, name containing
  `MPL`). Positive = looking up. **[C]**
- Y and Z gyroscope rates are yaw/roll-type motion; the nudge detector
  rejects motions with large Y or Z (§4). **[I]** exact physical labels not
  verified.

## 3. GLOBAL_LOOK_UP (look up to wake)

### 3.1 Behaviour **[C]**

- The detection runs inside the MPU DMP; the CPU can sleep. A detection
  arrives as one sensor event of type `0x110000`. The service ignores the
  event payload and stamps it with `SystemClock.elapsedRealtime()`.
- The service forwards it to clients only if all hold:
  1. the screen is **off**;
  2. no client is registered for RELATIVE_LOOK_UP;
  3. the look-up *inhibition window* is not active (set by clients for N ms,
     and set to 500 ms automatically after every accepted NUDGE).
  On forwarding it also starts the 1000 ms nudge inhibition (§4.3).
- The system server registers for it when the setting `head_wake_up_enabled`
  is true **and** the device is on head; it unregisters when off head
  (`wake-policy.md`).

### 3.2 Parameters and their encoding **[C]**

Parameters are carried as a text property list `KEY=VALUE;KEY=VALUE`
(keys sorted, floats printed with `%f`), handed to the HAL with the
vendor NDK call `ASensorEventQueue_setParameters(queue, gluSensor, string)`;
the HAL routes it when the sensor handle is 7 (GLU). The keys and defaults
(from `glass::NewGluParams` in `libglass_sensorsutils.so`):

| Key | Default | Meaning | DMP value written |
|---|---|---|---|
| `MAX_OUTLIER_REJECTION_FRACTION_OF_GRAVITY` | 1.5 | reject accel samples above 1.5 g | `round(f^2 * 2^28)` |
| `MIN_OUTLIER_REJECTION_FRACTION_OF_GRAVITY` | 0.5 | reject accel samples below 0.5 g | `round(f^2 * 2^28)` |
| `LEVEL_ANGLE_THRESH_DEGS` | 22 | re-arm angle (must come back below this) | `round(cos^2(deg) * 2^30)` |
| `TRIGGER_ANGLE_THRESH_DEGS` | 25 | fire angle | `round(cos^2(deg) * 2^30)` |
| `ROLL_ANGLE_THRESH_DEGS` | 10 | maximum roll | `round(cos^2(90 deg - deg) * 2^30)` |

(`cos` arguments in degrees converted with pi/180; "round" = add 0.5 and
truncate to unsigned 32-bit.)

`HeadGestureManager.setGlobalLookUpGestureParameters(trigger, hysteresis)`
sends only two keys: `TRIGGER_ANGLE_THRESH_DEGS = trigger` and
`LEVEL_ANGLE_THRESH_DEGS = trigger - hysteresis`. The HAL then rewrites the
five values above to the IIO sysfs nodes of the MPU driver, with the DMP
master-disabled around the write. **[C]** (the HAL writes all five members;
whether keys not present in the string keep their defaults is **[I]**: the HAL
object is built with the defaults and only the provided keys are parsed.)

Stock values used by the settings app (GlassSettings.apk): default trigger
angle **30 deg** (setting `glu_angle_degs`, stored in the Glass settings
provider), hysteresis **7 deg** (so level = 23 deg), user-selectable range
**10..40 deg**. They are applied at `BOOT_COMPLETED` and whenever the user
picks a new angle. **[C]**

### 3.3 Kernel side **[C]**

The IIO MPU driver (GPL) exposes the DMP look-up controls as sysfs
attributes `glu_outlier_max`, `glu_outlier_min`, `glu_level`, `glu_trigger`,
`glu_roll`, `glu_gyro_bias_lpf_gain`, `glu_relative_lookup_leak_gain`,
`glu_relative_angle_threshold`, `glu_horizontal_threshold_squared`,
`glu_version4_on`, plus `event_glu` (pollable event) and `tap_on`,
`tap_min_count`, `tap_threshold`, `tap_time`, `event_tap`
(`drivers/staging/iio/imu/inv_mpu/inv_mpu_core.c:802-1017`, `1465-1500`).
The HAL references exactly these names (strings in `libinvensense_hal.so`).
Values are written raw (big-endian 32-bit) into DMP memory keys.

When enabling look-up the HAL turns off compass, accel and gyro streaming,
(re)loads the DMP when needed, writes the parameters, enables the look-up
interrupt, sets a rate-like value of 200 on one node and enables the DMP
event interrupt. **[I]** (node identities for the 200 and the final enable were
not resolved; see open questions.)

## 4. NUDGE (head nudge up to turn the screen off)

### 4.1 Detection **[C]**

- Input: gyroscope frames at least 15 ms apart (faster frames are skipped).
- A native boosted cascade classifier (`BoostingNudgeDetector` JNI in
  `libglass_services_jni.so`, model file `/system/etc/gaze/nudge-detector.txt`,
  7355 bytes) is fed `(timestamp_ms, gyro_x)` per frame and says "positive"
  or not. The classifier resamples the signal to a fixed frame rate first.
  Model format and contents are Google data and are not specified; a
  replacement must train or hand-design its own detector.
- A positive is accepted only if:
  1. more than **0.5 s** passed since the last accepted nudge; and
  2. over the last **1.0 s** of a 60-frame buffer the other axes were quiet:
     `|max_y / max_x| < 0.83`, `|max_z / max_x| < 0.42`,
     `|y| < 3.2 rad/s` and `|z| < 2.0 rad/s` for both extremes.

A reasonable open replacement (from the unused Java threshold detector,
§7): pitch rate above +1.0 rad/s (liberal), 1.2 (moderate) or 1.4
(conservative), with the non-pitch axes below one third of the pitch rate
and their integrated angle below 20 deg, within a 0.75 s window. **[I]**

### 4.2 Service rules **[C]**

A detected NUDGE is forwarded only if the screen is **on** and the nudge
inhibition window is not active. On forwarding the service sets the
look-up inhibition to 500 ms and restarts the nudge inhibition.

### 4.3 Inhibitions **[C]**

| Window | Length | Started by |
|---|---|---|
| Nudge inhibition | 1000 ms (from start time) | any forwarded GLOBAL_LOOK_UP, RELATIVE_LOOK_UP or NUDGE |
| Look-up inhibition | 500 ms after a NUDGE, or N ms set by a client via `setGlobalLookUpInhibitionTimer(N)` | NUDGE, client |

The home app sets a 5000 ms look-up inhibition while a notification card is
shown and clears it (0) when the card closes. **[C]**

### 4.4 Who uses it **[C]**

The system server registers NUDGE when the screen turns on and unregisters it
when the screen turns off, only if nudge-to-sleep is enabled (setting
`nudge_screen_off_enabled`; see `wake-policy.md` for the lab property
override). On a forwarded NUDGE it puts the device to sleep with the
"gesture" sleep reason, unless a touch sequence is in progress.

## 5. RELATIVE_LOOK_UP **[C]**

- Gyroscope only, at least 2 samples.
- At most every **0.1 s**, integrate gyro X over the samples newer than the
  last trigger and newer than a **5 s** window, walking back from the newest
  sample (sum of `x * dt`). Fire when the running sum exceeds **17 deg**
  (0.2967 rad). After firing, only samples newer than the trigger count.
- Forwarded without screen checks; starts the nudge inhibition.
- Used by the notification UI (GlassSystemUI) as a one-shot "glance" trigger,
  and its registration suppresses GLOBAL_LOOK_UP.

## 6. ACCEL_TAP **[C]**

- DMP tap events (type `0x110001`). The native bridge passes
  `info = (int) event.data[0]` (tap direction 0..5: +X, -X, +Y, -Y, +Z, -Z)
  and `extra = 0` to Java.
- Java forwards ACCEL_TAP only for direction +X with `extra == 2`, so with the
  stock native code it is **never forwarded**; it can only be event-logged
  (lab property `persist.lab.at_l`). A replacement may omit it. **[C]** (from
  the observed argument values; behaviour on device **[?]**)

## 7. Unused detector parameters in the jar (reference only) **[C]**

| Detector | Parameters |
|---|---|
| Nudge-up (threshold) | up rate 1.0 rad/s; wait-for-down timeout 0.75 s; non-pitch silence window 0.1 s; non-pitch threshold 1/3 of pitch rate; non-pitch integrated limit 20 deg |
| Nod once | down rate -0.6 rad/s, up rate 0.3 rad/s, down-to-up scale -0.5; stillness before 0.3 s / recognition 0.2 s; non-primary axis limit 0.16 rad/s; stillness squared magnitude 0.01; timeouts 0.5 / 0.75 / 1.25 s |
| Look-up with accelerometer | trigger 27 deg from the up vector, hysteresis 7 deg, max roll 10 deg, low-pass gain 5 /s, reject accel samples differing from 9.81 by more than 4.905 m/s² |
| Container presets | nudge-up thresholds 1.0 / 1.2 / 1.4 rad/s; nod-down -0.4 / -0.6 / -0.8 rad/s; windows 0.5 / 0.75 / 1.0 s |

## 8. Native bridge (summary; full list in `glass-services-jni.md`)

`com.google.android.glass.head.HeadGestureService` natives, registered with
`RegisterNatives` from `JNI_OnLoad` of `libglass_services_jni.so`:

| Java method | Signature | Behaviour |
|---|---|---|
| `nativeInit` | `(Lcom/google/android/glass/head/HeadGestureService;)I` | creates the native object, its looper, sensor queue and looks up the GLU and TAP sensors; returns a handle |
| `nativeEnableDetector` | `(IIZ)Z` | gesture id 0 -> GLU sensor, 3 -> TAP sensor; enable or disable; other ids fail |
| `nativeSetGlobalLookUpGestureParameters` | `(IFF)Z` | §3.2 |

Callback into Java: instance method `handleGestureDetected(int gestureId,
long timestamp, int info, int extra)` (`(IJII)V`), called on the native looper
thread for each sensor event (id 0 for GLU, 3 for TAP).

`BoostingNudgeDetector` natives: `nativeInit()Z` (loads
`/system/etc/gaze/nudge-detector.txt`), `nativeReset()Z`,
`nativeUpdate(DF)Z` (timestamp ms, gyro X). Static, single global detector.

## 9. Open questions (need a device)

- Which IIO nodes receive the value 200 and the final enable in the HAL's
  look-up enable sequence, and the values of the other `glu_*` nodes
  (`glu_gyro_bias_lpf_gain`, `glu_relative_*`, `glu_horizontal_*`) at boot.
- Whether the DMP look-up fires once per crossing or repeatedly while the
  head stays up (re-arm through `LEVEL` suggests once per crossing). **[I]**
- Physical sign of gyro Y and Z on the worn device.
- Real-world nudge rates to tune an open detector.
