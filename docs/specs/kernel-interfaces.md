# Kernel interfaces consumed by Glass system services

Status: spec, derived from GPL kernel source only. Nothing here was observed on a
device. Items marked **[device]** need a check on real hardware before code
relies on them.

## Source

- Repository: `OpenMyGlass/kernel-omap`, branch `glass-omap-xrx13b-stable-3.4.113`
  (Google `glass-omap-xrx13b`, the XE24 kernel, with Linux 3.4.113 merged),
  commit `b6ea8564759ba36c96db4a07e2f5647c890c3f86`.
- Citations are `path:line` in that tree. The GPL source is open and may be
  read directly. This document only restates facts (names, paths, codes,
  ranges, timings). It contains no copied code.
- Board: `arch/arm/mach-omap2/board-notle.c` ("notle" is the Glass board name),
  config: `arch/arm/configs/notle_defconfig`.

## 0. Summary for implementers

| Function | Kernel interface | Notes |
|---|---|---|
| Touchpad | evdev, input name `sensor00fn11`, Synaptics RMI4 F11, MT protocol **A** | Screen-off gestures are encoded as fake touches at (100+dx, 100+dy) |
| Camera button | evdev, input name `gpio-keys`, `EV_KEY KEY_CAMERA` (212) | wake source |
| Power button | evdev, input name `twl6030_pwrbutton`, `EV_KEY KEY_POWER` (116) | PMIC button |
| Head gestures, wink, on-head | glasshub MCU: sysfs under the I2C device, `poll()` on `don_doff` and `wink`; prox stream on `/dev/glasshub` | no input device |
| IMU | IIO device `mpu9150` (`/sys/bus/iio/devices/iio:deviceN`, `/dev/iio:deviceN`) | gyro + accel + AK8975 compass via the MPU aux bus; DMP "GLU" event |
| Ambient light / prox | evdev `ltr506_als` (`ABS_MISC`) and `ltr506_ps` (`ABS_DISTANCE`), sysfs on the input devices | LTR-506ALS |
| Display on/off | fb blank (`FBIOBLANK` on `/dev/graphics/fb0`), Android early suspend via `/sys/power/state` | panel driver `panel_notle` |
| Brightness | `.../panel-notle-dpi/brightness` (effective 0..223 at the default cap) | never raise `brightness_limit` or `brightness_multiplier` |
| Wake locks | `/sys/power/wake_lock`, `/sys/power/wake_unlock`, `/proc/wakelocks` | Android wakelock API |

## 1. Touchpad

### 1.1 Driver and wiring

- Chip: Synaptics, sensor name `tm2240`, driven by the out-of-tree Synaptics
  RMI4 bus driver in `drivers/input/rmi4/` (board-notle.c:1479-1481).
  Not Cypress: all Cypress touch options are off in the defconfig
  (e.g. `TOUCHSCREEN_CYTTSP_CORE`, notle_defconfig:1449).
- Enabled options: `CONFIG_RMI4_BUS`, `RMI4_I2C`, `RMI4_GENERIC`, `RMI4_F05`,
  `RMI4_F09`, `RMI4_F11`, `RMI4_F34` (flash), `RMI4_DEV` (char dev),
  `RMI4_DEBUG` (notle_defconfig:1514-1534). `RMI4_F11_PEN` and
  `RMI4_VIRTUAL_BUTTON` are off.
- I2C: bus 3, address `0x20`, device type `rmi_i2c` (board-notle.c:1598).
  Bus registered at 384 kHz (board-notle.c:1935).
- Attention (IRQ): GPIO `GPIO_TOUCHPAD_INT_N`, active low, level triggered
  (board-notle.c:1482-1484, 1798, 1804), `IRQF_ONESHOT` threaded IRQ
  (rmi_driver.c:1142-1145, 1207). The IRQ is a system wake source
  (`irq_set_irq_wake`, board-notle.c:2104).
- Reset delay 100 ms (board-notle.c:1488).

### 1.2 Input device

- One input device per F11 sensor. Name `sensor00fn11`, phys
  `sensor00fn11/input0` (rmi_f11.c:2974-2979). The name prefix is hard coded,
  not derived from the RMI device name.
- A second device `rmi_mouse` is created only if the sensor reports relative
  data (`has_rel`) (rmi_f11.c:3008-3019). **[device]** whether it exists.
- Event types: `EV_SYN`, `EV_ABS`, `EV_SW`; `INPUT_PROP_DIRECT` is set
  (rmi_f11.c:2983-2988). `EV_REL` with `REL_X`/`REL_Y` only if `has_rel`.
- **No `EV_KEY` and no `BTN_TOUCH`** are declared on the main device. Touch
  state must be inferred from the presence of MT contacts in a frame.
- Switch: `SW_CAMERA_LENS_COVER` (0x09) is used as a "viewfinder" flag, see 1.5.

