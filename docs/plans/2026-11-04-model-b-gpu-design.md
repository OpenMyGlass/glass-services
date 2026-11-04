# Model B GPU design: PowerVR SGX540 on Alpine/musl, kernel 3.4.113

Status: design, 2026-11-04. Nothing here has run on a device. It extends
[`model-b-gpu.md`](model-b-gpu.md) and changes that document's loader decision (section 3).

Evidence tags used on every claim:

- **[V]** verified from a source I read: a repo file, a public page, an ELF header or symbol table, a kernel log.
- **[R]** reported by project notes; I found no primary log.
- **[I]** inferred. Reasoned from verified facts, not tested.
- **[E]** estimate. A number of mine, not a measurement.

## 1. Decision

Build the launcher's GPU variant as a **plain bionic executable** and run it under the stock
Android linker on Alpine, next to the musl system. No libhybris. Keep the static musl launcher,
unchanged, as the software fallback.

| Question | Answer |
|---|---|
| Can we recompile the user space or shader compiler? | **No.** No public, legitimately released source was found for any DDK version. Only kernel modules are open. |
| Is there a usable alternative binary set? | Not on 3.4. TI's glibc SGX540 build exists and is redistributable, but it requires omapdrm with a plugin API our kernel lacks, and it ships no fbdev window system. |
| Best route for Model B | Route C: bionic launcher + stock blobs + a small framebuffer window shim. |
| What does the UI gain? | Smoother swipes. Nothing on a still screen, no meaningful battery gain. Do it after deep sleep. |

## 2. Sources: what exists to recompile

### 2.1 Verdict

- **Kernel module: open and rebuildable.** Already done for our exact DDK [V: `glass-kernel/docs/gpu-module.md`].
- **User space, GLSL compiler, USC, EGL core, EDM microkernel: closed at every version.** I found
  no public source. The only user-space sources mentioned anywhere are leaks (section 2.3).
- **Open replacement: does not exist for SGX540.** The nearest projects are research-grade.
- **Mesa:** the PowerVR driver covers Rogue and newer, not SGX [V: Mesa source tree docs, search
  summary; the Phoronix article itself returned 403].

### 2.2 Inventory

"3.4 module?" means: can a matching kernel module be built for our 3.4.113 with its
omapfb + dsscomp display stack?

