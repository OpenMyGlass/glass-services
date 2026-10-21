# glasshub from user space: wink, on-head, look-at-screen (stock XE24)

Clean-room specification. Sources: the stock user-space library
`/system/lib/libglassgaze.so` (studied as a binary; cited by C++ symbol and
string), its JNI user `libglass_services_jni.so`, the Java services in
`glass-services.jar`, the stock init script (ownership facts only), and the
GPL kernel driver `drivers/input/misc/glasshub.c` + `include/linux/i2c/glasshub.h`
(read directly; cited `file:line` against the copy used for this study, which
matches `kernel-omap` branch `glass-omap-xrx13b-stable-3.4.113`). The kernel
interface itself is described in depth in `kernel-interfaces.md`; this file
says what the stock user space *does* with it. Legend: **[C]** certain,
**[I]** inferred, **[?]** open.

The glasshub firmware (`/system/vendor/firmware/glasshub.s19`) is not studied
or described beyond what user space needs (§7).

## 1. Components

| Layer | Stock component | Notes |
|---|---|---|
| Kernel | `glasshub` I2C driver, address `0x35` on `omap_i2c.4` | sysfs + misc device `/dev/glasshub` |
| Native service | `glass::GazeService` singleton in `libglassgaze.so` (version string `GazeServiceVersion`) with modules `WinkModule`, `DonDoffModule`, `DoubleBlinkModule`, `LookAtScreenModule`, and the device wrapper `GlasshubProximitySensor` | runs inside `system_server` |
| JNI | `glass::NativeEyeGestureService` in `libglass_services_jni.so` | bridge to `EyeGestureService` |
| Java | `com.google.android.glass.eye.EyeGestureService` (binder name `eye_gesture`) and `DonManager` | see `glass-services-jni.md`, `wake-policy.md` |

## 2. Device paths and ownership **[C]**

- sysfs directory: `/sys/devices/platform/omap_i2c.4/i2c-4/4-0035/`
  (string in `GlasshubProximitySensor`; all attribute paths are built by
  appending the attribute name).
- Raw stream: `/dev/glasshub` (misc device, opened read-only).
- Presence test: the attribute `version` exists in the directory
  (`GlasshubProximitySensor::existsOnDevice`). If absent, the gaze service is
  "non-functional" and every eye gesture reports unsupported.
- The stock init script (`init.omap4430.rc`, section "glasshub") changes the
  owner of `/dev/glasshub` and of every glasshub attribute to `system:system`.
  A replacement daemon needs the same (or runs as root).
- Directory for calibration and logs: `/data/system/gaze/` (created on demand).

## 3. Raw data stream `/dev/glasshub` **[C]**

- Record: packed 10 bytes, little-endian: `int64 timestamp_ns`,
  `uint16 value` (`include/linux/i2c/glasshub.h:31-34`).
- The library reads up to 64 records per `read()` (640-byte buffer) and treats
  a short read that is not a multiple of 10 as an error.
- `value`: proximity reading, 14 bits (`& 0x3fff`). If `wink_flag_enable` is
  1, bit 14 (`0x4000`) is also passed through and marks "firmware wink
  detected" on that sample (`glasshub.c:195-198`, `673-683`).
- Timestamps are synthesized by the driver from the interrupt time and an
  averaged sample interval; nominal rate **32 Hz** (`PROX_INTERVAL`,
  `glasshub.c:184-192`; the library also hard-codes an expected data rate of
  32.0 Hz, `glass::GazeService::kExpectedDataRateHertz`). Clock: the driver's
  "robust clock" in ns; `current_timestamp` returns the same clock, which the
  library uses to align history with "now".
- Only one opener at a time (`-EBUSY` otherwise); opening flushes the FIFO;
  `lseek(fd, 0, SEEK_END)` flushes it too (`glasshub.c:2164-2246`). The
  library flushes by closing and reopening. The FIFO holds 64 records
  (`PROX_QUEUE_SZ`).
- Data flows only while `passthru_enable` is 1.

## 4. Attributes the stock user space writes or reads **[C]**