| Axis | Min | Max | Source |
|---|---|---|---|
| `ABS_MT_POSITION_X` | clip low (0) | device max X | rmi_f11.c:2711; board clip values are 0 (board-notle.c:1459-1462) |
| `ABS_MT_POSITION_Y` | 0 | device max Y | rmi_f11.c:2713 |
| `ABS_MT_PRESSURE` | 0 | 255 | rmi_f11.c:68, 2699 (value is the F11 Z byte) |
| `ABS_MT_TOUCH_MAJOR` | 0 | 15 | rmi_f11.c:69, 2701 (max of W_x, W_y) |
| `ABS_MT_TOUCH_MINOR` | 0 | 15 | rmi_f11.c:2703 (min of W_x, W_y) |
| `ABS_MT_ORIENTATION` | 0 | 1 | rmi_f11.c:70, 2705 (1 if W_x > W_y) |
| `ABS_MT_TRACKING_ID` | 1 | 10 | rmi_f11.c:71-72, 2707 |
| `ABS_MT_TOOL_TYPE` | 0 | `MT_TOOL_FINGER` | rmi_f11.c:2722 |

- Device max X/Y come from the controller's F11 control registers, not from
  platform data. They are readable from sysfs `maxPos` (1.4). Hints in the
  source: the default viewfinder rectangle uses X 940..1000 and Y 160..187
  (rmi_f11.c:87-90), so X spans at least ~1000 and Y at least ~187.
  **[device]** exact ranges (`evtest` or `maxPos`).
- Resolution (`ABS` resolution field) is not set (0). No physical units.
- Up to 10 fingers (`F11_MAX_NUM_OF_FINGERS`, rmi_f11.c:50).

### 1.3 Protocol (MT type A) and frame shape

- Board sets `type_a = 1` (board-notle.c:1475-1476), so slots are not
  initialised (rmi_f11.c:2715-2716) and each contact is followed by
  `SYN_MT_REPORT` (rmi_f11.c:1620-1621).
- Per contact the driver emits, in order: `ABS_MT_TRACKING_ID`,
  `ABS_MT_TOOL_TYPE`, `ABS_MT_PRESSURE`, `ABS_MT_TOUCH_MAJOR`,
  `ABS_MT_TOUCH_MINOR`, `ABS_MT_ORIENTATION`, `ABS_MT_POSITION_X`,
  `ABS_MT_POSITION_Y`, `SYN_MT_REPORT` (rmi_f11.c:1600-1621). One
  `SYN_REPORT` closes the frame (rmi_f11.c:2147).
- Tracking ID quirk: the value sent is the finger index `n_finger`, starting
  at **0**, although the declared minimum is 1 (rmi_f11.c:1601 vs 2707).
  Consumers must accept 0.
- Lift: a released finger sends nothing (rmi_f11.c:1533-1543, "Android 4.0"
  note). When the last finger lifts, the driver sends an empty
  `SYN_MT_REPORT` then `SYN_REPORT` (rmi_f11.c:2132-2139, 2147). An "all up"
  frame is therefore a frame with zero contacts.
- While `view_enable=1`, a finger whose **first** sample of the touch
  sequence lands inside the viewfinder rectangle is not reported as a
  contact; a finger that starts outside and moves in is still reported
  (rmi_f11.c:1913-1925, the `movement_finger_cnt[i] == 0` test). Excluded
  fingers are also not counted for the press gesture.
- Report rate: set by the controller firmware; F01 exposes a one-bit
  `reportrate` (1.4). Not stated in source. **[device]** measure with evtest.

### 1.4 Axis orientation

- Board axis alignment: `swap_axes = false`, `flip_x = false`,
  `flip_y = true`, no offsets, no clipping (board-notle.c:1453-1466). Y is
  inverted in the driver (`y = max_y - y`, rmi_f11.c:1571-1572).
- Mapping to the Glass frame (which end of the temple is X=0, whether Y=0 is
  top) is not stated in the source. **[device]** swipe front-to-back and
  top-to-bottom and record the sign.

### 1.5 Screen-off gestures (Google additions to F11)

When the RMI device is early-suspended (screen off; `early_suspended = 1`,
rmi_f11.c:3183-3193), the driver turns controller gesture bits into
synthetic touches (rmi_f11.c:1961-2097, generator at 1756-1795):

| Gesture | `POSITION_X` | `POSITION_Y` | Condition |
|---|---|---|---|
| single tap | 100 | 100 | controller tap preceded by an "early tap", outside viewfinder |
| double tap | 99 | 99 | same |
| press (long press) | 101 | 101 | only with exactly one finger, once per touch sequence |
| flick | 100 + x_flick_dist | 100 + y_flick_dist | controller flick distances |
| driver tap (if `gesture=1`) | 100 | 100 | box < 90x90, 5..250 ms, 1 finger, >1 events; once per suspend cycle |

Markers of a synthetic event: `ABS_MT_PRESSURE = 1`, `TOUCH_MAJOR = 15`,
`TOUCH_MINOR = 15`, and no `TRACKING_ID`, `TOOL_TYPE` or `ORIENTATION`
(rmi_f11.c:79-83, 1761-1773).

Frame structure in screen-off mode:

- Real contacts are still reported in early suspend
  (rmi_f11.c:1931-1932), interleaved with the synthetic gestures.
- Each synthetic gesture is its own frame: one contact, `SYN_MT_REPORT`,
  `SYN_REPORT` (rmi_f11.c:1775-1776).
- Before a press gesture the driver closes the current real-finger frame
  with an extra `SYN_REPORT` (rmi_f11.c:2071-2075). The outer handler then
  closes its own frame with another `SYN_REPORT` (rmi_f11.c:2147).
