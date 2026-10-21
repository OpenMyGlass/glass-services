# Touchpad gestures (stock XE24 behaviour)

Clean-room specification. Written by the reverse-engineering agent from the
XE24 factory image (`glass_1-img-5585826`, build `XRH35`); no stock code,
disassembly or decompiled text is reproduced here. Facts from the GPL kernel
are cited by file and line in `OpenMyGlass/kernel-omap`, branch
`glass-omap-xrx13b-stable-3.4.113`. See `README.md` for the certainty legend.

Legend: **[C]** certain (directly observed), **[I]** inferred, **[?]** open.

## 1. Where the logic lives

| Component | Role | Observed in |
|---|---|---|
| Kernel `rmi_f11` (Synaptics RMI4, function 11) | Real touchpad. Multi-touch protocol A. While the screen is off (early suspend) it also synthesizes *encoded* gesture events | `drivers/input/rmi4/rmi_f11.c` |
| GDK gesture recognizer (`com.google.android.glass.touchpad.GestureDetector` and its state classes) | Turns Android `MotionEvent`s from the touchpad source into gestures and scroll callbacks, in each app's process | `/system/framework/glass-core.jar` |
| System input filter (`com.google.android.glass.server.InputHub`) | Installed as the WindowManager input filter; handles screen-off gestures, power/camera keys, the camera and hinge switches, and two system-wide screen-on gestures | `/system/framework/glass-services.jar` |
| Touchpad interception service (`TouchpadService`, binder name `touchpad_manager`) | Lets one privileged client consume a screen-off TAP before it wakes the device | `glass-services.jar` |
| `TouchpadHardware` | Puts the touchpad to sleep and wakes it through sysfs | `glass-services.jar` |
| `libglassinputdevice.so` | **Not a gesture classifier.** A small `uinput` helper that creates a virtual touchpad so the phone companion (screencast / remote control) can inject touches | `/system/lib/libglassinputdevice.so`, caller `com.google.glass.util.GlassInputDevice` in `GlassBluetooth.apk` |

There is no native gesture classifier for the touchpad: classification is
Java code over `MotionEvent`s, plus a small kernel-side tap/flick encoder used
only while the screen is off. **[C]**

## 2. Input devices

### 2.1 Real touchpad **[C]**

- Driver: RMI4 over I2C, address `0x20` (`board-notle.c:1596-1601`).
- evdev name: `sensor00fn11` (the driver builds it as `"sensor00" + "fn%02x"`,
  `rmi_f11.c:2974`). The IDC file `/system/usr/idc/sensor00fn11.idc` marks it
  `device.internal = 1` and `touch.deviceType = touchPad`, so Android reports
  it as `InputDevice.SOURCE_TOUCHPAD` (`0x00100008`). Two more IDC files with
  the same content exist for other parts or tests: `tsc3060.idc` and
  `simulated_touchpad.idc`.
- Multi-touch **protocol A** (`board-notle.c:1475-1476`, `.type_a = 1`): per
  finger `ABS_MT_TRACKING_ID` (range 1..10), `ABS_MT_TOOL_TYPE`,
  `ABS_MT_PRESSURE` (0..255), `ABS_MT_TOUCH_MAJOR`, `ABS_MT_TOUCH_MINOR`,
  `ABS_MT_ORIENTATION`, `ABS_MT_POSITION_X`, `ABS_MT_POSITION_Y`, then
  `SYN_MT_REPORT`; a frame ends with `SYN_REPORT` (`rmi_f11.c:1599-1621`,
  `2699-2722`). An empty `SYN_MT_REPORT` + `SYN_REPORT` means all fingers up.
- Axis alignment from board data: `swap_axes = false`, `flip_x = false`,
  `flip_y = true` (`board-notle.c:1453-1457`).
- The stock user space opens no `/dev/input` node itself and issues no ioctl
  on the real touchpad: all reading goes through Android's InputReader. **[C]**
  (no `/dev/input` string or `EVIOC*` use in any of the four studied
  libraries).

### 2.2 Sysfs used by user space **[C]**

`TouchpadHardware` (glass-services.jar) on a `glass_1` product
(`Build.PRODUCT`):

| Path | Write | When |
|---|---|---|
| `/sys/bus/rmi/devices/touchpad.fn01/sleepmode` | ASCII `1` to sleep, `0` to wake (single byte, no newline) | off head (sleep) / on head (wake) |
| `/sys/bus/rmi/devices/touchpad.fn11/sleepmode` | same | same, written right after fn01 |