All writes are ASCII decimal, opened and written per call.

| Attribute | Written values | When |
|---|---|---|
| `passthru_enable` | `1` / `0` | start / stop streaming raw samples (high-power wink classification, calibration, logging) |
| `wink_enable` | `1` / `0` | arm / disarm the firmware's low-power wink interrupt |
| `wink_flag_enable` | `1` / `0` | written together with `wink_enable` (same value) |
| `don_doff_enable` | `1` / `0` | arm / disarm the firmware's on-head detection |
| `don_threshold`, `doff_threshold`, `don_max_threshold` | `%u` | after don/doff calibration, and at load from the parameter file |
| `detector_gain`, `wink_min`, `wink_max` | `%u` | when applying wink calibration ("MCU params") |
| `pause` | `1` / `0` | master disable / enable of the gaze service (§6) |
| `activity_threshold` | `%u` | setter exists; no caller seen **[I]** |
| `update_fw_enable`, `update_fw_data` | see §7 | firmware update at boot when needed |

Read: `don_doff` (state), `wink` (interrupt acknowledgement read), `version`,
`bootloader_version`, `flash_status`, `pause`, `current_timestamp`,
`prox_version`, and, for the debug dump only, `name`, `mcu_debug16`,
`pause_for_audio`, `led_drive`, `error_code`, `driver_flags`,
`last_timestamp`, `irq_status`, `last_irq_status`, `irq_timestamp`,
`timer_count`, `error_count`, `proxraw`.

`pause_for_audio` is not written by the gaze library or the audio HAL
(`audio.primary.glass_1.so`, `libglassalsa.so` contain no reference). **[C]**
Who writes it, if anyone, is open. **[?]**

Kernel clamps on writes (`glasshub.c:976-1360`): `doff_threshold` 0..10000,
`don_threshold` 1..10000, `don_max_threshold` 1..10000,
`activity_threshold` 1..10000, `activity_guardband` 1..255,
`activity_timeout` 1..255, `led_drive` 0..15, `wink_min` 1..2000,
`wink_max` 1..2000, `detector_gain` 0..2.

## 5. Event delivery **[C]**

- One worker thread (`GazeService::GlasshubProximitySensor::GlasshubThread`)
  blocks in `poll()` with no timeout on:
  1. a control FIFO (named pipe) `/data/system/gaze/gaze_service_control_pipe`
     (created with `mknod`) used to wake the thread for commands
     (start/stop streaming, pause, flush, "stream history to new listener");
  2. sysfs `don_doff`, polled for `POLLPRI | POLLERR`;
  3. sysfs `wink`, polled for `POLLPRI | POLLERR`;
  4. `/dev/glasshub` for `POLLIN | POLLERR` while streaming.
- The kernel signals 2 and 3 with `sysfs_notify` from its interrupt handler
  (`glasshub.c:748-770`).
- On a `don_doff` event: `lseek(0)` then `read()` up to 64 bytes; the state is
  the parsed number (the library expects 2 bytes, e.g. `"1\n"`); **1 = on head
  (donned), 0 = off head (doffed)** **[I]** (value semantics taken from the
  names; the kernel just reads firmware register 6). Listeners are called
  only when the state changes.
- On a `wink` event: `lseek(0)` + `read()` to re-arm, then call low-power
  wink listeners.
- Each poll wake holds a wake lock (`WakeLock_ProximitySensor_EventInterrupt`);
  while raw data listeners exist the library holds
  `Glasshub_ProxData_WakeLock` (reference counted). The thread sleeps 1 ms
  between loop iterations.
- Raw packets are kept in a ring buffer so a new listener can ask for recent
  history ("stream historical data for N ms").

## 6. Gaze service behaviour

### 6.1 Gesture ids and modules **[C]**

Gesture ids are the ordinals of `com.google.android.glass.eye.EyeGesture`:

| Id | Gesture | Module | Hardware gate |
|---|---|---|---|
| 0 | WINK | WinkModule | serial number's first character `> 'F'` (hardware rev G or later) |
| 1 | DOUBLE_WINK | none | same gate, but no module: unsupported |
| 2 | BLINK | none | unsupported |
| 3 | DOUBLE_BLINK | DoubleBlinkModule | none |
| 4 | DON | DonDoffModule | none |
| 5 | DOFF | same module as DON | none |
| 6 | LOOK_AT_SCREEN | LookAtScreenModule | none |
| 7 | LOOK_AWAY_FROM_SCREEN | same module as LOOK_AT_SCREEN | none |

Ids >= 8 are clamped to 8 ("invalid"). The serial number comes from
`glass::Utils::getGlassSerialNumber` in `libglassutils.so` (uses the CPU
serial / `ro.serialno` **[I]**). `isSupported(g)` = hardware present and a
module exists for `g`.

### 6.2 Master enable and pause **[C]**

- `enableGazeService(false)` (called by `DonManager` when it forces the
  off-head state) sets master enable false and writes `pause = 1`; `true`
  writes `pause = 0`. While paused any running data logger is stopped and
  restarted on resume.
- `start` refuses to run when master enable is false.

### 6.3 On-head (DON / DOFF) **[C]**

- Start: write `don_doff_enable = 1` and listen to `don_doff` events. Stop:
  `don_doff_enable = 0`. Not usable in one-shot/timeout mode.
- Each state change produces DON (state 1) or DOFF (state 0) as an eye
  gesture event to Java.
- The Java `DonManager` registers DON and DOFF only when the Glass setting
  `on_head_detection_enabled` is true (default **false**).
- Thresholds live in the firmware (`don_threshold`, `doff_threshold`,
  `don_max_threshold`). Defaults when nothing is calibrated: don **450**,
  doff **400**, don max **2000**, detector gain **2**
  (`glass::MCUParams::kDefault*`).
- Calibration (used by the settings app): begin interval -> stream raw
  samples (`passthru_enable = 1`) while the user wears and removes the
  device -> end interval. Samples are clamped to **250..4000**
  (`DonDoffCalibrator::DON_VALUE_LOWER_BOUND/UPPER_BOUND`); the interval
  closes itself after too many samples. The samples are split into two
  clusters (k-means style, 5 iterations); calibration fails when a cluster
  is too small or the two centres are too close. New don / doff / don-max
  thresholds are derived from the two centres (log/exp interpolation
  **[I]**), written to the three sysfs nodes (only while on-head detection
  is disabled) and saved to the parameter file (§6.6). A debug file
  `/data/system/gaze/don_doff_debug.txt` gets `minValue`, `maxValue`,
  `minCenter`, `maxCenter`. The exact formulas and limits were not
  extracted. **[?]**

### 6.4 Wink **[C]**

Two-stage design:

1. **Low power**: write `wink_enable = 1` and `wink_flag_enable = 1`. The
   firmware raises the `wink` interrupt on a wink candidate (using
   `wink_min`, `wink_max`, `detector_gain`).
2. **High power confirmation**: on the interrupt, start raw streaming
   (`passthru_enable = 1`), request recent history, and run the CPU
   classifier for **52 samples** (about 1.6 s at 32 Hz) after the interrupt;
   then stop streaming. A gap in timestamps or a stale (backward) timestamp
   resets the counters.
3. The CPU classifier (`CalibratedMultiDurationWinkDetector`, version
   "Multi-Duration 0.1.0") runs several boosted-cascade detectors for
   different wink durations (model set
   `/system/etc/gaze/multi_dur_wink_detector.txt` listing
   `wink-300-550ms-detector.txt`, `wink-300-650ms-detector.txt`,
   `wink-400-800ms-detector.txt`), keeps the one matching the user's
   calibration, and accepts a detection only if its magnitude and duration lie
   in the calibrated acceptable ranges. It enforces a minimum interval between
   detections.
4. A confirmed wink is sent to Java as gesture 0 (WINK).

- WINK requires a valid calibration (`/data/system/gaze/wink_calibration.txt`,
  loaded at service start). Without one the module does not start.