- So the frame that follows a gesture can be empty or partial. Consumers
  must not read it as "all fingers up". Classify each frame by the
  synthetic markers first, and track real contacts separately. Offsets:
rmi_f11.c:102-109. Driver-tap thresholds: rmi_f11.c:92-97. Each synthetic
event takes a 250 ms wakelock `touchpad_wakelock` (rmi_f11.c:28, 1794,
2895). The same wakelock is taken while any finger is down (rmi_f11.c:2127-2130).

The early-suspend state is cleared on late resume (rmi_f11.c:3201-3211).
These gestures exist only with Android early suspend; on a kernel without
`CONFIG_HAS_EARLYSUSPEND` the plain `suspend`/`resume` hooks are used instead
(rmi_f11.c:3276-3285).

The controller is **not** put to sleep on early suspend: the F01
early-suspend handlers are compiled out so that the pad can wake the device
(rmi_f01.c:1413-1424). The RMI early-suspend level is
`EARLY_SUSPEND_LEVEL_BLANK_SCREEN + 1` (rmi_driver.c:1131-1137).

Viewfinder: if `view_enable=1` and exactly one finger is inside the
rectangle with no other finger, `SW_CAMERA_LENS_COVER` is reported as 1,
otherwise 0 (rmi_f11.c:1940-1958).

### 1.6 sysfs

RMI bus devices live under `/sys/bus/rmi/devices/`. The sensor is named
`touchpad` (rmi_bus.c:150, bus name `rmi` rmi_bus.c:118); functions are
`touchpad.fnXX` (rmi_driver.c:713). **[device]** confirm paths.

`/sys/bus/rmi/devices/touchpad.fn11/` (rmi_f11.c:175-184, 1466-1476, 3325-3620):

| Attribute | Mode | Semantics |
|---|---|---|
| `maxPos` | RO | `"<max_x> <max_y>"` of sensor 0 |
| `gesture` | RW | 0/1, enables the driver tap synthesiser (default 0) |
| `sleepmode` | WO | 0/1. 1 = drop all finger processing, clear state, report viewfinder 0 and an empty frame; 0 = resume (rmi_f11.c:3448-3500, 3163-3167) |
| `view_enable` | RW | 0/1, viewfinder cut-out (default 0) |
| `view_dim` | RW | `"min_y max_y min_x max_x"`, validated against `maxPos`; default `160 187 940 1000` |
| `view_val` | RO | current viewfinder switch value |
| `relreport` | RW | 0/1 relative reporting |
| `rezero` | WO | 1 = issue F11 rezero command |
| `abs_pos_filt` | RW | F11 ctrl0 absolute position filter bit |
| `z_touch_threshold`, `z_touch_hysteresis` | RW | only if ctrl29/30 exist |

`/sys/bus/rmi/devices/touchpad.fn01/` (rmi_f01.c:422-470): `productinfo`,
`productid`, `manufacturer`, `datecode` (RO), `sleepmode` (RW, 0 normal,
1 sensor sleep; rmi_f01.h:104-110), `nosleep` (RW), `chargerinput` (RW),
`reportrate` (RW, 0/1, rmi_f01.c:714-735), `doze_interval`,
`wakeup_threshold`, `doze_holdoff` (RW), `configured`, `unconfigured`,
`flashprog`, `statuscode`, `interrupt_enable` (RO), `reset` (WO).

`/sys/bus/rmi/devices/touchpad/`: `enabled` (RW, deprecated), `bsr` (RW)
(rmi_driver.c:371-377).

Char device: `RMI4_DEV` creates class `rmidev`, node `rmi/rmi0`, which gives
raw register access (rmi_dev.c:31-32, 315-322, 372). F34 is the firmware
flash function. Services must not use either.

## 2. glasshub (wink, on-head, IR proximity)

### 2.1 Wiring

- Driver `drivers/input/misc/glasshub.c`, version `0.26`
  (glasshub.c:42-43), `CONFIG_INPUT_GLASSHUB=y` (notle_defconfig:1512).
  A small MCU with an IR proximity sensor that runs on-head (don/doff) and
  wink detection itself.
- I2C bus 4, address `0x35`, flag `I2C_CLIENT_WAKE` (board-notle.c:1760-1762),
  so the sysfs directory is expected at `/sys/bus/i2c/devices/4-0035/`.
  **[device]**
- IRQ: GPIO "blink" (`gpio_int_no`, board-notle.c:1812, 1829,
  2147-2157), falling edge, threaded, wake-enabled (glasshub.c:2354-2365).
- Part ID register must read `0xbb` (glasshub.c:123). Minimum MCU app
  version 1.12; older firmware disables everything except firmware update
  (glasshub.c:46, 2264-2290). Bootloader version 2 is refused
  (glasshub.c:2322-2350).

### 2.2 No input device

A `ps_input_dev` field exists but no input device is registered
(glasshub.c:274; no `input_register_device` in the file). User space gets
events through:

1. `sysfs_notify()` on the attributes `don_doff` and `wink`
   (glasshub.c:755-769). Use `poll()`/`select()` with `POLLPRI|POLLERR` on an
   open fd of the attribute, then re-read from offset 0.