Both writes run on one background thread, in that order. The F11 attribute
accepts `0` or `1`; entering sleep terminates the current touch sequence and
emits an empty MT frame (`rmi_f11.c:3448-3505`).

On other products (a later Glass model, not XE) it uses
`/sys/bus/ttsp5/devices/main_ttsp_core.cyttsp5_touch/{sleep,wake,sleep_status}`
(write `0` to `sleep` or `wake`). Not relevant for XE but the names are listed
so a port does not trip over them. **[C]**

### 2.3 Virtual touchpad created by `libglassinputdevice.so` **[C]**

Used only by the companion screencast/remote-control path (§7).

- Opens `/dev/uinput` with `O_WRONLY | O_NONBLOCK` (flags `0x801`).
- `UI_SET_EVBIT`: `EV_SYN`, `EV_ABS`.
- `UI_SET_ABSBIT`: `ABS_MT_TRACKING_ID`, `ABS_MT_POSITION_X`,
  `ABS_MT_POSITION_Y`, `ABS_MT_TOUCH_MAJOR`, `ABS_MT_TOUCH_MINOR`,
  `ABS_MT_PRESSURE`. (No key bits, no `BTN_TOUCH`, no `INPUT_PROP_*`.)
- Writes a legacy `struct uinput_user_dev`: name = the string passed from
  Java, truncated to 79 bytes; `bustype = BUS_VIRTUAL (6)`, vendor 1,
  product 1, version 1; `absmax`: POSITION_X 1600, POSITION_Y 200,
  TOUCH_MAJOR 200, TOUCH_MINOR 200, TRACKING_ID 10, PRESSURE 255; all
  `absmin` 0. Then `UI_DEV_CREATE`.
- Device name used by the only caller: **`virtual_touchpad`**
  (`GlassInputDevice.VIRTUAL_DEVICE_NAME`), matching
  `/system/usr/idc/virtual_touchpad.idc` (touchPad, internal).
- Inject call (`injectTouchInternal(handle, x[], y[], major[], minor[],
  pressure[])`): for each pointer *i* (protocol A) writes
  `ABS_MT_TRACKING_ID = i`, `POSITION_X`, `POSITION_Y`, `TOUCH_MAJOR`,
  `TOUCH_MINOR`, `PRESSURE`, then `SYN_MT_REPORT`; after all pointers one
  `SYN_REPORT`. Event timestamps come from `gettimeofday`.
- Finish call: one empty `SYN_MT_REPORT` followed by `SYN_REPORT` (all up).
- Destroy: `UI_DEV_DESTROY` and close. **[I]** (the destroy symbol is present;
  its ioctl was not traced in detail).
- JNI symbols (static-name binding, no `RegisterNatives`):
  `Java_com_google_glass_util_GlassInputDevice_create(String) -> int`,
  `..._injectTouchInternal(int, int[], int[], int[], int[], int[])`,
  `..._finishTouchInternal(int)`, `..._destroy(int)`. The returned `int` is
  the uinput file descriptor (or -1).

## 3. Coordinates and direction conventions

- The recognizer works in Android `MotionEvent` coordinates of the touchpad
  source (raw device units after InputReader scaling). **[C]**
- "Forward" is **+X**: the stock recognizer treats `SWIPE_RIGHT` and
  `TWO_SWIPE_RIGHT` as forward, `SWIPE_LEFT` / `TWO_SWIPE_LEFT` as backward,
  and a scroll displacement `>= 0` as forward
  (`GestureDetector.isForward`). **[C]** Physically, on the worn device,
  forward = finger moving from the hinge toward the front (eye side). **[I]**
  (consistent with the GDK documentation and the `flip_y` only board setup).
- `+Y` is "down" (`SWIPE_DOWN` when the Y distance is positive). **[C]**
- Companion injection maps the phone's percentage coordinates as
  `x_dev = Xmax - x% * Xmax / 100` (X mirrored) and
  `y_dev = y% * Ymax / 100`, where `Xmax`/`Ymax` are the motion ranges of the
  first input device whose sources equal `SOURCE_TOUCHPAD`; pressure is
  `p * 255`, major and minor are fixed at 2 (`EventInjector` in
  GlassBluetooth.apk). **[C]**

## 4. The gesture recognizer (screen on, in apps)

### 4.1 Gesture vocabulary **[C]**

Enum `com.google.android.glass.touchpad.Gesture`, in this order (the ordinal
is the wire value of `ITouchpadCallback.onDetected(int)`):