| Source | Kind and licence | Libc / ABI | DDK; SGX540 rev | 3.4 module? |
|---|---|---|---|---|
| Stock XE24 `/vendor` blobs (owner's image) | Binary, Google/IMG; extraction only | Bionic, softfp, ARMv7 [V: readelf, no VFP-args tag] | 1.9@2166536; rev 120 [V] | **Yes, built and loading** [V: `gpu-module.md`; hardware validation 2026-11-01] |
| [KFire-Android/android_vendor_amazon_otter](https://github.com/KFire-Android/android_vendor_amazon_otter) `eurasia_km` | Source, kernel only; MIT or GPL-2 [V: README] | n/a | 1.9@2166536; rev 120 [V: `pvrversion.h`] | **Yes**, this is what we build |
| [andi34/android_omap4_pvr-source](https://github.com/andi34/android_omap4_pvr-source) | Source, kernel only; MIT/GPL-2 files [V] | n/a | `ddk-1.9`: 1.9@2291151. `ddk-1.14`: 1.14@4081762 [V: `pvrversion.h`] | 1.9 likely [I]. No matching user space on the device, so no gain |
| [TI PPA `pvr-omap4` 1.9.0.8.1.1](https://launchpad.net/~tiomap-dev/+archive/ubuntu/omap-trunk), mirror [maemo-leste/pvr-omap4](https://github.com/maemo-leste/pvr-omap4) | Binary; TI licence allows unmodified binary redistribution, **forbids reverse engineering** [V: `copyright`, `license.txt`] | **glibc armhf** (the libraries checked need only GLIBC_2.4) [V: readelf] | 1.9@2253347; rev 120 [V: `libIMGegl` string, DKMS `pvrversion.h`, Makefile] | **Not as-is**: see 2.4 |
| [TI PPA `pvr-omap4-dkms` 1.9.0.8.1.1](https://ppa.launchpadcontent.net/tiomap-dev/omap-trunk/ubuntu/pool/main/p/pvr-omap4-dkms/), [tmlind/pvr-omap4-dkms](https://github.com/tmlind/pvr-omap4-dkms) (v5.4, v5.5 branches), [robtaylor](https://github.com/robtaylor/pvr-omap4-dkms), [Cosby5](https://github.com/Cosby5/pvr-omap4-dkms) | Source, kernel only; MIT/GPL-2 [V: `MIT-COPYING`, `GPL-COPYING` present] | n/a | 1.9@2253347 `SGX_DDK_Linux_CustomerTI` [V] | Builds as an omapdrm plugin only, see 2.4 |
| Ubuntu precise [`pvr-omap4`](https://launchpad.net/ubuntu/+source/pvr-omap4) 1.7.10.0.1.21 ([bug 959924](https://bugs.launchpad.net/ubuntu/+bug/959924)) | Binary, multiverse, "redistributable" | glibc armhf [V] | 1.7 [V: version] | Older than the 1.9 line; no reason to prefer it [I] |
| [TI Graphics SDK](https://software-dl.ti.com/dsps/dsps_public_sw/sdo_sb/targetcontent/gfxsdk/) | Binary installer plus kernel source [I] | glibc; latest 5.01.01.02 has a hardfp build [V: download page] | [Maemo wiki](https://leste.maemo.org/Motorola_Droid_4/PowerVR) says 4.04.00.04 is the last for SGX540 rev 1.2.0, and that page is marked out of date. That release's page returned 404 at the path I tried [V] | **Not established.** Installer is an executable; not downloaded or run. Likely 2.6/3.0-era kernels [I] |
| [TI omap5-sgx-ddk-um-linux](https://git.ti.com/cgit/graphics/omap5-sgx-ddk-um-linux), [rcn-ee mirror](https://github.com/rcn-ee/ti-omap5-sgx-ddk-um-linux); meta-ti `ti-sgx-ddk-um` | Binary | glibc | 1.17 targets: ti335x, ti437x, ti572x, ti654x. 1.14: jacinto6evm, ti335x, ti437x. 1.9: OMAP5/DRA7 [V: tree listings, README] | **No SGX540 target**; wrong core |
| [maemo-leste/sgx-ddk-um](https://github.com/maemo-leste/sgx-ddk-um), [MightyM17 fork](https://github.com/MightyM17/sgx-ddk-um) | Binary; **no licence stated and provenance unclear**: has `ti443x`, which is absent from TI's public branches above [V], changelog says "pull latest blobs" | glibc armhf, needs GLIBC_2.34; DRM render node [V: readelf and strings of one library] | 1.17.4948957; `ti443x` [V] | **No** on 3.4 [I]. Relevant to mainline: 2.5 |
| [mobiaqua/sgx-pvr5-module](https://github.com/mobiaqua/sgx-pvr5-module), [TI omap5-sgx-ddk-linux](https://git.ti.com/cgit/graphics/omap5-sgx-ddk-linux) | Source, kernel only; MIT/GPL-2 [V] | n/a | 1.17.4948957; `ti443x_linux`: SGX 540 rev 120, DRM [V: Makefile] | No, tracks modern kernels (recent commit: "fixed compilation for kernel >= 6.8") [V]; a 3.4 port is not reasonable [I] |
| [openpvrsgx linux_openpvrsgx](https://github.com/openpvrsgx-devgroup/linux_openpvrsgx) | Source, kernel only, GPL; explicitly "NOT about reverse engineering or user-space" [V] | n/a | [Several 1.9 to 1.17](https://github.com/openpvrsgx-devgroup/linux_openpvrsgx/wiki/DDK-versions), OMAP4 listed [V] | Targets mainline, not 3.4 [I] |
| [pvrports](https://gitlab.com/pvrports/pvrports) (unofficial pmaports, active 2026-03) | Packaging, depends on the 1.17 blobs above | **musl via `gcompat`** + `libc6-compat` [V: APKBUILD] | 1.17; `ti443x` subpackage [V] | No, `letux-pvrsrvkm-5.17-rc2` [V] |
| [freemangordon/mesa `mesa-pvr-ti`](https://github.com/freemangordon/mesa/tree/mesa-pvr-ti), [mobiaqua/mesa-sgx](https://github.com/mobiaqua/mesa-sgx) | Mesa forks, MIT | glibc | 1.17 [V: commit text] | Mainline shims. `src/gallium/drivers/sgx` holds only a `meson.build` [V], so **no open compiler** there; they front the closed blobs [I] |
| [Garnet/sgx540-reversing](https://codeberg.org/Garnet/sgx540-reversing), [Hazematman CI20 fork](https://github.com/Hazematman/sgx540-reversing) | Research; no licence stated; claims only officially published inputs [V] | Tools | SGX540 | Partial ISA and assembler. Unchecked: full encoding, PDS, MOE, microkernel, any rendering without the vendor libGL. Last visible commit 2022-10 [V] |
| [SGX535-reMESA](https://github.com/reMESA-devgroup/SGX535-reMESA) (MIT), [SGX544-reMESA](https://github.com/gabrieelgama/SGX544-reMESA), [sgxhumation](https://github.com/adn770/sgxhumation) (SGX545), [usse-decoder-gen](https://github.com/VelocityRa/usse-decoder-gen) (SGX543) | Open research | x86 / various | **Not SGX540** | SGX535 reaches glxgears at about 16 fps, "isn't a finished OpenGL driver". SGX544 and SGX545 have no driver. reMESA cites "historical DDK material" as an input, so check its provenance before reuse [V: READMEs] |
| [CI20_PVR-prebuilts](https://github.com/MIPS/CI20_PVR-prebuilts) | Binary | **MIPS** | 1.14/1.15 [V: thread](https://groups.google.com/g/mips-creator-ci20-dev/c/XNBWlAb8DX8) | Wrong ISA |
| Replicant, LibrePlanet | Notes | n/a | n/a | [LibrePlanet](https://libreplanet.org/wiki/Group:PowerVR_drivers): no free 3D driver (search summary) [V]. Replicant's own wiki returned a bot block, so I could not read its conclusions |
| Sailfish/Mer | Notes | glibc + libhybris | n/a | [Maguro wiki](https://wiki.merproject.org/wiki/Maguro_Info) says sources "TBD" [V: search summary]. [meta-samsung](https://layers.openembedded.org/layerindex/branch/pyro/layer/meta-samsung) has `pvrsrvkm-sgx540-module`, `android-system-image-maguro` and a libhybris bbappend [V], all glibc [I] |

### 2.3 Leaks: not opened

Two pointers appear in legitimate pages. I did not open either target and they are not inputs to
this design: an LG Android SGX540 DDK source mentioned on the Maemo Leste wiki (no link given), and
a 2014 list post titled "PVR SGX graphics driver leaked". The openpvrsgx maintainer treats leaked
source as off-limits [V: CI20 thread].

### 2.4 Why TI's 1.9@2253347 does not fit on 3.4

This is the only legally clean SGX540 glibc set, so it deserves the detail:

- `libsrv_um` opens services through `drmOpen` and the name `omapdrm`, and carries no `/dev/pvrsrvkm` path [V: strings].
- Window system plugins are `libpvrws_KMS`, `libpvrws_WAYLAND`, `libpvrws_OMAPDRI2` [V]. **No fbdev or null plugin.** `libIMGegl` also needs libwayland-server, libdrm and libdrm_omap [V: NEEDED].
- The DKMS module builds with `SUPPORT_DRI_DRM := 1` and `PVRSRV_MODNAME := omapdrm_pvr`, and registers through `omap_drm_register_plugin` [V: Makefile, `pvr_drm.c`].
- Our kernel has `CONFIG_DRM_OMAP=y` but `# CONFIG_DRM_OMAP_DISPLAY is not set`, and `omap_drm_register_plugin` is **absent** from `System.map` [V].
- Making this work means porting the plugin API, moving display from omapfb/dsscomp to omapdrm, and adding libgbm plus KMS. Tony Lindgren's closest result, on Droid 4 (also OMAP4 SGX540) with 1.9.2253347 on v5.6, needed omapdrm hacks and he called it "somewhat usable" [V: [list post](https://lists.dyne.org/lurker/message/20200314.184317.ae6d5d92.en.html)].

Its ABI is the attraction: armhf, no bionic. That does not outweigh a display stack rewrite on 3.4.

### 2.5 What this means for mainline later

The maintained path on mainline is linux_openpvrsgx or sgx-pvr5-module, the 1.17 `ti443x` blobs, and
a Mesa shim, on musl through `gcompat`. pvrports does this for Galaxy Tab 2 (`samsung-espresso`)
[V], which is also OMAP4430 [I, from general knowledge of that device]. Before relying on it, ask
the maintainers where `ti443x` came from and under what licence. If unanswered, TI 1.9@2253347 with
tmlind's forward port is the fallback, since its licence is clear [I].

## 3. Route comparison

Efforts are [E], for one person, including device test rounds.

| | A1: libhybris, musl launcher (current plan) | A2: libhybris, glibc launcher | B: TI glibc blobs + rebuilt module | **C: bionic launcher, no libhybris** | D: open user space |
|---|---|---|---|---|---|
| Work | Port hooks to musl, TLS fix, softfp bridge, shim | Add glibc runtime, TLS fix, shim | Port plugin API, omapdrm display, gbm/KMS or our own WSEGL | NDK build, cfg gates, shim, init fixes | Compiler, PDS, microkernel, driver |
| Effort | 4-8 weeks | 2-4 weeks | 6+ weeks, kernel-wide | **1-2 weeks** | Years [V: sgxhumation says multi-year] |
| Main risk | Unknown number of glibc-isms; thin precedent | TLS against unpatched wrappers | Regressing the working display, and sleep | gralloc window on non-Android; launcher cannot stay musl | Unbounded |
| Licence | Google + IMG, extract at install | Same | TI redistributable binary (IMG content still kept out of public repos) | Same as A; bionic libs could later be AOSP builds | Clean |
| Extra RAM | Same driver; libhybris adds about 1 MB [E] | Plus glibc, about 2-3 MB [E] | Plus libdrm, gbm, wayland-server | Lowest: bionic libs only | n/a |
| Power | Same GPU work | Same | Module rebuilt; sleep unknown | Same GPU work | n/a |

Why libhybris is weak here, specifically:

- Upstream's README says Android is "patched to remove TLS accesses in bionic" because of glibc TLS slot problems [V]. We use stock binaries and cannot patch them. Bionic 4.4 reserves slots 3 and 4 for the OpenGL API, right where a libc's own thread data sits [V: `bionic_tls.h`; the overlap is I].
- `hooks.c` and `configure.ac` in current master contain no reference to musl or `__GLIBC__` [V: grep], although the README lists musl. postmarketOS said in 2018 that "with some changes it can be compiled with musl" [V: issue 678], and I could not find its current package.
- Our own plan already records that route 2 on musl "is less tried" [V: `docs/plans/2026-10-22-gpu-rendering.md`].

Why C works: every hard problem in libhybris exists only because two libcs share one process. A
bionic process has one libc, softfp ABI throughout, consistent TLS, and the stock linker, and that
linker already runs the DDK's init on Alpine [R: state file, "GPU SGX540 core bring-up" works].
The launcher already builds for this target at API 19 with NDK r25c [V: `tools/android-dist.sh`,
CI], and `egl.rs` already selects `libEGL.so` on Android [V]. The launcher talks to glassd over a
Unix socket, so it does not care what libc glassd uses [V: `Client::connect_as(path)`; I].

Accepted costs of C:

- The GPU launcher depends on extracted stock bionic libraries at run time.
- Two launcher binaries exist.
- The `--target` differs from the rest of Model B.
- It does not carry to mainline by itself. The module and the blobs are the mainline problem under every route except 2.5.

Memory is not a differentiator. This device has **2 GB**: the kernel reports 1448764k available
plus 647364k reserved, which is 2047 MiB [V: boot log of 2026-11-04]. Lowmem is 508 MB, vmalloc 496 MB
[V]. The battery figure I can source is about 600 mAh (`model-b-energy.md`) [V]; I could not confirm 682.

## 4. Design

### 4.1 Rootfs layout

Everything Google or IMG stays outside git and outside public artifacts.

```
/system/bin/linker                    stock (PT_INTERP of every bionic binary)
/system/lib/...                       libc libdl libm libstdc++ libcutils liblog libhardware libutils
                                      libz libcrypto libEGL libGLESv2 libGLESv1_CM libGLES_trace
/system/lib/egl/egl.cfg               stock
/system/vendor/lib/...                SGX540_120 set listed in model-b-gpu.md section 2
/system/vendor/bin/pvrsrvctl_SGX540_120
/vendor/lib -> /system/vendor/lib     NEW
/vendor/bin -> /system/vendor/bin     NEW
/lib/modules/pvrsrvkm_sgx540_120.ko   ours, built with the running kernel
/usr/bin/glass-launcher               existing static musl build, software
/usr/libexec/glass-launcher-gles      NEW bionic build, ours
/usr/lib/glass/libglass_fbwin.so      NEW bionic build, ours
```

- The extracted set is 4.6 MB uncompressed [V: `du`, excluding hwcomposer]. `hwcomposer.omap4.so` is left out, as `model-b-gpu.md` already decided.
- **Fix needed:** in `wip-gpu/rootfs`, `/vendor` is a real directory holding only `firmware` [V]. The Android EGL loader looks in `/vendor/lib/egl` and `/system/lib/egl` [V: strings of `libEGL.so`], and `hw_get_module` looks in `/vendor/lib/hw` first [V: AOSP 4.4.4 `hardware.c`]. Add the symlinks.

**Extraction:** add `tools/extract-gpu-blobs.sh <system image or partition>`. It mounts or reads
read-only (debugfs, as now), copies a fixed list, and checks each file against
`gpu-blobs.sha256`. Hashes contain no binary content and can be committed. On a real install it
reads the device's own stock `system` partition. A boot image that contains blobs stays local;
CI must never upload one (project rule; it applies to `out/modelb-alpine-gpu-boot.img` too).

### 4.2 Boot sequence

Runs in init after the panel is up. All steps log to kmsg with `OMGB-GPU:`.

1. **Load:** `insmod /lib/modules/pvrsrvkm_sgx540_120.ko`. It must come from the same build as the zImage: modversions CRCs differ between variants, 59 between `tuned` and `tuned-stable` [V: `gpu-module.md`]. OMAPLFB is built into the kernel and the module calls `OMAPLFBRegisterPVRDriver` [V: symbol tables], so no second module.
2. **Nodes:** read the major of `pvrsrvkm` from `/proc/devices` and `mknod /dev/pvrsrvkm` (already in the test image). Add `ln -s /dev/fb0 /dev/graphics/fb0`: `gralloc.omap4430.so` names only `/dev/graphics/fb0`, while `gralloc.default.so` also tries `/dev/fb%u` [V: strings].
3. **Properties:** create `/dev/__properties__`, see 4.3.
4. **Start services:** run `LD_LIBRARY_PATH=/system/lib:/system/vendor/lib:/system/vendor/lib/egl:/system/vendor/lib/hw /system/vendor/bin/pvrsrvctl_SGX540_120 --start --no-module`. There is no firmware file: the EDM microkernel is uploaded from user space through this step [V: `model-b-gpu.md` section 2; `pvrsrvinit` holds this exact command line, strings]. Log rc and `/proc/pvr/version`. Do not use `pvrsrvinit`: it adds SoC checks through `/sys/board_properties/soc/*` for no benefit [V: paths; I: benefit].
5. **Launcher:** the supervisor starts the GPU launcher (4.6).

Expected added boot time is under 2 s [E].

### 4.3 Making bionic content without Android init

Two gaps that the current image has not exercised, because `pvrsrvctl` is the only client so far:

**Property area.** In AOSP 4.4.4, if `/dev/__properties__` is missing, `__system_property_area__`
stays NULL and `__system_property_find` walks from `root_node()`, which computes an address off
that NULL pointer with no guard [V: `system_properties.c`]. So a `property_get` should fault [I].
`libsrv_um` imports `property_get` and `libhardware` calls it [V: imports, `hardware.c`]; the Android
EGL wrapper links `libcutils` and very likely does too [I]. The recorded `pvrsrvctl` success [R] suggests either that path does not reach it or
that XE24's bionic differs, which I cannot tell from here. Do not rely on luck:

- Create an empty valid area: a 128 KiB zero file, owner root:root, mode 0444, with `PROP_AREA_MAGIC` 0x504f5250 and `PROP_AREA_VERSION` 0xfc6ed0ab. The loader checks owner, mode, minimum size, magic and version [V: `system_properties.c`, `_system_properties.h`]. Confirm the field offsets in `prop_area` from that source when implementing. Lookups then return "not found" [I].

**gralloc selection.** With no `ro.hardware`, `hw_get_module` skips every variant and loads
`gralloc.default.so` [V: `hardware.c` logic], which knows nothing about PVR. That would produce a
confusing EGL failure rather than a clean one [I]. Fix: install
`/system/vendor/lib/hw/gralloc.default.so` as a symlink to `gralloc.omap4430.so`. The vendor path
is searched first [V]. Filling the property trie would also work but is more code for no gain.

### 4.4 Presenting: no gralloc service, no SurfaceFlinger

The blobs' only window system is `libpvrANDROID_WSEGL` and it needs an `ANativeWindow` [V:
`model-b-gpu.md`; `libIMGegl` does read a `WindowSystem` setting [V: strings], but writing our own WSEGL is unnecessary].

The stack, top to bottom:

```
GlesCompositor -> eglSwapBuffers -> PVR WSEGL
  -> libglass_fbwin ANativeWindow::queueBuffer -> framebuffer_device_t::post (gralloc.omap4430)
  -> PVR2D flip chain / PVRSRVSwapToDCBuffer2 (imports, V)
  -> OMAPLFB (built-in) -> OMAPLFBFlip -> dsscomp_gralloc_queue, since CONFIG_DSSCOMP=y
```

[V: gralloc imports; config; `omaplfb_linux.c` in the DDK tree. The kernel's built-in copy is what
runs; `glass-kernel/src` is empty here, so I confirmed it only through `System.map` symbols and
assume the logic matches [I].]

**`libglass_fbwin.so`** is an NDK-built C library, about 300 lines [E], Apache-2.0, modelled on
AOSP 4.4 `frameworks/native/libs/ui/FramebufferNativeWindow.cpp` (Apache-2.0: `hw_get_module`,
`framebuffer_open`, `gralloc_open`, `grDev->alloc`, `fbDev->post`) [V]. libhybris'
`fbdev_window.cpp` is the second reference, also Apache-2.0 [V]. Headers come from AOSP
`system/window.h`, `hardware/gralloc.h` and `hardware/fb.h`, which are not in the NDK [I], so vendor
just the structs needed.

It keeps the ABI `gles_probe` already defines, `glass_gfx_create_native_window(u32 w, u32 h, i32
format) -> ANativeWindow*` [V], and adds `glass_gfx_native_window_format() -> i32`.

**Buffers:** `android_display: setting vram to 2764800` [V: boot log] is exactly 3 × 640 × 360 × 4
bytes, so three scanout buffers fit [I]. Whether `numFramebuffers` reports that needs a test.

**Rust changes needed in `glass-gfx-gles`:**

- `EglContext::for_window` takes the first config with RGB888 + ES2 [V]. The Android launcher can set the window's format to match, but a framebuffer's format is fixed (BGRA per `framebuffer.rs` [V]). Add `for_window_with_format(format)`, which walks configs and requires `EGL_NATIVE_VISUAL_ID == format`. A mismatch at `post` is a classic silent failure [I].
- Swap interval stays 1. The launcher presents only on change [V: `framebuffer.rs` notes], so the GPU is idle at rest.
- Y-flip is already handled for window mode [V: `android/README.md`]; "upright on panel" is still an unchecked item in the plan [V].

**Two tiers.** Tier 1 uses `GlesCompositor::offscreen`, `read_frame` and the existing
`FramebufferDisplay`. No window, no flip, no dsscomp involvement; `Compositor::present` already does
this when no window is attached [V: `compositor.rs`]. It costs a read-back of 921,600 bytes per
changed frame, but it gives pixels on the panel from a GPU with the least new code. Tier 2 is the
window above. Ship Tier 2 if it works; Tier 1 is the intermediate milestone and a fallback.

### 4.5 Launcher build

- New binary: same crate, built with `--target armv7-linux-androideabi`, API 19, NDK r25c, features `gles,fb,sound`. Not `cdylib`; no JNI.
- **Dynamic, bionic.** It gets `/system/bin/linker` as interpreter from the NDK default [I]. The static musl build stays for software; the current design note that `gles` needs dynamic linking is right [V].
- **How it finds EGL:** `dlopen("libEGL.so")` and `dlopen("libGLESv2.so")` on Android, which is already the code [V: `egl.rs`], through `LD_LIBRARY_PATH` set by the supervisor. The wrapper reads `egl.cfg` and loads `libEGL_POWERVR_SGX540_120.so` [V: strings, `egl.cfg`].
- **Reuse the shim** in `crates/glass-android/src/compat.rs`: stock API 19 lacks `dl_iterate_phdr`, needed by Rust's std [V]. Move it where both can use it.
- **cfg gates to widen,** from `target_os = "linux"` to `any(linux, android)`:
  - `glass-gfx/src/lib.rs` lines 17 and 30 (`fb`) [V];
  - all the `sound` gates in `glass-launcher/src/main.rs`, and `audio.rs` [V].
  - Check `libc::ioctl` request types; the code already uses `as _` [V].
- `--display fbwin [PROVIDER]`: new launcher mode that creates `GlesCompositor::for_window`, loads the provider, attaches, and uses `window_target()`. `--display fb` keeps meaning read-back or software.
- `DEVICE_BUILD` is already true on Android, so GLES failure is a hard error rather than a silent fallback [V: `renderer.rs`]. Keep that and handle fallback a level up.

### 4.6 Fallback to software

The plan's "no silent fallback" rule is an owner-visible decision [V: plan], so fallback lives in
the supervisor, with a log line, never inside the binary:

```
if gpu_init_ok (steps 1-4) && ! [ -e "$STATE/no-gpu" ]; then
    run glass-launcher-gles --renderer gles --display fbwin ... 
    # exit nonzero within 20 s, or no heartbeat: log, count a failure
fi
exec glass-launcher --renderer software --display fb ...
```

- **Crash latch:** two consecutive failures write a `no-gpu` marker in the persistent state directory, kept until the next software update. Under RAM boot the marker lasts only that boot. A GPU bug must never cost a working UI.
- **Heartbeat:** the launcher touches `/run/glass-launcher.alive` after each present and on a slow timer. No heartbeat for 20 s while on screen means restart in software. A hang in `eglSwapBuffers` is the case this catches.
- **After any flip,** dsscomp owns the pipeline and scanout may sit on buffer 1 or 2 [V: `OMAPLFBFlip` comment and offset]. The software path writes to offset 0 [V: mmap], so its `open` must do `FBIOPAN_DISPLAY` to yoffset 0 and be tested after a GPU run. If dsscomp blocks that, reboot to fall back [I].
- If the blobs are absent the supervisor goes straight to software, which is the fully open configuration.

### 4.7 Sleep: the GPU must not block CORE OFF

What the source guarantees [V: `module.c`, `sysconfig.c`, `sgxinit.c`, `omaplfb_*.c`, `config_kernel.h`]:

- `SUPPORT_ACTIVE_POWER_MANAGEMENT` is on, with `SYS_SGX_ACTIVE_POWER_LATENCY_MS` of 2. Idle SGX drops clocks through `pm_runtime_put_sync`, so a running module should idle at frame granularity.
- On suspend, `PVRSRVDriverSuspend` sets D3, which asks the microkernel to power down. **If D3 fails it returns `-EINVAL`, which aborts system suspend.** The bridge lock is held until resume, so a client ioctl blocks instead of touching the GPU.
- Wake lock `pvrsrvkm` is taken only during hardware recovery (`HWRecoveryResetSGX`), not in normal use. It would show in `/proc/wakelocks` if recovery loops.
- OMAPLFB does not wait for vsync after early suspend or blank (`DontWaitForVSync`), so a swap should not hang across screen-off.
- `sgxfreq` defaults to `on3demand` and scales through `device_scale`; sysfs exposes the governor, with `onoff`, `activeidle`, `cool` and `userspace` alternatives.

What it does not guarantee: that the GFX domain and its clock domain actually reach the state the
PRCM needs for CORE OFF with this kernel's board code, or that `device_scale` leaves no OPP
constraint. The current deep sleep result is CORE OFF at 9.1 mA from one sample, after closing
`ttyO1` [R: state file], and that run's kernel log shows no PVR lines, so **the module was probably
not loaded** [I]. Treat GPU sleep as unproven.

Rules for the launcher and glassd:

1. Nothing draws while the screen is off; this already holds [V: README]. So no frame is in flight when suspend starts.
2. Add `glFinish` after the last frame before glassd requests suspend. Cheap, and it guarantees idle [I].
3. Keep the EGL context and surface across suspend. Rebuilding wastes the resume budget, and contexts should survive since suspend keeps RAM [I]. Check `EGL_CONTEXT_LOST` on the first frame after resume; on it, rebuild the compositor rather than crash.
4. glassd's planned suspend timing (about 40 s after unplug) and its resume wake lock are unchanged [R].
5. Because the module is ours, any blocker can be patched at source [V: rebuild works]. Last resort: GPU off by default and loaded only when an app wants it. Do not try to unload after `pvrsrvctl --start` without testing; there is no evidence that reload works [I].

### 4.8 Known failure modes

| Symptom | Likely cause | Check or fix |
|---|---|---|
| `insmod`: disagrees about version of symbol | Module from another kernel variant | Build both together, compare `compare_ko.py` |
| `(FAIL) SGXInit: Mismatch in client-side and KM driver build options` | Rebuilt module options drifted [V: message in `gpu-module.md`] | Keep the pinned build flags; not decidable from strings |
| SIGSEGV near address 0 in `libc.so` in any bionic process | Missing property area | Section 4.3 |
| `eglInitialize` fails or `eglCreateWindowSurface` EGL_BAD_NATIVE_WINDOW | `gralloc.default.so` loaded, or `/vendor` not linked | Section 4.3; `strace` the opens |
| `framebuffer_open` fails | `/dev/graphics/fb0` missing | Section 4.2 |
| Renders but screen stays old | Post went to a non-scanout buffer; dsscomp idle after blank | Check `/sys/class/graphics/fb0`, `/d/omapdss`; use Tier 1 |
| Boot log's `dispc_ovl_setup failed for ovl 1/2/3` becomes fatal on first dsscomp queue | These errors already occur at boot [V: log]; first flip may hit them [I] | Capture dmesg around the first swap |
| Colours swapped | Config visual differs from fb format | `for_window_with_format` |
| Image flipped or shifted | y-flip; OMAPLFB's overlay config lacks our panel's settings [I] | Compare with software path photo |
| Swap blocks forever | Vsync IRQ missing on the Glass DPI/FPGA path [I] | Set swap interval 0 in the shim as a test |
| Periodic `HWRecovery` lines, UI freezes | Microkernel lockup | Heartbeat falls back; capture `/proc/pvr` |
| Suspend aborts, `PVRSRVDriverSuspend` failed | SGX not idle or stuck | Section 4.7, rule 2; bug in module path |
| Standby stays above 15 mA only with GPU | GFX domain never off | Test 5 matrix; patch module |
| ALSA/ioctl behaves differently under bionic | Different `ioctl` ABI in `libc` crate | Sound regression test in step 4 |
| Rust std panics at start on stock bionic | XE24 older than the 4.4.4 the shim was tested on [I] | `gles_probe probe` shows this first |

## 5. Test plan

**Rule:** RAM boot only through `./fastboot-guard.sh boot <img>`, owner present. Each image
self-reboots and leaves logs in `/proc/last_kmsg`. Read with
`adb shell cat /proc/last_kmsg | grep OMGB-GPU`.

**Step 0, host (no device).**
1. `rustup target add armv7-linux-androideabi`, install NDK r25c: this host has neither [V]; CI has both [V].
2. Build `gles_probe` for that target. Run `readelf -l` and confirm the interpreter is `/system/bin/linker`, with `NEEDED` containing only libraries present in the extracted set.
3. Host unit test for `for_window_with_format` config walking under Mesa.

*Pass:* builds, no undefined symbols at `--no-undefined`.

**Step 1, EGL comes up (first device boot).** Image = current GPU image + section 4.1 symlinks +
section 4.3 fixes + bionic `gles_probe`. init runs `gles_probe probe`, logs the output, then
`gles_probe offscreen /tmp/p.png` and logs its size and a checksum.

*Pass:* `OMGB-GPU` lines show `pvrsrvctl` rc 0, no `(FAIL) SGXInit`, `GL_RENDERER` naming SGX 540, and `offscreen` exits 0.
*Also capture:* `/proc/pvr/version`, `/proc/meminfo` before and after. This replaces the [R] with a real log; save it under `wip-gpu/log/`, which currently holds only the host PNG [V].

**Step 2, Tier 1 pixels.** `gles_probe` home card read back to `/dev/fb0`. *Pass:* owner sees the
10:42 home card, right colours, upright.

**Step 3, Tier 2 window.** `gles_probe window /usr/lib/glass/libglass_fbwin.so`, then 300 frames
of a moving rectangle with frame times logged.

*Pass:* visible motion without tearing, `numFramebuffers` logged ≥ 2, under 4 ms of CPU per frame in `glFinish` (the plan's target [V]), and no new dmesg errors over the run.

**Step 4, launcher.** `glass-launcher-gles --renderer gles --display fbwin --glassd` on a full
UI image, for a hold of 240 s, as in earlier UI images.

*Pass:*
- swipes, tap, sleep and wake, Settings "device" card says `gles`;
- UI clicks audible;
- settings persist;
- A CPU idle check at rest: top shows the launcher at 0%.

**Step 5, sleep gate (needs the owner to lift the deep sleep pause).** Same test script as
suspendtest12, four runs, each about 6 minutes unplugged. Read `Suspended for`, `Successfully put
all powerdomains to target state`, `core OFF` count, and `(q_suspend-q_resume)/65536` mAh.

| Run | State | Pass |
|---|---|---|
| a | No PVR module (control) | Reproduces about 9 mA, CORE OFF ≥ 1 |
| b | Module + `pvrsrvctl --start`, no client | Same as a, within 1 mA |
| c | Launcher alive, screen off, after 20 s of swiping | Same as a, within 1 mA |
| d | c, then wake and swipe 3 times | Frames correct, no `EGL_CONTEXT_LOST`, no HWRecovery |

Also: `/proc/wakelocks` has no active `pvrsrvkm`, and `/d/pm_debug/count` shows the GFX domain OFF.
**Do not make GPU the default until a, b, c agree.** The 9.1 mA result is a single sample, so use at least 2 cycles per run.

**Step 6, fallback.** Three forced failures: blobs absent; `no-gpu` latch set; launcher killed
mid-run. *Pass:* software UI appears within 25 s with a log line each time, and colour and offset are correct after a GPU run.

**Step 7, measurement (for section 7).** Same swipe script, both renderers, at `eco`, `cool` and
`stock` profiles: frame time p50/p95, launcher RSS, and fuel-gauge mA over 2 minutes of constant
swiping and over 2 minutes still with the screen on.

## 6. Model A and the open goal

**Model A.** Little changes: it already draws through a `SurfaceView` with EGL on these blobs,
and it keeps SurfaceFlinger [V: `android/README.md`].
- The new `for_window_with_format` is additive; the Android path stays as it is.
- Moving `compat.rs` must keep the JNI library's symbol.
- Module rebuild rules apply to both.
- `libglass_fbwin` is not used there.

**Closed for a 100% open goal.** No route removes these on SGX540 today:
- `libGLESv2_POWERVR`, `libGLESv1_CM_POWERVR`, `libEGL_POWERVR`, `libIMGegl`;
- `libusc` and `libglslcompiler` (the shader compiler);
- `libsrv_um`, `libsrv_init` (including the EDM microkernel), `libpvr2d`, `libpvrANDROID_WSEGL`;
- `gralloc.omap4430`.

These could be open: the kernel module (already), and the bionic runtime, Android EGL wrapper and
linker, since those are AOSP and could be built from `android-4.4.4` instead of extracted [I].
Our own glass-kk-out build tree is the obvious source [R].

So the 100% open configuration of Model B is, honestly, **the software renderer**, which already
works. Under this design GPU support remains an opt-in closed layer that extraction adds and whose
absence is handled. An open SGX540 driver is not something to plan around: microkernel and PDS are
unreversed, and the closest projects are at the first-triangle stage on other chips [V].

## 7. Performance: what the launcher UI actually gains

I have no measured software frame times on this device; none are recorded [V: searched docs and
state]. Everything below is an estimate [E] until step 7 runs.

| Case | Software today | GPU | Gain |
|---|---|---|---|
| Still screen | 0 CPU, memcmp only on present | 0 CPU | **None** |
| Clock tick | Few dirty spans | Full recomposite, then full-buffer flip | **None, or slightly worse** |
| Swipe across cards (full-screen) | 230,400 pixels: perhaps 5-20 ms at high OPP, 2-3× at 300 MHz, plus BGRA conversion | Under 4 ms CPU (target), one quad per layer | **Real**: steadier 60 Hz at lower CPU clock |
| Menu fade, scroll | As above, smaller area | As above | Small to real |
| Text-heavy new card | fontdue rasterises once; glyph cache either way | Atlas upload | Similar |
| Future: camera preview, video, photo scaling | Costly on CPU | Textures | **The strongest reason** |

Plain arithmetic bounds the fill demand: 640×360 at 60 Hz is under 14 Mpixel/s, a small fraction of
what an SGX540 does, so the GPU will not be the limit [I].

Power: active current is dominated by display and sensors, with a median of 436 mA and the UI at
rest most of the time [V: `model-b-energy.md`]. Swipes last a second or two. Swapping CPU work for
GPU work during those moments cannot move daily battery life measurably [I], and it adds some risk
to standby. Memory: 4.6 MB of libraries on disk, and in use roughly 20-40 MB counting parameter
buffers and layers (4 × 900 KiB of layers [V: plan]), up to a kernel pool ceiling of 10800 pages, about 42 MB [V: config]. On 2 GB that is irrelevant.

**Recommendation on priority:** the current software UI is usable [R]. GPU is a polish feature and a
prerequisite for camera preview, not a fix. Finish deep sleep and companion first, per the owner's
order [R], and let camera work decide how soon Tier 2 is needed.

## 8. Tasks

1. **EGL gate (2-3 days [E]).** Section 4.1 symlinks, section 4.3 property area and gralloc alias, init steps; bionic `gles_probe` from NDK r25c; `tools/extract-gpu-blobs.sh` and hash list. Test steps 0-1. Update `model-b-gpu.md` with the log. If this fails for a reason other than the fixes above, stop and re-evaluate before writing more code.
2. **Window (3-5 days [E]).** `libglass_fbwin.so` and `for_window_with_format`. Test steps 2-3. This decides Tier 1 versus Tier 2.
3. **Sleep gate (1-2 days plus owner time).** Test step 5, once the owner resumes deep sleep work. Blocks any default-on.
4. **Launcher variant (3-5 days [E]).** cfg gates, shared `compat.rs`, `--display fbwin`, supervisor with latch and heartbeat. Test steps 4 and 6.
5. **Measure (1 day).** Test step 7; replace section 7's estimates and record the verdict.
6. **Later, for mainline:** ask pvrports and Maemo Leste about the provenance of `ti443x`; evaluate as in 2.5.

Corrections to `model-b-gpu.md` when task 1 lands:

- Section 1 "pending" items: update from the real log.
- Section 3 loader decision: replace libhybris with route C. Its TI rejection reason should add that TI's user space is DRM-only.
- Section 3 precedent claim ("postmarketOS packages libhybris for this device class"): I could not verify it, so remove or source it.
- Section 4: the gralloc path is `/dev/graphics/fb0`.