2. The misc device `/dev/glasshub` for the timestamped proximity stream (2.4).

### 2.3 sysfs attributes

All under the I2C client device. Modes: `RO` = 0440, `WO` = 0220,
`RW` = 0660 (owner and group only, glasshub.c:143-145). Values are decimal
unless noted. `show_reg` returns `65535` (0xffff) on I2C failure
(glasshub.c:835-850). `store_reg` clamps to [min, max] and always returns
success (glasshub.c:852-870). Reading or writing an MCU register boots the
MCU app if needed (`boot_device_l`, 100 ms per attempt, up to 5 attempts,
glasshub.c:509-517).

Always present ("bootmode" group, glasshub.c:2133-2145):

| Attribute | Mode | Semantics |
|---|---|---|
| `version` | RO | MCU app version `"major.minor"` |
| `bootloader_version` | RO | decimal |
| `driver_version` | RO | `0.26` |
| `driver_flags` | RO | hex driver state bits (glasshub.c:132-140) |
| `irq` | RO | IRQ info (glasshub.c:1957) |
| `disable` | RW | 1 = reset the MCU and keep it disabled; 0 = allow boot again |
| `debug` | RW | 0/1 kernel log verbosity |
| `update_fw_enable`, `update_fw_data`, `flash_status` | RW/WO/RO | firmware update path, see 2.6. **Never use.** |

Present when the MCU firmware is supported (main group, glasshub.c:2089-2131):

| Attribute | Mode | Register | Range on write | Semantics |
|---|---|---|---|---|
| `don_doff_enable` | RW | 5 | 0..1 | enable on-head detection |
| `don_doff` | RO, notified | 6 | n/a | current on-head state (MCU value; **[device]** 1 = on head?) |
| `wink_enable` | RW | 7 | 0..1 | enable wink detection |
| `wink` | RO, notified | 16 | n/a | wink status; **reading clears it** (glasshub.c:1202-1206) |
| `wink_flag_enable` | RW | driver flag | 0/1 | keep bit 14 (wink marker) in `/dev/glasshub` samples |
| `wink_min`, `wink_max` | RW | 0x8c, 0x8d | 1..2000 | wink magnitude window |
| `passthru_enable` | RW | 4 | 0..1 | stream raw prox samples to `/dev/glasshub` |
| `proxraw` | RO | 0x82 | n/a | one raw prox reading |
| `ambient_enable` | RW | 18 | 0..1 | enable ambient light readings |
| `visible`, `ir` | RO | 0x85, 0x87 | n/a | raw visible and IR light |
| `doff_threshold` | RW | 0x80 | 0..10000 | prox threshold for "off head" |
| `don_threshold` | RW | 0x81 | 1..10000 | prox threshold for "on head" |
| `don_max_threshold` | RW | 0x8b | 1..10000 | upper prox bound for "on head" |
| `activity_threshold` | RW | 0x8a | 1..10000 | |
| `activity_guardband` | RW | 19 | 1..255 | |
| `activity_timeout` | RW | 8 | 1..255 | units not stated **[device]** |
| `led_drive` | RW | 9 | 0..15 | IR LED drive level |
| `detector_gain` | RW | 12 | 0..2 | |
| `pause` | RW | 15 | 0/1 | pause MCU processing (OR-ed with `pause_for_audio`) |
| `pause_for_audio` | RW | 15 | 0/1 | second pause reason, e.g. during audio (glasshub.c:1251-1324) |
| `calibrate` | WO | flash | nonzero | sweeps LED levels 0..7, 3 samples each with 35 ms gaps, **writes calibration to MCU flash** at 0x9ff0 (glasshub.c:1071-1142). Do not call from services. |
| `calibration_values` | RO | flash | n/a | 8 space-separated values |
| `prox_version` | RO | 13, 14 | n/a | `"0xPP 0xSS"` prox part and sequence ID |
| `error_code`, `error_count`, `timer_count` | RO | 10, 0x87, 0x89 | n/a | diagnostics |
| `mcu_debug`, `mcu_debug16` | RW | 17, 0x88 | 0..255 | diagnostics |
| `sample_count` | RO | driver | n/a | prox samples read |
| `irq_timestamp`, `last_timestamp`, `current_timestamp` | RO | driver | n/a | ns, `read_robust_clock()` |
| `irq_status` | RO | 64 (virtual, no clear) | n/a | hex |
| `last_irq_status` | RO | driver | n/a | hex |

Register map: glasshub.c:64-103. Interrupt status bits: `0x01` passthru
data, `0x02` wink, `0x04` don/doff, `0x08` debug, `0x80` overflow
(glasshub.c:114-120).

### 2.4 `/dev/glasshub` (prox stream)

- Misc device, dynamic minor, name `glasshub` (glasshub.c:2257-2261).
  Registered only together with the main sysfs group.
- Exclusive open: a second `open()` fails with `EBUSY`. Open flushes the
  FIFO (glasshub.c:2164-2172).
- Samples are buffered only while the device is open (glasshub.c:667).
- `read()`: blocks until data is available; `count` is rounded down to a
  multiple of the record size; less than one record gives `EINVAL`
  (glasshub.c:2185-2214).