| Ordinal | Name |
|---|---|
| 0 | TAP |
| 1 | TWO_TAP |
| 2 | THREE_TAP |
| 3 | LONG_PRESS |
| 4 | TWO_LONG_PRESS |
| 5 | THREE_LONG_PRESS |
| 6 | SWIPE_DOWN |
| 7 | SWIPE_LEFT |
| 8 | SWIPE_RIGHT |
| 9 | SWIPE_UP |
| 10 | TWO_SWIPE_DOWN |
| 11 | TWO_SWIPE_LEFT |
| 12 | TWO_SWIPE_RIGHT |
| 13 | TWO_SWIPE_UP |

There is no THREE_SWIPE gesture: three-finger movement only produces scroll
callbacks.

Listener callbacks exposed by the GDK detector (public GDK API names):
`onGesture(Gesture)`, `onFingerCountChanged(previous, current)`,
`onScroll(displacement, delta, velocity)`, `onVerticalScroll(...)`,
`onTwoFingerScroll(...)`, `onOneFingerScroll(...)`, `onGenericScroll(...)`.
Every callback returns "handled"; events are reported as consumed if any
callback handled the current event or the sequence.

### 4.2 Parameters **[C]**

| Parameter | Value | Source |
|---|---|---|
| Touch slop (tap radius) | `ViewConfiguration.getScaledTouchSlop()` (AOSP default 8 dp; Glass density not checked **[?]**) | `Utils` |
| Expanded tap radius (three fingers) | 2 x touch slop | `Utils` |
| Distance test | per pointer, squared Euclidean distance of the integer-truncated X/Y delta against slop², every pointer of the current event compared with the same pointer id in the reference event | `Utils` |
| Long-press timeout | `ViewConfiguration.getLongPressTimeout()` (AOSP default 500 ms) | `Utils` |
| Forward release velocity | +0.7 px/ms (X velocity must exceed it) | `Utils` |
| Backward release velocity | -1.0 px/ms (X velocity must be below it) | `Utils` |
| Max angle for a horizontal swipe | 25 deg from the X axis when moving backward (dx < 0); 18 deg when moving forward (dx > 0) | `Utils` |
| Velocity smoothing | first-order low-pass with time constant RC = 15.915494 ms (`1/(2*pi*0.01)`): `alpha = dt/(RC+dt)`, `v = (1-alpha)*v_prev + alpha*v_raw`; no smoothing while `|v_prev| < 0.001` | `Utils` |
| Two-finger swipe release window | both fingers must lift within 100 ms of each other | `TwoScrollState` |
| Multi-pointer distance | mean of per-pointer deltas over the pointer ids present in both events | `Utils` |

### 4.3 State machine (described in our own words) **[C]**

Only events whose source is `SOURCE_TOUCHPAD` are considered, and only from the
device that produced the current `ACTION_DOWN` (events from another device id
are ignored until that sequence ends).

States: *Idle*, *One finger*, *Two fingers*, *Three fingers*, *One-finger
scroll*, *Two-finger scroll*, *Three-finger scroll*.

- **Idle**: a DOWN goes to *One finger*. CANCEL anywhere returns to *Idle*.
- **One finger**: on entry, arm a long-press timer. If it fires, report
  LONG_PRESS (if a listener handles it, the machine returns to *Idle* and the
  rest of the sequence produces nothing). UP: if no long press fired and the
  finger is still within slop of the last DOWN, report TAP; go to *Idle*.
  MOVE beyond slop: *One-finger scroll*. Second finger down: *Two fingers*.
- **Two fingers**: arm the long-press timer for TWO_LONG_PRESS. UP (last
  finger) within slop and no long press: TWO_TAP. MOVE beyond slop: if two
  pointers remain, *Two-finger scroll*; if one, *One-finger scroll*;
  otherwise *Idle*. A third finger: *Three fingers*.
- **Three fingers**: timer for THREE_LONG_PRESS; UP within slop: THREE_TAP;
  MOVE beyond 2 x slop or a fourth finger: *Idle* (nothing reported).
- **Scroll states** (common behaviour): on entry, the direction is fixed from
  the move that left the tap area: horizontal if `|dx| > |dy|`, vertical if
  `|dy| > |dx|`, none if equal; sign gives LEFT/RIGHT or UP/DOWN. During the
  scroll, displacement is measured from the latest reference (the last
  pointer-down or pointer-up, whichever is newer) along the fixed axis; in the
  first session (no finger added or removed yet) the displacement is reduced
  by `sign * slop` so it starts near zero. A horizontal direction fires
  `onScroll(displacement, delta, vx)`; a vertical one fires
  `onVerticalScroll(displacement, delta, vy)`. Callbacks only fire when delta
  is non-zero. Two- and three-finger scrolls only report horizontal motion.
