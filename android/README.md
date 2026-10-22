# glass-services on Android (route 1)

How `glassd` and its Android glue go into an AOSP build of Android 4.4 (Track A) or 6.0 (Track B) for Glass. AOSP 4.4 and 6.0 cannot build Rust, so the modules are **prebuilt** here and copied into the tree as one directory. The device tree only includes it; nothing in this directory is a Google file.

## The dist directory

`tools/android-dist.sh <out-dir>` (or the CI artifact `glass-services-android-dist` of the `android-dist` job) writes:

| Path | What |
|---|---|
| `bin/glassd` | the daemon, `armv7-linux-androideabi`, built with NDK r25c for API 19, so the same binary runs on 4.4 and 6.0 (stripped) |
| `app/GlassServicesGlue.apk` | the glue ([`glue/`](glue/)), signed with a throwaway key; AOSP signs it again with the platform key |
| `Android.mk` | the prebuilt modules `glassd` (`/system/bin`) and `GlassServicesGlue` (`/system/priv-app`, `LOCAL_CERTIFICATE := platform`) |
| `glass-services.mk` | the product makefile: `PRODUCT_PACKAGES`, and copies of the files below; stops the build with an error if the prebuilts are missing |
| `init.glassd.rc` | the init service (user `system`, groups `system input`, socket `glassd` 0666), `/data/glassd`, the touchpad `sleepmode` owner |
| `keylayout/twl6030_pwrbutton.kl` | power key as `BUTTON_MODE`, so Android never handles it (docs/api.md "Android glue") |
| `idc/sensor00fn11.idc` | the touchpad as a touch navigation device (no pointer on screen) |
| `sepolicy/` | `glassd` domain, socket and data types, `connectto` for the glue, launcher and provider (Android 6.0 only) |
| `SOURCE`, `SHA256SUMS` | the glass-services commit and toolchain, and the files' checksums |

The directory goes to `<aosp>/vendor/openmyglass/glass-services` (or anywhere, with `GLASS_SERVICES_DIR` set to it).

## Lines the device tree needs

`device.mk`:

```make
# OpenMyGlass glass-services: glassd and its Android glue (prebuilt, vendor/openmyglass/glass-services)
$(call inherit-product, vendor/openmyglass/glass-services/glass-services.mk)
```

`rootdir/init.omap4430.rc`, next to the other imports:

```
import /init.glassd.rc
```

`BoardConfig.mk`, Android 6.0 branch only (4.4 runs services without a domain of their own unconfined, and its policy build needs every file listed in `BOARD_SEPOLICY_UNION`):

```make
BOARD_SEPOLICY_DIRS += vendor/openmyglass/glass-services/sepolicy
```

Nothing else: the glasshub attributes are already owned by `system` in `init.omap4430.rc`; the touchpad's F11 `sleepmode` is handled in `init.glassd.rc`. If the device tree ever installs its own `twl6030_pwrbutton.kl` or `sensor00fn11.idc`, the two copies conflict and one must go.

## What runs

- `glassd` starts in `class main` as `system`. It takes the socket that init created (`ANDROID_SOCKET_glassd`) and checks each peer's uid: root and system, and the packages `org.openmyglass.launcher` (glass-apps) and `org.openmyglass.glass` (the OpenMyGlass Glass app, the companion link provider), looked up in `/data/system/packages.list`. It logs to `logcat` (tag `glassd`). Defaults on Android: `--display log` (the framework blanks the panel), `--head off` (the sensors HAL owns the IMU), `--screen-off-input encoded`.
- The glue `org.openmyglass.services.glue` runs as `android.uid.system` with `android:persistent`, so the framework starts it at boot and restarts it. It maps `ScreenState` to `PowerManager`, injects key events for regular apps while it holds the input focus, and shuts down on a long power-key press (docs/api.md "Android glue"). Log tag `glass-glue`.

## Tests without a device

- `cargo test --workspace` (host), as before.
- The glue's logic is plain Java without Android imports, except `GlueApplication`: `test/.../GlueTests.java` runs on a desktop JVM (17 or newer, for Unix sockets), including an end-to-end test against `glassd --mock`:

  ```sh
  cargo build -p glassd
  out=$(mktemp -d)
  javac -d "$out" $(find android/glue/src android/glue/test -name '*.java' ! -name GlueApplication.java)
  java -cp "$out" org.openmyglass.services.glue.GlueTests target/debug/glassd
  ```
- CI's `android-dist` job builds the dist directory with NDK r25c and checks the binary's dynamic dependencies (API 19 libraries only) and the APK's manifest.

## Device checklist (not run: no Glass available)

- [ ] `glassd` starts at boot on 4.4 and 6.0 (`logcat -s glassd`), with the socket from init, and finds the touchpad, buttons and glasshub as `system` with group `input`.
- [ ] The launcher and the OpenMyGlass Glass app connect; another app (`adb shell` as `shell`, uid 2000) is refused with `ERROR_CODE_DENIED`.
- [ ] Screen: a tap wakes, the timeout dims then turns the screen off (`dumpsys power`: the glue's wake locks follow glassd), a two-finger swipe down sleeps, and the framework never turns the screen off by itself while glassd says on.
- [ ] An alarm or notification that wakes the screen through a wake lock turns glassd's screen on too (`ACTION_WAKE` from the glue).
- [ ] The power key's short press toggles off-head and does not sleep the screen through Android; a long press shuts down.
- [ ] With a regular app in front (for example Settings), tap, swipes and swipe down move and select as `DPAD_CENTER`, `DPAD_RIGHT`/`LEFT` and `BACK`; with the launcher in front no keys are injected.
- [ ] No pointer is shown for the touchpad (`sensor00fn11.idc`).
- [ ] Android 6.0: no SELinux denials for `glassd` (`dmesg | grep avc`); tighten the sysfs rules to the glasshub and touchpad labels.
- [ ] Dictation through the OpenMyGlass Glass app once it registers as the companion link provider (it does not yet: that change belongs to the OpenMyGlass repository).