- `poll()`: `POLLIN|POLLRDNORM` when data is queued (glasshub.c:2225-2246).
- `lseek(fd, 0, SEEK_END)` flushes the FIFO; any other seek gives `EINVAL` (glasshub.c:2216-2223).
- No ioctls.
- Record `struct glasshub_data_user`, packed, 10 bytes
  (include/linux/i2c/glasshub.h:32-35):

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | `timestamp`, signed 64-bit, ns, host endianness (ARM little-endian) |
| 8 | 2 | `value`, unsigned 16-bit |

- `value` is masked to 14 bits (`0x3fff`), or to 15 bits (`0x7fff`) when
  `wink_flag_enable=1`, in which case bit 14 (`0x4000`) marks a sample where
  the MCU detected a wink (glasshub.c:195-198, 680-686, 714).
- FIFO depth 64 records (glasshub.c:181, 308). When it is full, `kfifo_put`
  discards the **new** record (glasshub.c:717), so the queued data is kept.
  At most 64 samples per interrupt are kept (glasshub.c:667); the rest of
  the burst is still drained from the MCU and thrown away. A slow reader
  therefore sees stale samples, then a gap. Read promptly, or flush with
  `lseek(fd, 0, SEEK_END)` before resuming.
- Timestamps: the hard IRQ takes `read_robust_clock()`; samples in a burst
  are spaced by a running average interval that starts at 1/32 s
  (≈31.25 ms, so about 32 Hz) and is re-estimated every 32 single-sample
  interrupts (glasshub.c:184-193, 689-742). The clock source of
  `read_robust_clock()` is board code (include/linux/time.h:141-145).
  **[device]** confirm it matches `CLOCK_BOOTTIME` or `CLOCK_MONOTONIC`.

### 2.5 Interrupt, suspend and timing

- Hard IRQ saves a timestamp and wakes the threaded handler; it calls
  `pm_request_resume` (glasshub.c:811-831).
- Threaded handler reads the status register, drains prox data until the
  end-of-data flag `0x8000`, then handles don/doff and wink
  (glasshub.c:580-794).
- `suspend_noirq` returns `EBUSY` while a handler is pending or running,
  which aborts system suspend; deferred interrupts run in `resume_noirq`
  (glasshub.c:2446-2480).
- No wakelock is taken by the driver. A service that reacts to
  don/doff or wink in suspend must take its own wakelock.
- Timings: MCU boot 100 ms (glasshub.c:516), reset 50 ms (glasshub.c:567),
  write retries 10 ms (glasshub.c:340, 370), 3 I2C retries.

### 2.6 Firmware update path (note only, never use)

The MCU bootloader accepts S19 text written to `update_fw_data` between
`update_fw_enable` 1 and 0. Writing 255 targets the bootloader itself and
can brick the MCU (glasshub.c:221-269, 1485, 1811). Services must not
touch these attributes or `calibrate`.

## 3. IMU and other sensors

### 3.1 InvenSense MPU-9150

- Chip: MPU-9150 (MPU-6050 gyro+accel die plus AK8975 magnetometer),
  I2C bus 4, address `0x68`, type `mpu9150`, flag `I2C_CLIENT_WAKE`
  (board-notle.c:1742-1745). Magnetometer is a secondary slave at `0x0C`
  behind the MPU aux I2C master (board-notle.c:1614-1617).
- Driver: InvenSense IIO driver `drivers/staging/iio/imu/inv_mpu/`
  (`CONFIG_INV_MPU_IIO=y`, notle_defconfig:2621). The older char-device
  driver `drivers/misc/mpu` (`/dev/mpu`) is **not** built
  (`CONFIG_INV_MPU` is not set, notle_defconfig:1226), and neither are the
  input-subsystem `mpu6050`/`ak8975` drivers (notle_defconfig:1510-1511).
  So there is no `/dev/mpu` and no IMU input device.
- IRQ: `GPIO_MPU9000_INT`, rising edge, shared, threaded
  (board-notle.c:1797, 1822-1823; inv_mpu_ring.c:840-846), wake-enabled
  (board-notle.c:2139).

### 3.2 User-space access (IIO)

- IIO device name `mpu9150` (`indio_dev->name = id->name`,
  inv_mpu_core.c:1946). Expected sysfs `/sys/bus/iio/devices/iio:device0/`
  and node `/dev/iio:device0` (only one IIO driver is enabled). Trigger
  name pattern `"%s-dev%d"` (inv_mpu_trigger.c:67). **[device]** index.
- Driver attributes are flat in the device directory (the `mpu` group name
  is dropped by the staging IIO core, industrialio-core.c:670-708).
- Channels (inv_mpu_core.c:1425-1444), scan type signed 16-bit unless noted:

| Scan index | Channel | Notes |
|---|---|---|
| 0..3 | `quaternion` R, X, Y, Z | signed 32-bit, only with DMP and `quaternion_on` |
| 4..6 | `anglvel` X, Y, Z | gyro |
| 7..9 | `accel` X, Y, Z | accel |
| 10..12 | `magn` X, Y, Z | compass, sensitivity-adjusted in the driver |
| 13 | `timestamp` | soft timestamp, ns from `read_robust_clock()` (inv_mpu_core.c:52-55, inv_mpu_ring.c:346-356) |