- **One-finger scroll, on UP**: classify the swipe from the scroll's entry
  event to the UP event using the dx/dy and the smoothed X velocity:
  - `dx == 0`: DOWN if dy > 0, UP if dy < 0.
  - Horizontal candidate when `|dy/dx|` is within the angle limit for the
    sign of dx (25 deg backward, 18 deg forward). Then RIGHT if
    `vx > 0.7 px/ms`, LEFT if `vx < -1.0 px/ms`, else *no gesture*.
  - Otherwise vertical: DOWN if dy > 0, UP if dy < 0.
  - The new classification replaces the scroll direction except that a
    scroll which started horizontal is never turned into a vertical swipe.
  - Report SWIPE_DOWN / SWIPE_LEFT / SWIPE_RIGHT / SWIPE_UP accordingly.
  - A second finger during the scroll: *Two-finger scroll*, keeping direction.
- **Two-finger scroll**: when the first finger lifts, classify its direction
  from the entry event (dominant axis); it must equal the scroll direction,
  else no gesture. When the last finger lifts within 100 ms of the first,
  report TWO_SWIPE_{DOWN,LEFT,RIGHT,UP}. A third finger: *Three-finger
  scroll*.
- **Three-finger scroll**: horizontal scroll callbacks only; UP returns to
  *Idle* without a gesture; a fourth finger ends the sequence.
- `onFingerCountChanged` fires on every DOWN / POINTER_DOWN
  (count-1 -> count), POINTER_UP / UP (count -> count-1) and CANCEL (-> 0).

Note: swipe *velocity*, not distance, decides LEFT/RIGHT. A slow horizontal
drag produces scroll callbacks and no swipe.

## 5. System-wide handling in the input filter (glass-services.jar `InputHub`)

### 5.1 Screen on **[C]**

The filter runs its own recognizer on touchpad events and consumes only:

| Gesture | Action |
|---|---|
| TWO_SWIPE_DOWN | play sound effect 15, then turn the screen off (`PowerManager.goToSleep`) |
| THREE_LONG_PRESS | only on non-`user` builds, and only if set up and unlocked: send ordered broadcast `android.intent.action.BUG_REPORT` |

When it consumes a gesture it injects an `ACTION_CANCEL` for the in-flight
sequence so apps see the sequence end. Any other touchpad event goes to apps,
with one exception: when the screen is on and the event carries policy flag
`0x20000000` (AOSP "bright here"), the filter calls `userActivity` and clears
flag `0x40000000` (AOSP "pass to user") before re-injecting it, so the touch
that brightens a dimmed screen is not delivered to apps. **[C]** for the flag
operations, **[I]** for the AOSP flag names.

### 5.2 Screen off (XE path, also forced with `glass.usersim=true`) **[C]**

While the screen is off the kernel driver (early suspend) suppresses normal
touches and instead emits *encoded gesture frames*
(`rmi_f11.c:1756-1790`, `1960-2090`; see `kernel-interfaces.md` §1.5): one
pointer at
`(100 + ox, 100 + oy)`, `ABS_MT_PRESSURE = 1`, `TOUCH_MAJOR = TOUCH_MINOR = 15`,
followed by a 250 ms wake lock. Offsets: single tap (0, 0), double tap
(-1, -1), press (1, 1), flick (x_flick_dist, y_flick_dist). The driver
also synthesizes a tap itself when all samples stay in a 90 x 90 box, more
than one sample, one finger, and the contact lasts more than 5 and less than
250 ms (`rmi_f11.c:93-97`, `1852-1857`). Rate limits: the press frame is sent
at most once per touch sequence; the driver-synthesized tap only exists when
the F11 `gesture` attribute is 1 and is sent at most once per suspend cycle
(`rmi_f11.c:1842`, `2089-2119`); controller taps, double taps and flicks are
not limited.

User space recognises such a frame when the event has exactly one pointer,
normalized pressure `<= 0.01` and touch major and minor `>= 14.8`. It then
decodes `dx = (int)(x - 100)`, `dy = (int)(y - 100)`:

- (0, 0) or (-1, -1): TAP
- (1, 1): LONG_PRESS
- `|dx| >= 6` and `|dx| > |dy|`: SWIPE_RIGHT if dx > 0, else SWIPE_LEFT
- anything else: no gesture.