- Calibration flow (GlassGesture app): `startCalibrationInterval(WINK)`, the
  user winks a few times, `endCalibrationInterval`, `isCalibrationComplete`,
  `applyAndSaveCalibration`. The saved file holds the magnitude range, the
  duration range (ms) and the active detector index; the derived firmware
  parameters (`detector_gain`, `wink_min`, `wink_max`) are written to sysfs
  and stored in the parameter file.
- The model files are Google data; a replacement must provide its own
  classifier. Only the interface (raw stream + low-power interrupt) matters
  for compatibility.

### 6.5 Look at screen and double blink **[C]**

- LOOK_AT_SCREEN / LOOK_AWAY_FROM_SCREEN: `LookAtScreenModule` (version
  0.3.4) streams raw data and runs a boosted detector
  (`/system/etc/gaze/laser_detector_set.txt` -> `laser_1000ms_detector.txt`)
  with a single threshold from the user calibration file
  `/data/system/gaze/laser_params.txt`. It refuses to start when not
  calibrated. Supports one-shot and timeout starts. Used by the notification
  "glance" feature (setting `notification_glance_enabled`, default false; lab
  property `persist.lab.laser_rlu`, default true).
- DOUBLE_BLINK: `DoubleBlinkModule` with
  `/system/etc/gaze/two_blink_detector_set.txt` (300-550 ms and 300-900 ms
  models); one-shot or timeout start; no stock caller seen. **[I]**

### 6.6 Parameter file `/data/system/gaze/gaze_params.txt` **[C]**

Line-based `KEY = VALUE` text (first line a `#` comment), keys:
`VERSION` (current `3.0`; `2.0` files are converted), `GAIN`, `DON_THRESH`,
`DOFF_THRESH`, `DON_MAX_THRESH`, `WINK_MIN`, `WINK_MAX`. Missing keys take the
defaults of §6.3 (gain 2, don 450, doff 400, don max 2000). A replacement
does not need to read this file, but may, to inherit a user's calibration.

### 6.7 Logging (debug) **[C]**

`activateGazeLogging(true)` writes raw samples to
`/data/system/gaze/prox_data_<time>_<n>.txt` with a header of version,
device, epoch / uptime / elapsed times.

## 7. Firmware check at start (note only) **[C]**

At service start the library compares the hub's application version
(`version`, format `major.minor`) with the version embedded in the first
record of `/system/vendor/firmware/glasshub.s19` (the record must start with
`S1058480`; the next two hex bytes are major and minor). If they differ it
flashes: `update_fw_enable = 1`, write the S-record lines to
`update_fw_data`, `update_fw_enable = 254` (checksum), read `flash_status`
(must be `0`), `update_fw_enable = 0`. Experimental firmware (version with
bit 15 set) is reported but kept. The kernel side is in
`glasshub.c:1438-1956`.

**OpenMyGlass must not reflash the hub** (roadmap rule: device tests are
`fastboot boot` only, and the firmware blob is not ours). An open replacement
should only *read* `version` and log a mismatch.

## 8. Minimal compatible behaviour for an open daemon

1. Check `version` exists; read `don_doff` once at start.
2. On-head: write `don_doff_enable = 1`, poll `don_doff` for `POLLPRI`,
   re-read on each wake, report changes.
3. Wink (optional, needs calibration): `wink_enable = 1` +
   `wink_flag_enable = 1`, poll `wink`; on an interrupt, stream
   `/dev/glasshub` for ~1.6 s and confirm with an open classifier, or accept
   the firmware flag bit 14 as a low-confidence wink.
4. Pause the hub (`pause = 1`) when the user forces "off head" and resume
   on exit from that state.
5. Never write `update_fw_*`.

## 9. Open questions (need a device)

- Exact meaning of `don_doff` values (1 = donned assumed) and whether the
  firmware debounces.
- Typical raw values on and off head for the open calibration.
- Whether `wink_flag_enable` alone (without the CPU classifier) gives a
  usable false-positive rate.
- Who, if anyone, writes `pause_for_audio`.