- Buffer record (inv_mpu_ring.c:650-686): enabled channels packed in scan
  order, then padded to 8 bytes; Google added a format byte in the last
  padding byte before the timestamp (`0x01` quat, `0x02` gyro, `0x04` accel,
  `0x08` compass, inv_mpu_ring.c:536-539, 680-683), then the 64-bit
  timestamp. No format byte is written when only quaternion data is
  enabled (inv_mpu_ring.c:680-681). Readers must handle this byte. **[device]** dump a few records.
- Recommended sequence (README in the driver directory, lines 281-313):
  `power_state=1`; set `sampling_frequency`; set `gyro_enable`,
  `accl_enable`, `compass_enable`; enable scan elements; `buffer/enable=1`;
  read `/dev/iio:device0`. The README says `fifo_rate`; the actual IIO
  attribute is `sampling_frequency` (README:287; inv_mpu_core.c:1447-1449).
  Most settings return `EPERM` while the buffer is
  enabled or the chip is asleep (inv_mpu_core.c:546-549, 561).

### 3.3 Rates and ranges

| Item | Value | Source |
|---|---|---|
| `sampling_frequency` | 4..1000 Hz, default 50 | inv_mpu_iio.h:544, 564-565; inv_mpu_core.c:651 |
| `sampling_frequency_available` | `10 20 50 100 200 500` | inv_mpu_core.c:1447 |
| Effective rate | 1000 / (1000/f) Hz (integer divider) | inv_mpu_core.c:662 |
| Compass rate | about f/10 (divider `10*f/1000 - 1`, AK8975 max ~100 Hz) | inv_mpu_iio.h:532; inv_mpu_core.c:655-660 |
| Gyro full scale | index 0..3 = ±250/500/1000/2000 dps, default 2000 | inv_mpu_core.c:283-287, 391-396 |
| Accel full scale | index 0..3 = ±2/4/8/16 g, default 2 g | inv_mpu_core.c:305-309, 401-402 |
| LPF | 42 Hz at init, then set from the rate | inv_mpu_core.c:289-291 |
| DMP output rate | default 25 Hz | inv_mpu_iio.h:545 |

The `*_scale` attribute reads back the range in dps or g (not a per-LSB
factor) and takes the range **index** on write (inv_mpu_core.c:387-407,
564-570). Raw counts: 16-bit signed over the full scale.

### 3.4 Orientation matrices (board platform data)

Exposed read-only as `gyro_matrix`, `accl_matrix`, `compass_matrix`
(`"a,b,c,d,e,f,g,h,i"`, inv_mpu_core.c:1049-1068). The driver does **not**
rotate data; user space must apply them.

| Sensor | Matrix (row-major) | Source |
|---|---|---|
| gyro and accel | `[0 1 0; 0 0 1; 1 0 0]` | board-notle.c:1610-1612 |
| compass | `[1 0 0; 0 0 -1; 0 1 0]` | board-notle.c:1619-1621 |

With the usual InvenSense convention (out = M · chip), gyro/accel give
out.x = chip.y, out.y = chip.z, out.z = chip.x. The target frame is the
Android sensor frame of the device. **[device]** confirm sign and which
physical direction of the Glass frame each output axis has (lay the device
flat, then on its side).

### 3.5 DMP features and events

- `dmp_firmware` is a binary sysfs file; the DMP image must be written
  from user space as a whole, then `firmware_loaded` reads 1
  (inv_mpu_core.c:1847-1862; README). The write is rejected unless the
  image is exactly 2962 bytes with CRC32 `0x130bd18d`
  (inv_mpu_iio.h:515-516; inv_mpu_misc.c:1751-1762), so only one specific
  InvenSense image is accepted. That image is not in the kernel tree.
  **Open question:** where a redistributable copy of it could come from.
- Event attributes, read after `poll()`: `event_tap`, `event_orientation`,
  `event_display_orientation`, `event_glu` (inv_mpu_core.c:1510-1514).
- `event_glu` is a Google addition, notified when the DMP raises
  `GLU_INT_STATUS` (inv_mpu_ring.c:719-724); the read value is always `1`
  (inv_mpu_core.c:1095-1098). Its tuning attributes: `glu_outlier_max`,
  `glu_outlier_min`, `glu_level`, `glu_trigger`, `glu_roll`,
  `glu_gyro_bias_lpf_gain`, `glu_relative_lookup_leak_gain`,
  `glu_relative_angle_threshold`, `glu_horizontal_threshold_squared`,
  `glu_version4_on` (inv_mpu_core.c:1465-1487, 802-840). The names
  ("relative look up") suggest the head-tilt "look up to wake" gesture.
  **[device]** semantics and units; only works with DMP firmware loaded and
  `dmp_on=1`.
- Other attributes: `power_state`, `temperature`, `self_test`, `key`,
  `reg_dump`, `gyro_enable`, `accl_enable`, `compass_enable`, `dmp_on`,
  `dmp_int_on`, `dmp_event_int_on`, `dmp_output_rate`, `orientation_on`,
  `quaternion_on`, `display_orientation_on`, `tap_on`, `tap_time`,
  `tap_min_count`, `tap_threshold` (inv_mpu_core.c:1450-1524, 1528-1578).

### 3.6 Suspend