Decoding happens only for DOWN, MOVE or POINTER_DOWN while the screen is off;
the touch sequence is then cancelled. What happens with the decoded gesture
is in `wake-policy.md` §3 (wake on TAP, SWIPE_LEFT, SWIPE_RIGHT; TAP may be
intercepted first by a `touchpad_manager` client).

On non-XE products any touchpad UP while the screen is off counts as TAP.

### 5.3 Keys and switches seen by the same filter **[C]**

| Event | Code | Handling |
|---|---|---|
| Key | Android 24 / 25 (VOLUME_UP / DOWN); XE has no volume buttons, so these only come from external keyboards or Bluetooth devices (Linux `KEY_VOLUMEUP` 115 / `KEY_VOLUMEDOWN` 114 via the stock `Generic.kl`) **[I]** | on release: adjust stream 1 (`STREAM_SYSTEM`) by +1/-1 with flag 4 (`FLAG_PLAY_SOUND`) |
| Key | Android 27 (`KEYCODE_CAMERA`), from Linux `KEY_CAMERA` (212) on the `gpio-keys` device (`board-notle.c:1250-1254`), mapped by `/system/usr/keylayout/keys.kl` | press/long-press detector (long = `getLongPressTimeout`), see `wake-policy.md` |
| Key | Android 110 (`KEYCODE_BUTTON_MODE`; the filter calls it its power key), from Linux `KEY_POWER` (116) on `twl6030_pwrbutton`, mapped by `keys.kl` (`key 116 BUTTON_MODE`); both `gpio-keys.idc` and `twl6030_pwrbutton.idc` select `keyboard.layout = keys`. So the power button never reaches Android as `KEYCODE_POWER` (26) | short press: manual on/off-head override; long press: shutdown |
| Switch | code 0 (camera switch / lens cover) | camera preload when lab property `persist.lab.chvr` is true |
| Switch | code 1 (hinge) | closed = forced off head; opened = timed forced on head |

The F11 driver reports a "viewfinder" contact as `SW_CAMERA_LENS_COVER`
(`rmi_f11.c` view-finder block). The mapping from Linux switch codes to the
filter's codes 0 and 1 happens in the Glass framework (`SwitchEvent`) and was
not traced. **[?]**

## 6. `touchpad_manager` interception service **[C]**

- Binder service name `touchpad_manager`, interface descriptor
  `com.google.android.glass.touchpad.ITouchpadManager`:
  - code 1 `register(ITouchpadCallback)`
  - code 2 `unregister(ITouchpadCallback)`
  - both require permission `com.google.android.glass.permission.TOUCHPAD_GESTURES`.
- Callback descriptor `com.google.android.glass.touchpad.ITouchpadCallback`,
  code 1 `onDetected(int gestureOrdinal)`, two-way call.
- Dispatch: for a screen-off gesture the service tries the registered
  callbacks in order and stops at the **first one that does not throw**; that
  delivery counts as "handled" and suppresses the wake-up.
- Only TAP is ever offered, and only when the device is set up and not locked.

## 7. Companion remote touches (for the phone protocol)

`EventInjector` in GlassBluetooth.apk receives `MotionEvent` protos from the
phone during screencast and injects them through the `virtual_touchpad`
uinput device (§2.3). It rejects events while the screen is off, creates the
uinput device lazily, and calls *finish* when the proto action's low byte is
1 (`ACTION_UP`). It destroys the device when screencast ends. **[C]**

## 8. Notes for a compatible replacement

1. Read `sensor00fn11` as MT protocol A; build frames on `SYN_MT_REPORT` and
   flush on `SYN_REPORT`; treat an empty frame as all-up.
2. Implement §4.3 with the §4.2 numbers. Use 500 ms long press and the
   touch-slop equivalent in device units (measure on hardware **[?]**).
3. While the display is off, decode encoded frames as in §5.2 instead of
   running the normal recognizer.
4. Use `sleepmode` on fn01 and fn11 for off-head power saving.
5. For phone remote control, a uinput device named `virtual_touchpad` with
   the §2.3 ranges keeps the stock IDC file valid.

## 9. Open questions (need a device)

- Real `ABS_MT_POSITION_X/Y` maxima of `sensor00fn11` and the dp density, to
  turn slop and velocities into device units. **[?]**
- Exact Linux switch code to filter code mapping for the hinge and camera
  switch. **[?]**
- (Answered) `ABS_MT_TOOL_TYPE` is always declared; its range is
  `0..MT_TOOL_FINGER` unless the controller reports pen support
  (`rmi_f11.c:2716-2722`).