- `suspend_noirq` does nothing on notle (the abort-on-busy logic is
  compiled out under `CONFIG_MACH_NOTLE`, inv_mpu_core.c:2054-2068; hook
  registered at inv_mpu_core.c:2070-2072).
- The chip keeps its configuration over suspend; the IRQ is a wake source,
  so a running FIFO or DMP event can wake the system. A service must turn
  the buffer or `power_state` off before suspend if it does not want that.
  **[device]**

### 3.7 Ambient light and proximity: LTR-506ALS

- I2C bus 4, address `0x3a`, flag `I2C_CLIENT_WAKE`
  (board-notle.c:1748-1750), driver `drivers/input/misc/ltr506als.c`.
- Input devices: `ltr506_als` with `EV_ABS ABS_MISC` 0..65535
  (ltr506als.c:1777-1779) and `ltr506_ps` with `EV_ABS ABS_DISTANCE`
  0..4095 (ltr506als.c:1815-1817; include/linux/i2c/ltr506als.h:108-113).
- Misc devices `ltr506_ls` and `ltr506_ps`; open/release only, no ioctl
  handler (ltr506als.c:884-916, 1000-1032). The `LTR506_IOCTL_*` numbers in
  the header are not wired. Closing `ltr506_ps` disables the PS.
- sysfs on the input devices (`/sys/class/input/inputN/`,
  ltr506als.c:1722-1758): ALS `als_adc`, `als_adc_ch1`, `als_adc_ch2`,
  `als_enable`, `als_gain`, `als_resolution`, `als_meas_rate`,
  `als_threshold`, `als_filter_interrupts`; PS `ps_adc`, `ps_enable`,
  `ps_gain`, `ps_led`, `ps_meas_rate`, `ps_pulse_cnt`, `ps_threshold`,
  `ps_filter_interrupts`. Also on the I2C device: `als_enable`,
  `ps_enable`, `dump_regs`, `status` (ltr506als.c:1703-1708). Mode 0666.
- Board settings: ALS gain 0.005 lux/count (range 0.01-320 lux), ALS
  measurement every 500 ms, PS every 50 ms, interrupt not a wake source
  (board-notle.c:1626-1730). A workaround keeps the PS on while ALS is on
  and reports ALS channel 1 (ltr506als.c:528-530, 1861).
- Early suspend keeps ALS and PS running (board flags 0) and forces a fresh
  ALS value after resume (ltr506als.c:532-537, 1985-2045).

## 4. Display, backlight and power states

### 4.1 Panel

- DSS device `notle_nhd_panel`, driver `panel_notle`, DPI on LCD2, 640x360,
  24-bit (board-notle.c:591-606). The panel is lit by LEDs driven through
  an iCE40 FPGA on SPI (board-notle.c:607-620). Driver
  `drivers/video/omap2/displays/panel-notle-dpi.c`.
- There is **no** backlight-class device for the panel (`FB_BACKLIGHT` is
  off, notle_defconfig:2014; the driver registers none). Brightness is a
  driver attribute.

### 4.2 Panel sysfs

kobject `panel-notle-dpi` under the DSS overlay manager
(panel-notle-dpi.c:2218-2219), expected at
`/sys/devices/platform/omapdss/manager<N>/panel-notle-dpi/` (LCD2 manager).
**[device]** exact path.

| Attribute | Semantics |
|---|---|
| `enabled` | RW 0/1: power the panel on or off directly, bypassing fb blank (panel-notle-dpi.c:486-511) |
| `brightness` | RW. Accepts any value >= 0 (panel-notle-dpi.c:955-998). The user scale is non-linear: `range_expand` maps 0..223 roughly linearly and 224..255 steeply, so 255 becomes 765 internally (panel-notle-dpi.c:934-948). The effective value is min(scaled brightness, internal limit) (panel-notle-dpi.c:1515-1518), so at the default cap the usable range is **0..223**. 0 turns the LEDs off. Applied at once if enabled, else at next enable. |
| `brightness_multiplier` | RW, percent x100, default 10000; any value >= 0, **no upper bound**; scales brightness before the cap (panel-notle-dpi.c:1048-1063, 1515-1516, 2178). **It raises LED power: services must never raise it above the default (rule 6).** |
| `brightness_limit` | RW. Default is 255 internal (panel-notle-dpi.c:2177), which **reads back as 223** (`range_reduce`, panel-notle-dpi.c:1000-1002). Writes go through `range_expand` (panel-notle-dpi.c:1004-1042), so writing 255 would set the internal cap to 765, about 3x the default. **Services must never raise it (rule 6).** |

`limit_mw` (76 on current boards, board-notle.c:569-589) is a separate fixed
factor in the PWM calculation (panel-notle-dpi.c:1531-1533). It is not tied
to `brightness_limit`.
| `gamma1`..`gamma8`, `gamma_preset`, `colormix`, `mono`, `cpsel`, `forcer/g/b`, `testpattern`, `list_testpatterns` | image tuning and test; not for services |
| `reg_addr`, `reg_value`, `dump_regs`, `fpga_revision`, `reset`, `debug` | FPGA register access and diagnostics; not for services |

Attribute list: panel-notle-dpi.c:1245-1325.

### 4.3 Screen on and off

- fb blank: `FBIOBLANK` with `FB_BLANK_POWERDOWN` (or VSYNC/HSYNC suspend,
  NORMAL) calls the panel `suspend`; `FB_BLANK_UNBLANK` calls `resume` or
  `enable` (omapfb-main.c:1377-1441). Panel suspend powers the panel off and
  drops an L3 bus throughput request; resume restores it
  (panel-notle-dpi.c:2276-2340). The L3 request exists; it is not something
  services change.
- Framebuffers: omapfb with 3 fbs (notle_defconfig:2041-2043);
  `/dev/graphics/fb0` on Android. **[device]** which fb drives the panel.
- `CONFIG_DRM_OMAP=y` is also set (notle_defconfig:2671).

### 4.4 Android power interfaces

Config: `HAS_WAKELOCK`, `HAS_EARLYSUSPEND`, `WAKELOCK`, `WAKELOCK_STAT`,
`USER_WAKELOCK`, `EARLYSUSPEND`, `FB_EARLYSUSPEND` (notle_defconfig:581-588).

| Path | Semantics | Source |
|---|---|---|
| `/sys/power/state` | write `on` or `mem`. With early suspend, `mem` runs early-suspend handlers (screen off, touchpad gesture mode) then suspends when no wakelock is held; `on` runs late resume | kernel/power/main.c:272-316; suspend.c:43-48 |
| `/sys/power/wake_lock` | write `name` or `name timeout_ns` to hold a lock | kernel/power/main.c:437; userwakelock.c:43-75 |
| `/sys/power/wake_unlock` | write `name` | kernel/power/main.c:438 |
| `/proc/wakelocks` | statistics | kernel/power/wakelock.c:598 |
| `/sys/power/wait_for_fb_sleep`, `/sys/power/wait_for_fb_wake` | block until the fb should stop or resume drawing | kernel/power/fbearlysuspend.c:72-137 |
| `/sys/power/suspend_cycle_cnt` | RO, count of completed suspend cycles (Google addition) | kernel/power/main.c:320-338; suspend.c:34-37 |
| `/sys/power/wakeup_count` | standard | kernel/power/main.c:391 |

Early-suspend levels: blank screen 50, stop drawing 100, disable fb 150
(include/linux/earlysuspend.h:34-36). The touchpad (rmi) and LTR-506
handlers run at level 51 (rmi_driver.c:1131; ltr506als.c:2156).

Wake sources found in board code: touchpad IRQ, MPU IRQ, glasshub IRQ,
camera key, and the battery gauge SOC interrupt (board-notle.c:1851, 2104,
2139; glasshub.c:2363; gpio_keys wakeup flag below). The LTR-506 interrupt
is not a wake source on this board.

Voltages, clocks, thermal (`notle_pcb_sensor`, OMAP die governor, duty
cycle) and charging (`CHARGER_NOTLE`, bq27520 gauge) exist in this kernel.
They are outside the scope of services and must not be changed (rule 6).

## 5. Keys and other gesture-related board items

| Item | Detail | Source |
|---|---|---|
| Camera button | `gpio-keys` device, one button, `KEY_CAMERA` (212), GPIO 30 (`gpio_wk30`), active low, 5 ms debounce, wakeup=1 | board-notle.c:1250-1272; board-notle.h:90 |
| Camera key input device | name `gpio-keys` (platform device name), phys `gpio-keys/input0` | gpio_keys.c:722-723 |
| Power button | `twl6030_pwrbutton`, `KEY_POWER` (116), phys `twl6030_pwrbutton/input0` | twl6030-pwrbutton.c:118-125 |
| Viewfinder | `SW_CAMERA_LENS_COVER` on the touchpad device (1.5) | rmi_f11.c:1942-1958 |
| Status LEDs | `notleboard::status1` (heartbeat), `notleboard::status2` (mmc0), GPIO 7 and 8 via `leds-gpio` | board-notle.c:419-442 |
| Keychord, keyreset, uinput | `INPUT_KEYCHORD`, `INPUT_KEYRESET`, `INPUT_UINPUT` built in | notle_defconfig:1409, 1488, 1498 |

## 6. Open questions (need a device or an owner decision)

1. Touchpad: exact `maxPos`, report rate, and the physical direction of
   +X and +Y on the temple.
2. Touchpad: does `rmi_mouse` exist (controller `has_rel`)?
3. Event node numbering: `inputN` and `eventN` order is not fixed by
   source; match devices by name, never by number.
4. glasshub: value meanings of `don_doff` (on/off encoding) and `wink`,
   units of `activity_timeout`, and whether the stock firmware on retail
   units is at least 1.12.
5. Clock domain of `read_robust_clock()` (glasshub and IIO timestamps).
6. IMU: IIO device index, the exact buffer layout with the Google format
   byte, the physical meaning of the matrix axes, and a redistributable DMP
   image (without it `event_glu`, tap and orientation events are not
   available; raw gyro/accel/compass still work).
7. Panel sysfs path (`manager<N>`) and which framebuffer is the panel.
8. On a non-Android kernel (no early suspend) the touchpad screen-off
   gesture mode and the LTR-506 early-suspend hooks do not run; a
   plain-Linux build needs another trigger for them (owner decision).
