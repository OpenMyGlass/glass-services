# Model B camera, video and multimedia coprocessors: design

Status: design only, 2026-11-04. Nothing here has been run on hardware as part of this work. No device was touched, and no firmware or TI binary was disassembled.

Scope: photo and video on Google Glass Explorer Edition, plain Alpine/musl Linux (Model B) on our kernel 3.4.113 (OMAP4430, OV5680 sensor). The first version uses Google's Ducati firmware. A later path leads to a clean-room replacement. Section 3 covers the whole multimedia subsystem: Ducati/IPU, IVA-HD and the Tesla DSP.

## Evidence labels

| Label | Meaning |
|---|---|
| **[V]** | Verified here by reading the source, header, config or captured log named beside it |
| **[R]** | Reported by a project document or log that was read here. The measurement was not repeated, and it may be from another kernel |
| **[I]** | Inferred. It may be wrong and has a test where that matters |
| **[U]** | Could not be determined from public source or local material |

Sources, with licences in section 12:

- Kernel tree `OpenMyGlass/kernel-omap` at `b6ea856`, which is our 3.4.113.
- Google's `glass-omap-xrx13b` at `989d925` (3.4.94), for the comparison in section 7.3, and the factory `boot.img` kernel.
- `Lasimeri/glass-xec-mainline` at `f6d3da5`: `tools/glass-camera`, `docs/ducati-omx.md`, and `userland/rootfs/usr/sbin/glass-firmware`.
- AOSP `platform/hardware/ti/omap4xxx`, tag `android-4.4_r1`.
- `glass-services` and `glass-apps` at their current main.
- Boot logs from suspend tests 9 to 11. The logs themselves are not committed.

## 1. Decisions in one page

| Topic | Decision |
|---|---|
| Camera owner | A new daemon, `glass-camd`. It is the only process that opens `/dev/rpmsg-omx1` and `/dev/ion` |
| Policy owner | `glassd`. It maps the button to commands, refuses capture off the head, holds wake locks and keeps the screen on. `glass-camd` registers with it as a provider, the same pattern as the companion link |
| UI | The launcher subscribes to a new `TOPIC_CAMERA` and adds gallery cards. Viewfinder frames never cross a socket: `glass-camd` writes them straight to the video overlay |
| Interface to the firmware | TI's DOMX wire (OMX over rpmsg), as `glass-camera` already speaks it. No Android libraries and no libhybris |
| First build | Static musl C tool, derived from `glass-camera` (MIT). Port to a Rust crate before `glass-camd` ships |
| Photo | Short press. First version: a video-mode frame encoded to JPEG on the A9, which uses only the proven path. Then the firmware's still port at 2592x1944, if it works |
| Video | Long press. H.264 from IVA-HD, the camera's buffers passed to the encoder in place, written as fragmented MP4 so a crash loses at most seconds |
| Playback | Software decode first, already measured at 640x360. Hardware decode through OMX as a time-boxed investigation. libdce cannot work with Google's firmware |
| Tesla DSP | Left off. Neither our kernel nor the factory kernel includes it |
| Ducati lifecycle | Boots once at system start, then stays runtime-suspended. No rpmsg file descriptor stays open while idle, because each open holds a kernel wake lock |
| Open path | The camera (capture, ISP, 3A) moves to the A9, and M3 firmware is reduced to running IVA-HD with TI's licensed codecs. Quality is judged against stock on a fixed scene |

Three facts drive this design:

1. **The stack below our code has not changed.** Between Google's public 3.4.94 and our 3.4.113 no file under `drivers/rpmsg`, `drivers/remoteproc` or ION changes (section 7.3) **[V]**. The camera path was proven under Alpine on stock 3.4.94 **[R]**, so it should work here **[I]**. No OMX client has yet run on 3.4.113 **[U]**, which is what milestone M1 settles.
2. **An open `/dev/rpmsg-omx*` blocks system suspend.** `rpmsg_omx_open()` takes a wake lock per service and only the last `release` drops it (`drivers/rpmsg/rpmsg_omx.c:682-684, 750-753`) **[V]**.
3. **The firmware can only be requested early.** The kernel asks for it once, during boot, through the userspace helper. There is no retry API for userspace (section 3.1) **[V]**.

## 2. Corrections to earlier documents

| Document and claim | What the evidence shows |
|---|---|
| `docs/model-b-camera.md`: the sensor is unknown, "do not assume a part number" | The firmware's runtime trace names OV5680 (`sensor_detect_MSP.c: OV5680_EVT2`, in `docs/ducati-omx.md`) **[R]** |
| `docs/model-b-camera.md`: the OMX ABI is undocumented and a musl `domx` port is a substantial effort | The wire is readable from TI's public DOMX source, and `glass-camera` speaks it in about 890 lines of C **[V]** |
| `docs/model-b-camera.md`: "`/dev/rpmsg-omx2` (dsp)" appears | The driver would accept that channel name, but the DSP is not built, so it never appears (section 3.5) **[V]** |
| `docs/model-b-camera.md`: JPEG and H.264 run on "IVA-HD / DSP" | The factory kernel has no DSP firmware entry (section 3.5) **[V]**, so the DSP plays no part in stock capture **[I]**. Where stock JPEG is computed is **[U]** |
| `docs/model-b-camera.md`: the firmware is `ducati-m3-core0.xem3` | A unit above 1024 MB, like the owner's, loads `ducati-m3-core0-2gb.xem3` (`arch/arm/mach-omap2/remoteproc.c:345`, boot log) **[V]** |
| `docs/model-b-camera.md`: native camera belongs in mainline; state notes "camera → mainline" | Superseded by the owner's decision: 3.4 now, mainline later |
| Ducati research report: "factory 3.4 kernel at `~/.cache/omg/ksrc`" | That tree is `b6ea856`, our 3.4.113, not a factory tree **[V]**. Its 3.4 findings still hold for our kernel. For rpmsg, remoteproc and ION they also hold for Google's tree (section 7.3) |

## 3. The multimedia subsystem on Model B

### 3.1 Ducati/IPU: load

Sequence, from `drivers/remoteproc/remoteproc_core.c` and the test 11 boot log **[V]**:

1. `rproc_add()` calls `request_firmware_nowait()` during kernel start, before userspace exists (line 2151).
2. In 3.4 the firmware class has no direct file loading. It uses the userspace helper only (`drivers/base/firmware_class.c`) **[V]**. The kernel's helper path is empty in our config (`CONFIG_UEVENT_HELPER_PATH=""`) **[V]**, so nobody answers the request until init does.
3. The request waits about 60 s in `/sys/class/firmware/` **[R]**. If unanswered, it fails. Nothing in the driver retries it. `rproc_reload()` has one caller, the secure-playback path (`remoteproc_secure.c:386`) **[V]**.
4. When answered, the kernel parses the resource table, creates virtio devices, and `rproc_boot()` requests the firmware a second time, then starts the core.
5. Test 11 timing: "Booting fw image" at 69.135 s, "now up" at 69.734 s, so the core boot takes about 0.6 s **[V]**. The 60 s delay before it was the wait for the first request, because that test image had no early answer **[I]**.

**Design rule.** Init answers pending requests as its first steps, using the same protocol as `glass-firmware` (MIT, section 12): walk `/sys/class/firmware/*/`, read `FIRMWARE=` from `uevent`, write `loading`, `data`, `loading` **[R]**. It then sets `/proc/sys/kernel/hotplug` to `mdev`, which answers later requests, including those of a crash recovery **[R]**.

**Consequences.**

- `/lib/firmware` must be readable within the 60 s window, and readable at all later times for recovery. If the rootfs that holds the firmware mounts late, the camera is lost until reboot **[I]**. Test this directly (F5 in section 10).
- A small optional kernel patch could expose a "retry firmware" sysfs file. It is not needed if init is correct **[I]**.

**Selecting the file.** `(omap_total_ram_size() >> 20) > 1024` selects `-2gb` **[V]**. The installer must supply the file that matches the unit's RAM, not a fixed name (section 7.4).

### 3.2 Ducati/IPU: services

Channels announced by Google's firmware, from the test 11 log **[V]**:

| Channel | Virtio device | Use in this design |
|---|---|---|
| `rpmsg-client-sample` (two) | virtio0 | None |
| `rpmsg-omx0` | virtio0 (SysM3) | None |
| `rpmsg-omx1` | virtio1 (AppM3) | **The camera and the codecs**, as `/dev/rpmsg-omx1` |
| `omaprpc` | virtio1 | None. It is TI's remote function call service, and nothing here needs it |

`rpmsg-resmgr` did not appear in that log, although the kernel has a driver for that name (`drivers/rpmsg/rpmsg_resmgr.c:678`) **[V]**. How the firmware obtains the sensor regulator, GPIO, aux clock, ISS and IVA-HD on this build is **[U]**. Log it during M1: enable `dev_dbg` on resmgr and read `dmesg` during a capture.

### 3.3 Ducati/IPU: suspend

- **Runtime suspend.** Without a `SUSPD_TIME` resource the timeout is 5000 ms (`DEFAULT_AUTOSUSPEND_TIMEOUT`) **[V]**. After 5 s without messages the kernel sends `RP_MBOX_SUSPEND`, waits for the ACK, waits for the core to idle (WFI), then shuts the device down and stops its timers (`omap_remoteproc.c:441-488`) **[V]**. This matches the observed state `suspended (1)` in the test log **[V]**.
- **System suspend.** `rproc_suspend()` sends `RP_MBOX_SUSPEND_FORCED` if the core is running, and resume restores it (`remoteproc_core.c:105-190`) **[V]**.
- **Resume on demand.** A message to a suspended core takes a runtime resume. The resume latency is **[U]**, and matters for shot-to-shot time. M2 measures it.

### 3.4 Ducati/IPU: recovery

- **Triggers:** `RP_MBOX_CRASH`, watchdog and IOMMU fault. All call `rproc_error_reporter()`, whose work item resets every virtio device and re-runs firmware configuration (`remoteproc_core.c:276-302, 2137-2195`, `omap_remoteproc.c:150-154, 276`) **[V]**.
- **Effect on clients:** each open connection moves to `OMX_FAIL` and pending readers wake (`rpmsg_omx.c:1001-1014`) **[V]**. `glass-camera` sees this as a lost connection and exits **[V]**.
- **Duration:** about 2 s, ten crashes observed and all recovered, on stock 3.4.94 **[R]**.
- **Firmware is needed again:** recovery goes through `request_firmware`, so `mdev` hotplug and `/lib/firmware` must work at that moment **[V, I]**.

`glass-camd` behaviour on loss of connection:

1. Stop the pipeline, close all connections, and release ION buffers.
2. Close the current MP4 at its last complete fragment and mark the item "interrupted".
3. Copy `/sys/kernel/debug/remoteproc/remoteproc0/trace1` to the log. This is the firmware's own message output, not an analysis of the binary.
4. Report `FAILED` with a reason.
5. Allow a new pipeline after `rpmsg-omx1` returns.
6. After 3 crashes in 5 minutes, refuse capture until reboot and show "camera unavailable". A crash loop must not drain the battery or heat the frame **[I]**.

### 3.5 Tesla C64x DSP: left off

| Question | Finding |
|---|---|
| Is it built? | No. `# CONFIG_OMAP_REMOTEPROC_DSP is not set` in `notle_defconfig` and in our built Model B config **[V]** |
| Does stock use it? | No. The kernel inside the factory `boot.img` (3.4.94-01990-g1cee760235d, a GPL binary, decompressed and searched for strings) contains `ducati-m3-core0` but no `tesla-dsp` string at all **[V]**, so that build has no DSP firmware entry. On stock, `remoteproc0` is `ipu_c0` **[R]**, which agrees |
| What is the file for? | `tesla-dsp.xe64T` is in the factory system image **[V, name search only]**. Its function is **[U]**: this work did not open it. It looks like a leftover from TI's platform build **[I]** |
| Could we enable it as is? | Not cleanly on the owner's unit. A unit above 1024 MB asks for `tesla-dsp-2gb.xe64T` (`remoteproc.c:122`), and that name was not found in the system image **[V, name search only]** |
| Cost of enabling | 50 MB more carveout, moving the TILER and multimedia heaps down (`board-notle.c:2373, 2385-2388`) **[V]**, plus a kernel config change and a second power domain to keep idle |
| Effect on sleep | Stock and our kernel both reach "Successfully put all powerdomains to target state" with no DSP driver **[V]** |

**Decision: leave it off.** No planned feature needs it. Its plausible future uses are low-power audio work (noise suppression, an audio encoder), which need the proprietary TI C6000 toolchain and original code, with no public Glass-specific reference **[I]**. Revisit only if measured A9 audio cost turns out to be a real problem.

The kernel prints `pwrdm state mismatch(tesla_pwrdm) ON != OFF` and the same for `ivahd_pwrdm`, on both stock and our kernel **[V]**. Since stock still reaches about 5 mA with those lines **[R]**, they are probably a debug-counter artefact **[I]**. Do not treat them as a failure signal. Use the OFF counters in `pm_debug/count` (section 6.3).

### 3.6 IVA-HD: encode and decode

**The interface.** On Google's firmware, OMX is the only route, and libdce is not.

| | OMX over `rpmsg-omx` | libdce over `rpmsg-dce` |
|---|---|---|
| Available with Google's firmware | **Yes**: `OMX.TI.DUCATI1.VIDEO.H264E`, `.CAMERA`, `.DECODER` **[R]** | **No**: the firmware announces no `rpmsg-dce` channel (section 3.2) **[V]** |
| Kernel side on 3.4 | Present, unchanged (`rpmsg_omx.c`) **[V]** | No dce driver or channel is visible in this log **[V]**. Would need a matching firmware |
| Camera | In the same firmware | IPUMM has no camera code **[R]** |
| When it applies | Now | Possibly in stage S5, with an open firmware (section 9) |

**Encode.** Works on stock 3.4.94: 960x540, 15 fps, 768 kbit/s, measured 15.0 fps and 762-779 kbit/s **[R]**.

- The stream is Annex-B, Constrained Baseline, with SPS and PPS in the first buffer **[R]**.
- `glass-camera` requests Baseline with CAVLC and no B frames **[V]**. Whether Main or High profile works on this firmware is **[U]**: test it, because CABAC can save bitrate.
- Codec names and versions come from the firmware's trace and a plain string search only **[R]**. No codec analysis is planned, as TI's licence forbids it.

**Decode.** `glass-decode` reaches `Executing`, then every frame fails with extended error `0x4000` and 0 bytes consumed, and stopping in that state crashes the firmware **[R]**.

- In TI's XDAIS header, bit 14 is `XDM_UNSUPPORTEDPARAM` (`xdm.h`, BSD, from the public XDAIS package) **[V]**. If this firmware passes the XDM value through unchanged **[I]**, the failure is a configuration mismatch, not corrupt data.
- TI's public proxy points at specific differences. It sets stride to `LINUX_PAGE_SIZE` only for 2D buffers and otherwise to the width from `OMX_TI_IndexParam2DBufferAllocDimension`. It also maps colour `0x27` to `OMX_TI_COLOR_FormatYUV420PackedSemiPlanar`, or a padded format for gralloc buffers (`omx_proxy_videodec.c:345-378, 555-595`) **[V]**.
- The Android path uses a single padded gralloc NV12 buffer. `glass-decode` uses two separate TILER containers **[R]**. Try the padded layout first.

**Investigation plan (time box: 5 work days):**

1. Test one change at a time: padded output dimensions from the alloc-dimension query; the TI colour format; single-container NV12.
2. If it still fails, compare against what stock actually sends (T2 in section 11).
3. If it still fails at the end of the time box, keep software decode.

**Playback plan.**

| Stage | Method | Evidence |
|---|---|---|
| P1 | FFmpeg `libavcodec` H.264 decoder, no deblocking, to the video overlay | 640x360 at 12 fps, 500 kbit/s: about 33-34 % of both cores at the 300 MHz cap for the whole stream, on stock 3.4.94 **[R]** |
| P2 | Optional 640x360 proxy stream recorded next to the full video, so playback and Bluetooth sync never touch 720p | Needs a second encoder instance and the camera's second port; both are plausible but **[U]** |
| P3 | IVA-HD decode, if the investigation succeeds | **[U]** |

Software decoding of 720p30 at the 300 MHz thermal cap is likely too slow **[I]**, hence P2. To avoid an FFmpeg dependency in a static binary, `openh264` (BSD-2) is the alternative. Its decoder speed on this CPU is **[U]**, so compare both in M9.

Audio during playback: V1 plays video silently. Bone conduction is a poor speaker, and the launcher already owns the playback front-end for UI sounds. Sharing it needs a small audio route decision, which is deferred **[I]**.

### 3.7 ISS, sensor and memory

**Ownership.** Linux registers no sensor and has no ISS driver. Firmware runs both (`docs/model-b-camera.md`, `board-notle.c`) **[V]**. Linux holds only:

- `CAMERA_PWDN` (GPIO 100), driven low at boot so the sensor is powered down "when Ducati:OFF" (`board-notle.c:1214-1223`) **[V]**.
- VAUX2 at 2.8 V, `always_on` and shared with the other sensors **[R, hw-map]**.

**Sensor modes reported by firmware [R]:**

| Item | Value |
|---|---|
| Preview and video | 64x64 to 1920x1088 |
| Stills | up to 2592x1944 |
| Thumbnails | up to 640x480 |
| Observed mode | `Mode -> video 30fps [960x540]` |
| Camera ports | 0 other in, 1 video in, 2 preview out, 3 video out, 4 measurement out, 5 image out |

**Memory.** Fixed at boot and independent of firmware state (`board-notle.c:2360-2420`, boot log) **[V]**:

| Region | Owner's 2 GB unit | Note |
|---|---|---|
| SMC | 3 MB | Secure workspace |
| ION secure input | 60 MB | Firmware's IOBUFS maps here **[R]** |
| IPU carveout | 107 MB at `0xf5500000` | Firmware code and data |
| ION TILER | 71 MB | Pages behind TILER buffers |
| ION non-secure TILER | 3 MB | |
| ION multimedia | **320 MB** (165 MB on 1 GB units) | Nothing in this design uses it **[I]** |
| **Total reserved** | **564 MB of 2047 MB** | |

**TILER budget.** Approximate sizes, assuming no padding **[I]**:

- 1920x1088 NV12: 2.1 MB + 1.0 MB per buffer.
- 2592x1944 NV12: 5.0 MB + 2.5 MB.
- 720p with 4 buffers: about 5.5 MB plus bitstream buffers.

All of this fits in 71 MB.

**A reclaim opportunity.** The multimedia heap is the lowest region, so shrinking it moves nothing else (`board-notle.c:2388`) **[V]**. The firmware's documented resource table maps only TILER and IOBUFS **[R]**, so the heap should not be needed by it **[I]**. A later kernel patch could return about 300 MB to Linux. Test only after the camera is stable. Do not shrink the IPU, TILER or secure regions, since the firmware's fixed addresses may depend on them.

## 4. Device architecture

### 4.1 Processes

```
 camera button --> glassd --(existing socket, new frames)--> glass-camd --> /dev/rpmsg-omx1 --> Ducati
                     |  policy: wear, wake lock, keep-on          |            /dev/ion          (ISS, IVA-HD)
                     |                                            |            /dev/fb1 (overlay)
                     v                                            v            ALSA capture
                 glass-launcher  <-- TOPIC_CAMERA, media list     media store: JPEG, MP4, thumbnails, index
```

**Why a daemon and not glassd [I, from source structure]:**

- glassd is a single-threaded `poll` loop. Its transport is built for small frames (`docs/api.md`, 64 KiB limit in `services.proto`) **[V]**. Camera work has blocking waits of up to 5 s per OMX call in the current client **[V]**, and it must not stall touch or wear handling.
- A firmware or client bug must not take down touch, the screen policy or the companion link.
- Only one process should open `/dev/ion` and the rpmsg nodes, so a compromise or bug is contained.
- The process can stop completely when not needed, freeing its memory.

**Why not a library in the launcher:** the launcher is the UI, and capture must survive a launcher restart. The launcher does not subscribe to `TOPIC_BUTTON` today (`crates/glass-launcher/src/glassd.rs:39-43`) **[V]**.

**Why not a shell-out per photo:** recording needs a long-lived state machine anyway. For the proof, a one-shot tool is fine (M1).

**Lifetime of `glass-camd`:** started by the supervisor at boot, idle with no device nodes open, using a few MB **[I]**. It registers with glassd on connect and restarts on exit.

### 4.2 Kernel interface

From `include/linux/ion.h`, `omap_ion.h`, `rpmsg_omx.h` **[V]**, which match `glass-camera`'s structures exactly:

| Item | Value |
|---|---|
| `/dev/rpmsg-omx1` | One open per OMX component; each has its own ION handle namespace |
| `OMX_IOCCONNECT` | `_IOW('X', 1, char *)`, name "OMX" |
| `OMX_IOCIONREGISTER`, `UNREGISTER` | `_IOWR('X', 2 and 3, struct ion_fd_data)` |
| `OMX_GET_TIMER` | `_IOWR('X', 5, ...)`, returns a persistent clock and the 32 kHz counter (`rpmsg_omx.c:587-604`). Candidate for aligning video and audio time |
| `/dev/ion` | `ION_IOC_CUSTOM` (`_IOWR('I', 6)`) with cmd 0 for a TILER allocation, `ION_IOC_SHARE` (4), `ION_IOC_FREE` (1) |
| `omap_ion_tiler_alloc_data` | 36 bytes; `w, h, fmt, flags, handle, stride, offset, out_align, token` |
| Formats | 0 8-bit, 1 16-bit, 2 32-bit, 3 page (1D) |
| Heap ids | 0 system, 1 tiler, 2 secure_input, 3 nonsecure_tiler, 4 tiler_reservation, 5 multimedia |

A packet is one 240-byte `write()`, and replies and callbacks are reads of the same size. The full table is in `docs/ducati-omx.md` **[R]**, and its sizes and numbers were checked against the headers above **[V]**.

**Device nodes.** Model B's init mounts tmpfs on `/dev` and uses `mdev` **[V]**, so `rpmsg-omx*` and `ion` come from hotplug. They should be owned by root with group `camera`, mode 0660, and only `glass-camd` in that group.

### 4.3 IPC: additions to `services.proto`

Additive only, as the proto's own header requires **[V]**. Current highest numbers: `ClientFrame` 17, `ServiceFrame` 22, `Topic` 9 **[V]**.

```proto
enum Topic { ...; TOPIC_CAMERA = 10; }       // CameraState, MediaItem, MediaRemoved

message ClientFrame { oneof body { ...
  CameraCommand camera_command = 18;         // launcher -> glassd; menu or UI actions
  CameraRegister camera_register = 19;       // provider only; this client owns the camera
  CameraState provider_camera_state = 20;    // provider only; request_id = glassd's id
  MediaItem provider_media_item = 21;        // provider only; a new or changed item
  MediaRemoved provider_media_removed = 22;  // provider only
  MediaQuery media_query = 23;               // client -> glassd; answered by MediaItem frames
}}

message ServiceFrame { oneof body { ...
  CameraState camera_state = 23;             // TOPIC_CAMERA
  MediaItem media_item = 24;                 // TOPIC_CAMERA
  MediaRemoved media_removed = 25;           // TOPIC_CAMERA
  CameraCommand provider_camera_command = 26; // to the provider only
}}

message CameraRegister { repeated string features = 1; }   // "photo", "video", "audio", "playback"

message CameraCommand {
  enum Action { ACTION_UNSPECIFIED = 0; ACTION_PHOTO = 1; ACTION_VIDEO_START = 2; ACTION_VIDEO_STOP = 3;
                ACTION_VIEWFINDER_START = 4; ACTION_VIEWFINDER_STOP = 5; ACTION_PLAY = 6; ACTION_PLAY_STOP = 7;
                ACTION_DELETE = 8; }
  uint32 request_id = 1;
  Action action = 2;
  string media_id = 3;                       // PLAY, DELETE
  string note_id = 4;                        // attach the capture to this note; empty = none
}

message CameraState {
  enum State { STATE_UNSPECIFIED = 0; STATE_OFF = 1; STATE_STARTING = 2; STATE_PREVIEW = 3; STATE_CAPTURING = 4;
               STATE_RECORDING = 5; STATE_STOPPING = 6; STATE_PLAYING = 7; STATE_FAILED = 8; }
  uint32 request_id = 1;
  State state = 2;
  uint32 elapsed_ms = 3;
  uint32 remaining_ms = 4;                   // until the time, storage or thermal limit
  string reason = 5;                         // "off-head", "storage-full", "thermal", "battery", "firmware", ...
  string media_id = 6;
  bool snapshot = 7;
}

message MediaItem {
  string id = 1;                             // stable: path + size + mtime, as omg_protocol::MediaItem
  bool video = 2;
  string path = 3;
  string thumb_path = 4;
  uint64 taken_at_ms = 5;
  uint32 width = 6;
  uint32 height = 7;
  uint32 duration_ms = 8;
  uint64 size = 9;
  bool synced = 10;
  bool interrupted = 11;
  string note_id = 12;
  bool snapshot = 13;
}
message MediaRemoved { string id = 1; }
message MediaQuery { uint32 limit = 1; uint64 before_ms = 2; }
```

Add capabilities `camera` and `video` to `Welcome` only while a provider is registered, as other capabilities are conditional **[V, pattern]**. Thumbnails travel as file paths, never in frames, because the launcher reads the same filesystem. Android/Model A can later provide the same interface from its glue, keeping one launcher path **[I]**.

### 4.4 Ownership and safety rules

Rules 1 to 5 are enforced in glassd or `glass-camd`, with no setting and no client override.

| # | Rule | Enforced by |
|---|---|---|
| 1 | **Capture only follows a local physical action on the Glass**: the camera button, or a menu tap. The companion can never start capture. No `omg-protocol` message does this | Absence of the message; glassd accepts commands from local clients only |
| 2 | **The camera always shows it is active.** The prism is on whenever the camera runs, with a visible state. Recording forces the screen on and overrides screen-off gestures. If the panel cannot be turned on, capture is refused | glassd keep-on, `glass-camd` checks `ScreenState` |
| 3 | **Audible cue, with no way to mute it**, at each photo and at recording start and stop, independent of the volume setting's zero | `glass-camd` requests the sound; failure to play logs but does not block (the owner's unit has proven playback) |
| 4 | **Off the head, the button does nothing.** glassd already delivers the camera key off the head without waking **[V: `tests/mock.rs:404`]**. Add a refusal here so a pocket press cannot capture. When wear is unknown (the sensor has never reported on-head), allow it, with rules 2 and 3 still applying | glassd |
| 5 | **Recording stops** on a second press, tap, the time limit, thermal limit, low battery, full storage, doff or `glass-camd` losing glassd | Both |
| 6 | Only one owner. Second requests get `FAILED` with reason "busy". Remove `glass-camera`'s behaviour of killing other processes by name **[V, it does this]** | `glass-camd` |
| 7 | Exit paths always reach Loaded and free the buffers, also on `SIGTERM`. A crash relies on kernel `release` to disconnect (`rpmsg_omx.c`, `OMX_DISCONNECT`) **[V]**. Whether the firmware fully cleans up after an abrupt exit is **[U]**, so test F3 in section 10 | `glass-camd` |
| 8 | Media files are mode 0640, group `glass`. No media in logs. Location is not written to EXIF by default, matching glassd's rule of never storing fixes **[V, `docs/api.md`]** | `glass-camd` |

**Is there a recording LED?** The board file defines two GPIO LEDs, `notleboard::status1` (GPIO 7, heartbeat trigger) and `status2` (GPIO 8, mmc0 trigger) (`board-notle.c:419-431`) **[V]**.

- Their names and triggers look like development-board status lights **[I]**. Nobody has reported a blinking heartbeat on a retail unit, and whether these GPIOs are populated or visible is **[U]**.
- No camera-specific indicator appears in the source **[V]**.
- Test: with the owner watching, toggle `/sys/class/leds/notleboard::status1/brightness` (T1 in section 11).
- If no LED is visible, the lit prism is the only external sign. It is visible to others only at some angles **[I]**. The owner should judge whether that is acceptable, and it is why rule 3 exists. Local law on recording is the wearer's responsibility; this is not legal advice.

## 5. Features and user flows

### 5.1 Button mapping

`glassd` already does the tracking: `PRESS` on release before 500 ms, `LONG_PRESS` at 500 ms, `RELEASE` after, with `screen_off` at press time, and a wake on key down (`buttons.rs`, `daemon.rs`) **[V]**.

| Input | Screen on or off | Action |
|---|---|---|
| Short press, idle | Either | Take a photo |
| Long press, idle | Either | Start recording, viewfinder on |
| Short press, recording | | Stop recording |
| Tap on touchpad, recording | | Stop recording |
| Press from system suspend | | Wake, then as above. The kernel synthesizes the key press on wake **[R]**. A short press may end before glassd sees it; test F7 |

### 5.2 Photo flow

1. Play the shutter cue; state `STARTING`. Open connections, allocate buffers, go to Executing.
2. Wait for exposure to settle (a count of frames in V1).
3. Capture. Encode. Write atomically: temp file, `fsync`, `rename`, directory `fsync`.
4. Show the picture full screen for 3 s through the launcher card. Add it to the timeline's history.
5. Close everything. The Ducati suspends after its 5 s timer.

**Target latency**, press to picture on the prism: under 3 s when awake. This is a guess to be replaced by M2's measurements **[U]**. If it is too slow, keep the pipeline warm for 30 s after a shot, which costs only that period's power **[I]**.

**V1 (M1): video-mode frame.**

- Uses the proven `OMX_CaptureVideo` preview path at 1920x1080 (or 1280x720 if needed), keeping one frame after settling.
- JPEG on the A9 with `libjpeg-turbo` (NEON).

**V2 (M2): still port.** TI's HAL sequence **[V: `OMXCapture.cpp`, `OMXCameraAdapter.cpp`, `OMXAlgo.cpp`]**:

1. `OMX_IndexCameraOperatingMode` = `OMX_CaptureImageProfileBase`.
2. Configure port 5: `eCompressionFormat = OMX_IMAGE_CodingJPEG`, with `OMX_COLOR_FormatCbYCrY`.
3. Set `OMX_IndexConfigCaptureMode` and the callback for shutter events.
4. `FillThisBuffer` on port 5, then `OMX_IndexConfigCapturing` = true.

The Glass firmware's capability structure differs from KitKat's headers **[R]**, so the sequence may need changes, and it has never been run from Linux **[U]**. V1 is the fallback, at reduced resolution. Stock output may also depend on DCC tuning data (section 9.4).

### 5.3 Video flow

1. Long press. Cue, keep-on, viewfinder, `RECORDING`, with a red dot and elapsed time drawn into the overlay by `glass-camd` so that it cannot be hidden by the launcher.
2. Camera and encoder run with shared buffers, as `glass-camera` does. Bitstream goes to the MP4 writer.
3. Stop for any reason in rule 5. Finish the fragment. Cue. Thumbnail from the first key frame's picture buffer. Add a timeline card.

**Default limits**, all settings the companion can change later:

| Item | Default | Reason |
|---|---|---|
| Resolution, rate | 1280x720, 30 fps; fall back to 960x540, 15 fps | 720p30 is the public stock figure **[I]**. Only 960x540 at 15 fps is proven **[R]** |
| Bitrate | 4 Mbit/s (about 30 MB per minute) | To tune against the quality benchmark |
| Key frame interval | 2 s | Bounded seek and crash loss |
| Time limit per clip | 5 min | Thermal, to be revised by M5 |

### 5.4 Viewfinder

- **V1:** copy each preview frame into `/dev/fb1` as YUYV and flip by register, the method of `glass-fb -n` **[V]**, which is proven with this camera on the stock kernel **[R]**. This uses `/dev/mem` for DISPC writes **[V]**, which should move to a small kernel ioctl before shipping **[I]**.
- **Later:** point the VID1 plane at the TILER NV12 buffer directly, through `dsscomp` (`CONFIG_DSSCOMP=y` **[V]**) or a small driver addition. It removes the copy and much of the CPU load **[I]**.

Keep to the prism's 640x360 from the camera's second port. If two ports cannot run together, scale from the recording buffer on the A9 at a lower rate. A viewfinder is not needed for a photo in V1. Only recording uses one.

The launcher keeps fb0. While the overlay is on, it stops drawing and input focus goes to a small "camera mode" so a swipe does not act on hidden cards.

### 5.5 Gallery in the timeline

The timeline already has what is needed **[V: `crates/glass-ui/src/card.rs`, `timeline.rs`]**:

- `CardContent::Image { image, caption, timestamp_s }`, `insert_history`, `update_card`, `remove_card`.
- Paint on demand, with at most 4 resident layers of 900 KiB.

Design:

- History cards are built from `MediaItem`, newest first. Load only a window of about 20 from `glass-camd`'s index, reading more with `MediaQuery`.
- The card uses the 640x360 thumbnail (decoded from JPEG when painted). Video cards add a play mark and duration.
- Tap opens a menu: Play (video), Attach to note, Delete (with confirmation), Details. Add the strings in all four languages (IT, EN, FR, ES). The existing i18n test checks them **[V]**.
- A failed `interrupted` video still appears, labelled, since it plays up to its last fragment.

### 5.6 Formats, paths, thumbnails

| Item | Choice | Note |
|---|---|---|
| Photo | JPEG, 4:2:0, quality 90, EXIF date and orientation | Hardware JPEG in V2 sets its own quality |
| Video | H.264 in **fragmented** MP4, 2 s fragments | A crash loses only the last fragment. Needs a written `moof` per fragment and no final `moov` rewrite |
| Audio in video | See section 5.8 | |
| Names | `IMG_YYYYMMDD_HHMMSS_mmm.jpg`, `VID_YYYYMMDD_HHMMSS_mmm.mp4` | No `..`, no user input in paths |
| Tree | `<media>/DCIM/`, `<media>/thumbs/<name>.jpg`, `<media>/index/<name>.json` | Rebuilt from files if lost |
| Thumbnail | 640x360 (timeline) JPEG quality 75, about 40 KB; no separate smaller size in V1 | Also the Bluetooth preview |
| Time | Wall clock; if never set, flag `time_unknown` and let the companion correct it | The Glass has no internet **[V, locked decision]** |

Plain MP4 players on phones accept fragmented MP4 **[I]**. Offer a remux on the companion if a gallery rejects them.

The MP4 writer: a small own implementation of about 500 lines of Rust, with no dependency, because only one video and one audio track and fixed structures are needed **[I]**.

### 5.7 Storage: not settled, owner decision needed

Facts:

- Observed partitions on another XE unit: boot and recovery 8 MiB each, system 1 GiB, cache 768 MiB, userdata 13.5 GB **[R]**.
- The owner's unit has `mmcblk0` of 14.6 GiB, p1-p10 **[V]**. Its partition names were not confirmed.
- Model B today boots from RAM. `/dev/block/mmcblk0p9` does not even exist under `mdev` **[R]**, and nothing persists.
- **Current RAM images (about 10.16 MB) are already larger than an 8 MiB boot partition [V sizes, R partition]**. A flashed Model B must keep firmware and most of userspace outside the boot image.

Options:

| Option | For | Against |
|---|---|---|
| A. A directory on the existing userdata ext4 (for example `/data/omg/media`) | No reformat; keeps Model A's data; most capacity | Shares space with Model A; mounted filesystem shared between two OSes |
| B. Reformat userdata for Model B | Cleanest layout | Destroys Model A's data |
| C. Cache partition | Separate, no effect on data | 768 MiB is about 25 minutes of video |

**Recommendation: A**, with Model B's rootfs on the same partition, in a subdirectory or an image file. Until the owner decides:

- RAM-boot tests use tmpfs capped at 128 MB and nothing persists.
- No flash write happens without the owner's explicit approval.

Policies, once settled:

- Refuse to start recording under 500 MB free, and stop at 200 MB free.
- Cap media at 4 GB by default.
- Never auto-delete. Offer to delete items already synced.

### 5.8 Audio with video

Known:

- The board declares `.has_dmic = 1`, `.has_hsmic = 0` and registers `dmic-codec` (`board-notle.c:1293-1307`), and `CONFIG_SND_SOC_DMIC=y` **[V]**.
- A teardown lists two Wolfson WM7231 MEMS microphones **[R]**.
- The card has a `Multimedia Capture` front-end (`MultiMedia2`) and a `DMIC` back-end, and the ABE driver has DMIC gain and UL mixer controls (`omap-abe-twl6040.c:694-745`, `omap-abe-mixer.c`) **[V]**.
- The launcher has its own raw `SNDRV_*` ioctl code and playback only (`audio.rs`) **[V]**. `glass-xec-mainline`'s finding that Alpine's `alsa-lib` misbehaves on 3.4 (time64 ioctls) **[R]** is another reason to reuse it.

Not known **[U]**: the working mixer route for capture, the correct gains, which DMIC pair carries which microphone, and whether capture interferes with playback.

**Approach, reusing what solved the output problem:**

1. Capture a gold reference on stock: run dictation or video recording on the stock image and dump mixer, DAPM and PCM state while it runs. The existing audio-state dump tool applies.
2. Implement capture by raw ioctls, at 48 kHz S16, then downmix to mono.

**Encoder.** Choose between:

| | AAC-LC | Opus |
|---|---|---|
| Phone compatibility in MP4 | Best | Uncertain on iOS **[U]**. Needs a check, since iOS is a first-class target |
| Licence | Fraunhofer FDK: non-copyleft but with patent caveats; FFmpeg's native AAC: LGPL | BSD-3 |
| Cost at 48 kHz mono | **[U]** | **[U]** |

**Recommendation:** Opus at 32 kbit/s via `libopus` to keep licensing simple, with the companion transcoding for iOS if a check shows it needs that. The owner may choose otherwise (section 13).

**A/V sync.** Video timestamps come from the firmware in `nTimeStamp` **[V]**, whose clock domain is **[U]**. Read `OMX_GET_TIMER`'s `clock32k` and ALSA's position at start and periodically to map the two **[I]**. Test with a clap and measure against a 40 ms target.

If capture is not ready when video ships, video records silently. Do not delay M5 for M6.

### 5.9 Thermal and battery

**What is known:**

- Thermal: the case governor's thresholds are 62 °C hot and 57 °C cold (`case_governor.c:31-35`) **[V]**. The CPU cap is applied through `max_thermal` in `omap-cpufreq.c:61, 163, 276` **[V]**. Under continuous streaming the CPU settled at 300 MHz **[R]**.
- The cap is on the CPU. Whether it also affects the M3 or IVA-HD is **[U]**. The encoder should keep real time at 300 MHz if the A9 only copies bitstream **[I]**, since A9 load with the camera alone was about 11 % at 600 MHz **[R]**.
- At 300 MHz both cores are shared with display copy, MP4 writing, audio encoding, and any radio, so viewfinder cost at the cap is the first number to measure.
- Power: screen off 42 mA, suspended 9.1 mA (one sample) on Model B; stock about 5 mA **[R]**. Camera and encoder cost is **[U]**. A secondary source reports 100-145 mW for IVA-HD encode **[R, unverified]**.

**Design rules:**

- Read CPU limit and battery temperature every 5 s while recording. The fuel gauge reports temperature **[V, log]**. The exact sysfs value for "the thermal cap is active" is **[U]**: `scaling_cur_freq` staying at 300 MHz while busy is the working proxy.
- At the cap, keep recording: drop the viewfinder rate to 5 fps first, then turn the viewfinder into a static "recording" card (the prism stays on for rule 2).
- Stop at battery temperature ≥ 45 °C (the figure to revise in M5) or if 62 °C case throttling lasts over 60 s. The user sees "too warm" as the reason.
- Block starting at battery < 10 %, and stop at 5 %.

**Target for M5:** at least 5 minutes of 720p30 in a warm room with no stop, and at least 20 minutes total from a full charge, measured by the fuel gauge. If it falls short, change defaults (960x540, or 15 fps) rather than the rules.

## 6. Sleep interaction

### 6.1 Rules

| # | Rule | Reason |
|---|---|---|
| 1 | `glass-camd` opens `/dev/rpmsg-omx*` only for the duration of a capture, recording or playback | Each open service holds a wake lock **[V]** |
| 2 | glassd never starts suspend while the camera state is anything but `OFF`, and `glass-camd` reaches `OFF` before releasing its request | Clean state; no forced firmware suspend mid-capture |
| 3 | No ION or TILER buffers stay allocated when idle | No value in keeping them. Removes a variable for OFF-mode context save **[I]** |
| 4 | Ducati stays loaded, runtime-suspended. It does not get unloaded for sleep | The firmware can be requested only at boot, and test 11 reached CORE OFF in this state **[V, one sample]** |
| 5 | Any capture holds a named wake lock for its duration, `glass-camd` | Explicit, and visible in `/sys/power/wake_lock` |
| 6 | After a wake, glassd takes its lock first, then the normal flow runs | Existing finding: otherwise the system re-suspends and timers stop **[R]** |

### 6.2 When to start and stop

| Component | Starts | Stops |
|---|---|---|
| Ducati | At boot, firmware answered by init | Never, in normal running. Autosuspends 5 s after last use, and is forced at system suspend **[V]** |
| Sensor | Requested by firmware when the camera component runs **[I]** | With the component. `CAMERA_PWDN` stays low otherwise **[V]**. Check during M1 that the line returns low |
| ISS, IVA-HD | Same, on demand by firmware **[I]** | Same. Confirm through OFF counters |
| Tesla | Never | Not applicable |

An alternative, not recommended: boot the Ducati only at first use, by delaying the firmware reply. It fails against the 60 s request window. It would save nothing in sleep, because carveouts are fixed and a suspended core draws little **[I]**.

### 6.3 Check in every sleep test

Sleep work is paused by the owner, and its result affects this design. Test 12 is pending **[R]**. It decides whether CORE OFF needs only the UART2 open-and-close fix.

- Log lines: `Successfully put all powerdomains to target state`.
- Counters: `/sys/kernel/debug/pm_debug/count`, for `core_pwrdm`, `ivahd_pwrdm`, `tesla_pwrdm`, `cam_pwrdm`, `dsp` and `abe`: OFF counts increase across a sleep.
- `cat /sys/power/wake_lock` shows no `rpmsg-omx*_wakelock`.
- `/sys/kernel/debug/remoteproc/remoteproc0/state` shows `suspended`.
- Drain by fuel gauge `Qpassed` over at least 5 minutes, cable out.

**Pass:** after a photo and a 30 s video, sleep current is within 1 mA of the sleep current before them (same image, same boot). A photo that adds a permanent drain fails the milestone.

## 7. Build and packaging

### 7.1 Tool strategy

| Option | Decision |
|---|---|
| Static musl C tool (fork of `glass-camera`) | **Use for M1 and M2.** Fastest proof, known behaviour. `glass-camera.c` and `domx.h` compile with only `pthread`, `-Iomx` |
| Rust wrapper around the C tool, as FFI | Rejected as a long-term form. It adds a C toolchain to the Rust build and keeps thread and error handling in two languages |
| **Rust crate `glass-domx`** used by `glass-camd` | **Target for M3 onward.** The protocol is small: 240-byte packets, about 15 functions, a reader thread. Struct layouts come from `repr(C)` definitions with size and offset tests generated from the C headers |
| Port or load TI's Android `domx` libraries | Rejected. They need bionic, gralloc and libhybris, and offer nothing the wire does not |

The crate should keep the rule from `domx.h` that the reader thread never makes a call itself **[V]**.

**Build commands, not run in this work:**

```sh
# M1 tool: static, for the kernel's ARMv7, with the toolchain already used by the project
zig cc -target arm-linux-musleabihf -mcpu=cortex_a9 -O2 -static \
    -Iomx -o glass-camera glass-camera.c -lpthread

# glass-camd, same as the launcher's documented build
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=rust-lld \
    cargo build -p glass-camd --release --target armv7-unknown-linux-musleabihf
```

Rust and musl binaries already run on this kernel **[R]**. `glass-camera`'s time calls go through musl's 32-bit fallback, as the launcher's do **[I]**.

### 7.2 Where the code lives

| Piece | Repository | Licence |
|---|---|---|
| `glass-domx`, `glass-camd`, MP4 writer | `glass-services` (same workspace as glassd) | GPL-3.0, plus MIT notice for code derived from `glass-camera` |
| Proto additions, glassd policy | `glass-services` | GPL-3.0 |
| Cards, camera mode, strings | `glass-apps` | GPL-3.0 |
| Kernel config pin (`modelb-camera.config`) and any patch | `glass-kernel` | GPL-2.0 |
| Extraction script, hashes | Installer | GPL-3.0 |

**Do not commit:** firmware, TI headers beyond those under BSD (the OMX headers are Khronos and TI BSD, which is fine), or any image containing firmware.

### 7.3 What changes between stock 3.4.94 and our 3.4.113

Compared here: Google's `989d925` and our `b6ea856`, both fetched, tree against tree. 1178 files differ **[V]**.

| Path | Difference |
|---|---|
| `drivers/rpmsg/`, `drivers/remoteproc/` | **None [V]** |
| `drivers/gpu/ion/`, `include/linux/ion.h`, `omap_ion.h`, `rpmsg_omx.h` | **None [V]** |
| OMAP DMM/TILER driver paths, `drivers/iommu/omap-*` | **None [V]**. Only AMD and Intel IOMMU files changed |
| `arch/arm/mach-omap2/remoteproc.c`, `omap4_ion.c`, `board-notle*.c`, `plat-omap`, mailbox | **None**. In `mach-omap2`, only `mux.c` changed **[V]** |
| `drivers/video/omap*`, `dsscomp` | **None [V]** |
| `sound/soc/omap/` | Only `omap-pcm.c` changed **[V]**. Not on the OMX path; relevant only to microphone work |
| `include/linux/virtio.h`, `drivers/virtio/virtio.c` | Refactor: `dev_to_virtio` becomes an inline function; same behaviour **[V]** |
| `drivers/base/firmware_class.c` | Adds a guard against empty names (+3 lines) **[V]**. Harmless here |

So nothing should need changing in `glass-camera` or the kernel for this path **[I]**.

**Three limits on that statement:**

1. The stock binary reports itself as `3.4.94-01990-g1cee760235d` **[V]**, which is not the pinned `989d925`, and one lookup of that hash on `android.googlesource.com/kernel/omap` returned not found **[V]**. So the comparison is with the closest public source, not the binary's own.
2. `glass-camera`'s headers came from `1091b53` (3.4.83) **[R]**. Our headers match them on every structure checked **[V]**, which is the point that matters.
3. Our config differs from stock's. `configs/modelb-camera.config` (PR 19, merged) guards the camera options **[V]**. The options it pins are on in our built Model B config **[V]**. Make sure the camera images use it. The local `glass-kernel` checkout is behind and does not have that file **[V]**, so update it first. `CONFIG_MEDIA_CONTROLLER` is not needed for this path and can be dropped from that overlay **[I]**.

**Kernel work actually needed: none for M1.** Later, optional:

- A safer overlay flip than `/dev/mem`.
- Shrinking the multimedia heap.
- A firmware retry file.

### 7.4 Firmware extraction at install

Never in any repository or published image.

**Sources, in order:**

1. The user's Glass, when it still has stock `/vendor/firmware` (`/system/vendor/firmware` on the factory layout **[R]**), read with read-only commands.
2. Google's public factory zip, supplied by the user: the host tool unpacks the sparse system image.

The project's `tools/sparse.py` handles the format **[V, exists; not reviewed for this]**.

**Steps:**

1. Decide the file by the unit's RAM (above 1024 MB means `-2gb`). The factory image has both **[V]**.
2. Check it against recorded SHA-256 values, stored in the installer (hashes are not blob content):

   | File | Size | SHA-256 |
   |---|---|---|
   | `ducati-m3-core0-2gb.xem3` | 6344279 | `fd5edc30d490b665c59daa87d67f4b12dfd9ea66aebdedeeba2310e4626a7ff4` |
   | `ducati-m3-core0.xem3` | 6344271 | `2c5e9e9853cd916dea3551d9a60933660dd7a02c55decf443ea94d68e98bb970` |

3. Install to `/lib/firmware` on persistent storage (once decided), or in a locally built RAM image for tests.
4. Refuse an unknown hash, unless a `--accept-unknown-firmware` flag is passed. Do not guess.
5. Write a licence note next to the file saying where it came from and that it is not redistributable by us.

**Flash budget.** The firmware is 6.3 MB, plus the sizes in section 5.7. A boot image cannot hold it if the 8 MiB limit applies, so firmware belongs on the rootfs.

**DCC tuning files.** See section 9.4. Treat the same way: extract locally if they exist on the unit.

## 8. Milestones and effort

Effort is focused work days for implementation and analysis, and excludes waiting on owner tests. Estimates are **[I]**.

| # | Milestone | Depends on | Effort | Done when |
|---|---|---|---|---|
| **M1** | One-photo proof over a RAM boot image, no adb | - | 1-2 | See section 10.1 |
| M2 | Still port at 2592x1944, AE settling, DCC check, baseline capture | M1 | 2-4 | A still from the firmware's image port, or a written reason it cannot work; first numbers against stock |
| M3 | `glass-domx`, `glass-camd`, proto, glassd policy, launcher card; photo by button on RAM images | M1 | 6-9 | Section 10.2 |
| M4 | Storage decision and persistence | Owner | 2-3 | Photos survive reboot; index rebuild works |
| M5 | Video with viewfinder and fragmented MP4 | M3 | 4-6 | Section 10.3 |
| M6 | Microphone capture and A/V sync | M5, stock reference | 3-5 | Section 10.4 |
| M7 | Sleep and recovery hardening | M5, sleep work | 2-4 | Sections 6.3, 10.5 |
| M8 | Companion sync and note attachments | M4, companion | 5-10 | Section 10.6 |
| M9 | Playback: software, then time-boxed hardware decode | M5 | 2-3, +5 | Section 10.7 |
| M10 | Installer: extraction, verification, permissions | M4 | 2-3 | Fresh install on the factory zip, unknown hash refused |

Roughly 35-55 work days for the stock-firmware product. Stages S0 to S6 in section 9 are separate and much larger.

## 9. Open path: to a clean-room firmware

### 9.1 What changes hands

Owner's rule: **TI's compiled video codecs stay. Everything else may be rewritten.** Scope of the rewrite: M3 runtime, capture, ISP, 3A.

| Part | Now | Target |
|---|---|---|
| M3 runtime, rpmsg | Google firmware | sysbios-rpmsg or TI IPC (BSD) |
| Video codecs on IVA-HD | Inside Google firmware | TI's redistributable binaries via IPUMM (TSPA licence; **no reverse engineering**, TI devices only) **[V, licence text]** |
| Sensor control | Firmware | New OV5680 driver, from black-box observation |
| CSI-2, ISP, resizer, H3A | Firmware | Driver from public TRM SWPU231 ch 8, starting from `omap4iss` v6.12 (GPL-2.0) |
| AE, AWB, tuning | Firmware plus unknown tuning | libcamera-derived algorithms (check licence per file); own tuning |
| JPEG | Firmware or A9 | `libjpeg-turbo` on A9, then SIMCOP if worthwhile |

**Where the camera runs: on the A9.** `omap4iss` already drives the ISS from Linux **[R]**, TRM ch 8 is public **[R]**, debugging is far easier, and this carries to mainline. M3 firmware only runs codecs. This is a change in emphasis from "rewrite the firmware". The owner should confirm it.

### 9.2 Reusable material

| Material | Licence | Use | Gap |
|---|---|---|---|
| `omap4iss` v6.12 (removed in Linux 6.13) | GPL-2.0 | CSI-2, IPIPEIF, basic flow, clock domain details | No H3A; IPIPE sets format and size only; resizer is a dummy **[R]**. V4L2 API far newer than 3.4 |
| libcamera AE/AWB/CCM/LSC | LGPL-2.1 for the library **[R, not checked per file]** | Algorithms and calibration tools | Drives other ISPs; needs an IPA for H3A statistics |
| Infinite-ISP, openISP | Apache-2.0, MIT **[R]** | Reference pipeline | Not hardware-specific |
| sysbios-rpmsg | BSD-3 **[R]** | Right generation for 3.4's resource table | Needs TI's compiler (proprietary, free to download) |
| MobiAqua `ti-ipumm` | BSD-3 for source; codecs TSPA **[R]** | OMAP4 base image | Newer vring layout, needs porting back to fixed addresses; encoder on OMAP4 unconfirmed **[R]** |
| Mainline `ov5648`, `ov5670` | GPL-2.0 | Driver structure | Not OV5680 registers |

**Warning:** no public OV5680 register documentation or driver was found **[R]**. Leaked datasheets exist on third-party sites; do not use them.

### 9.3 Legal limits for the observation work

Allowed in this plan: observing hardware behaviour and interfaces on the owner's own unit. Not allowed: disassembling Google's firmware or TI codecs, or reading leaked or NDA material. Observation methods, in increasing effort:

1. **Runtime traces** the firmware already prints (`trace1`).
2. **Kernel-side logging of resource requests** (resmgr, clocks, regulators): which rail, which clock, at what rate.
3. **Reading ISS and CSI-2 registers** from the A9 through `/dev/mem` while a stock capture runs: lane count, data type, frame size, PHY timing. Hardware state only.
4. **Sensor I2C:** the bus is probably I2C2, which Linux leaves with no devices **[R, I]**. Logic-analyser capture on the owner's unit needs opening the frame. Reading the sensor from the A9 at a quiet moment is simpler but risks collisions, so try it only with a stopped pipeline and the sensor powered.

The firmware has no licence text available to us; TI's TSPA covers TI's parts **[R]**. Method 4 is the "cleanest" per the research note, and it is not legal advice. **Ask the owner to confirm methods 2 to 4 are acceptable** (section 13). If someone analyses the stock binary in the private repo (the owner's earlier rule), they must not write the public rewrite.

### 9.4 Quality reference

**DCC:** TI's camera proxy loads tuning files from a directory into firmware at start (`omx_proxy_camera.c`: `DCC_PATH "/data/misc/camera/"`) **[V]**. Files with a `.dcc` name were not found in the system image, though `/data/misc/camera/raw` and `/yuv` paths appear in it **[V, name search only]**. Without these files firmware uses defaults **[R]**.

**So "Google firmware through our client" may not equal "stock camera":** quality may be lower until DCC is found. Check `ls -l /data/misc/camera` on a stock boot (T1). If files exist, load them through the same path. They are blobs to extract, never commit.

**Scene and rig (owner, one afternoon; cost about EUR 100 if a chart is bought):**

- **Mount:** Glass fixed on a head form or small tripod clamp. No movement between runs. Mark positions with tape.
- **Chart at 1 m:** X-Rite ColorChecker Classic (24 patch), printed slanted-edge chart (or SFRplus-style), Siemens star; a grey card.
- **Light:** three repeatable conditions, measured with a lux meter: 500 lux daylight-balanced LED; 100 lux warm indoor; 10 lux low light. Mains flicker at 50 Hz under fluorescent or LED.
- **Motion:** a printed pattern on a turntable at a fixed speed.
- **Step response:** switch light from 100 to 500 lux and back, with an LED in frame giving a time mark.

**Run each scene on:** (a) stock XE24 camera app (the reference), (b) stock firmware via `glass-camd`, (c) each open stage. Keep raw files where available. Take 5 stills, plus 30 s of video.

**Metrics:**

| Metric | Tool | Pass, against stock |
|---|---|---|
| Colour: mean and max ΔE2000 over 24 patches | `colour-science` (BSD-3) | Mean no more than 1.0 worse |
| White balance: grey patch angle error | Same | No more than 1° worse |
| Sharpness: MTF50, centre and corners | Own slanted-edge script | At least 95 % |
| Noise: SNR on grey patches at each light level | Own | No more than 1 dB worse |
| AE: final luminance; time to within 5 % after step | Own, from video | Luminance within 5 %; time no more than 1.25× |
| Flicker: visible banding under 50 Hz | Column-mean FFT | No worse |
| Video quality per bit: VMAF against the same-scene reference; PSNR | VMAF (check its licence file) | No more than 1 point worse at equal bitrate |
| Latency: press to file | Log timestamps | No more than 1.25× |
| Power and thermal: mA, minutes to 300 MHz | Fuel gauge and CPU limit | No worse |

Publish numbers, chart photos and the runs' scripts. A stage that fails a metric stays behind a flag, and stock stays the default. "Same or better" is defined by this table alone. The owner can revise thresholds before S0, not after seeing results.

### 9.5 Stages

| Stage | Goal | Gate | Effort [I] |
|---|---|---|---|
| S0 | Baseline: all scenes on stock and via `glass-camd`; OV5680 observation, HS or GP check | Numbers published | 1-2 weeks |
| S1 | Minimal M3 firmware: resource table, trace, rpmsg echo, idle and suspend. Boots with Google's firmware as fallback | Echo works, suspend reaches same mA | 2-4 weeks. Secure-mode limits on HS are **[U]** |
| S2 | Raw capture on the A9: OV5680 init, CSI-2 to memory, RAW10 frames saved, with M3 doing nothing to the ISS | Bayer frames at 2592x1944 and a video mode, correct colours in an offline demosaic | 4-8 weeks. Register data is the largest risk |
| S3 | ISP from TRM: black level, LSC, CFA, gamma, RGB2YUV, resizer to NV12 in TILER | Static-scene metrics near stock | 6-12 weeks |
| S4 | 3A: H3A statistics, AE, AWB, flicker; tuning on the rig | All still metrics pass | 4-8 weeks |
| S5 | M3 firmware with IPUMM and TI encoder: encoder on OMAP4 proven, NV12 from S3 | 720p30 encode at stock quality per bit | 4-8 weeks. If TI's codec fails on this silicon, fall back to software at lower resolution |
| S6 | Replace Google's firmware by default, including decode, sleep and recovery | Full section 9.4 table, and section 6.3 | 2-4 weeks |

**Mainline** is a separate decision. Stages S2 to S4, written to the omap4iss structure, carry over with small changes **[I]**. If mainline works before S2 starts, do S2 there and skip a 3.4 backport.

A note on 3.4: full `omap4iss` needs a newer V4L2 than 3.4 **[R]**. A small purpose-built 3.4 driver for capture to memory with a simple ioctl may be less work than a backport **[I]**.

## 10. Test plan

Every image: RAM boot only, with the existing watchdog pattern, using `fastboot-guard.sh boot`. Nothing written to flash. No `echo 0 > fb0/blank` loop in init: this undid screen-off in an earlier test **[R]**. Logs go through `/dev/kmsg` with a tag, and survive a warm reboot in `/proc/last_kmsg` **[R]**.

### 10.1 M1: one-photo proof, adb-free

**Image.** Current Model B RAM image (our kernel, Alpine rootfs), plus:

- Local firmware file in `/lib/firmware`, chosen for the unit's RAM. Built locally, never published.
- Static `glass-camera`, `glass-fb`, and a small `glass-photo` wrapper with `libjpeg-turbo` or a single-file encoder.
- Early firmware answer in init.
- `android_usb` gadget in RNDIS mode, static address `192.168.42.1` (`CONFIG_USB_G_ANDROID=y` and `f_rndis` in `android.c` **[V]**; RNDIS used in `glass-xec-mainline` **[R]**).
- `glass-photo` serves the file itself over TCP, about 20 lines of C. The current image has no `nc` link **[V]**. No `adbd`. The image makes no flash write.

**Init steps, each logged as `OMGB-CAM`:**

1. Mount, `mdev`, answer firmware. Log the time to `remote processor ipu_c0 is now up`, and channel list.
2. Log `ls -l /dev/rpmsg-omx1 /dev/ion`, `ipu_c0` state.
3. Wait for a camera button press, or 60 s, to allow framing. The prism shows "Press camera to take a photo" through the launcher or a simple text frame.
4. Run `glass-camera -i` (capabilities, to the log).
5. Run `glass-camera -y -w 1280 -h 720 -r 15 -n 30 > /run/p.nv12`. Keep the last frame. If this fails, retry with `-w 960 -h 540`, which is the known-good size.
6. Log the frame count, and `trace1` to kmsg.
7. Encode to `/run/photo.jpg`. Log size and SHA-256.
8. Decimate to 640x360 NV12, the size `glass-fb -n` is used with **[V]**, and show it on the prism for 20 s.
9. Serve the JPEG on RNDIS port 8000 for 3 minutes, then `reboot -f`. The watchdog forces a reboot at 10 minutes in any case.

**Owner steps:**

```sh
# Host, Glass in fastboot (camera button plus power, as already documented)
./fastboot-guard.sh -s <serial> boot out/modelb-camera-m1-boot.img
# Press the camera button when the prism asks, aiming at something recognisable
# Wait for the photo on the prism. Then on the host:
sudo ip addr add 192.168.42.2/24 dev <rndis-interface>
nc 192.168.42.1 8000 > photo.jpg
sha256sum photo.jpg     # compare with the OMGB-CAM value on the prism or log
```

**Success, all of:**

- The firmware is up within 15 s of boot, not 69 s.
- A recognisable, correctly coloured picture appears on the prism, at the correct orientation.
- The host's JPEG opens, with the matching SHA-256.
- `trace1` has no exception.
- The system does not reset.

**Fallback if USB fails:** a base64 thumbnail (under 200 KB) in kmsg, read later from `last_kmsg`, plus the prism. The log buffer is `CONFIG_LOG_BUF_SHIFT=21` **[V]**, so keep it well under 2 MB and throttled. That read happens on another boot and is not part of pass or fail. The prism is the direct proof.

**What failure means:**

| Symptom | Likely cause | Next step |
|---|---|---|
| No `ipu_c0 is now up` | Firmware file or the early answer | `list` mode of the firmware tool, log |
| `ioctl` ENOTTY | Header or config mismatch | Compare sizes; this would contradict section 7.3 |
| Up, but `GetHandle` fails | Firmware or memory map | `trace1` |
| Executing, no frames | Operating mode not set | Known cause **[R]**; check it was set |
| Green or black frames | Format or stride; sensor power | `trace1`, buffer dump, resmgr logging |

### 10.2 M3: photo by button

Mock tests first: extend `glassd --mock` and the launcher's script-driven tests. A fake provider covers states, refusal and reconnect. Add `glass-domx` unit tests with a fake rpmsg peer.

On the device:

1. Press 10 times with about 5 s between. 10 JPEGs, 10 cards, correct order.
2. Press while off the head: nothing happens and a log line says why.
3. Rapid double press: one or two photos with no failure.
4. Kill the launcher during capture: the file still saves.
5. Kill `glass-camd` in each state: it restarts, and the next photo works (F3).

**Success:** press-to-picture time reported; no leak in `/sys/kernel/debug/ion/*` after 10 shots.

### 10.3 M5: video

1. 30 s at 960x540, 15 fps. The result plays on the host, with a frame count within ±1 % of expected and correct duration.
2. Same for 1280x720 at 30 fps and 1920x1080 at 30 fps. Record whether real time holds.
3. Pull power (`reboot -f`) mid-recording. The file plays up to within 2 s of the end.
4. 5 minutes at the default, from cold and from warm. Log CPU limit, battery temperature, dropped frames, and mA.
5. Run with the viewfinder and without, to separate its cost.
6. Stop by each route in rule 5.

**Success:** no frame drop above 1 % with viewfinder on at the thermal cap; or the rules reduce viewfinder rate and hold recording. Target from section 5.9 met, or defaults changed.

### 10.4 M6: audio

1. With stock image: capture mixer, DAPM and PCM state during audio capture.
2. Model B: capture 10 s to WAV, check level with speech at 30 cm against the stock recording. Target within 3 dB.
3. Clap test in video: A/V offset under 40 ms at start and after 5 minutes.
4. Bone-conduction feedback during recording: check the amount in the recording.

### 10.5 Failure modes and recovery

| ID | Failure | Injection | Expected result |
|---|---|---|---|
| F1 | Firmware crash during capture | Debugfs recovery control, or stop the known decoder-crash sequence on purpose | `FAILED` reason, file safe, camera back within 5 s; 3 crashes in 5 min disables it |
| F2 | Encoder or camera stall | Stop feeding output buffers | Watchdog in `glass-camd` (no frames for 2 s) stops the session with a reason |
| F3 | `glass-camd` killed with SIGKILL | `kill -9` in each state | Wake lock gone; next session works or a recovery runs; report whether firmware cleans up |
| F4 | ION or TILER exhausted | Allocate from a test client first | Clean refusal, no crash |
| F5 | Firmware file missing or late | Remove from image; mount after 60 s | Camera reported unavailable; the rest of the UI unaffected |
| F6 | Storage full or read-only | Small tmpfs | Refuses to start; running recording stops cleanly |
| F7 | Button during suspend entry and resume | Press at various moments, in a loop | Never a stuck "busy"; the press is honoured or visibly ignored |
| F8 | Overheat | Warm chamber or a blanket, supervised | Stops with "too warm"; no reset |
| F9 | Low battery | Battery at 10 % and 5 % | Block, then clean stop; file valid |
| F10 | Launcher or glassd exit during recording | Kill each | glassd gone: recording stops within 2 s; launcher gone: recording goes on, indicator stays in overlay |
| F11 | Wrong firmware for RAM size | Install the other file | Hash and size check at install prevents it; if forced, boot log shows failure and UI reports unavailable |

### 10.6 M8: sync and offline

`omg-protocol` already has `MediaItem`, `MediaList`, `MediaServer`, `MediaChunkRequest`, `MediaChunk`, `MediaDelete`, `MediaDeleted` **[V]**. `docs/media-sync.md` describes the flow for Glass XE **[V]**. On Model B glassd has no media handling yet **[V, by search]**.

Design: reuse those messages, with `glassd` as the link holder and `glass-camd`'s index as the list source. Offline queue: the index is the queue, with `synced: false` items being pending. Sync resumes by byte offset. Attachments reference `media_id` from a note. Add a note message to `omg-protocol` when the note format is settled. Bluetooth moves thumbnails and short clips (48 KiB chunks, already defined), Wi-Fi on demand for full files, following the locked companion decision. The docs' rule "auto sync only while charging on Wi-Fi" should be reviewed for Model B: it predates that decision **[I]**.

**Tests:**

1. Take 5 photos with no phone, link later: all arrive, verified by SHA-256.
2. Cut the link mid-transfer: it resumes from the offset.
3. Delete on phone with delete-after-sync: removed only with a matching id.
4. 1-minute video over Bluetooth: record real throughput.

### 10.7 M9: playback

1. Software decode at 640x360 proxy and 720p. Record fps and CPU at the cap.
2. If not real time at 720p, use the proxy or a static first frame with a hint to view on the phone.
3. Hardware decoder investigation (section 3.6), one change per run, on a known-good own stream. Ducati crashes are expected, and recovery is proven **[R]**.

## 11. Pre-work needing a device session (owner approved only)

| ID | What | Why |
|---|---|---|
| T1 | On a stock boot: `ls -l /data/misc/camera`; read `omap_types`-style identity sysfs (HS or GP); record partition names; toggle `notleboard::status1` and `status2` with the owner looking | DCC, chip type, storage, LED |
| T2 | **Optional, owner to decide.** On Android with our kernel, log rpmsg-omx packets in the kernel driver while stock Camera and a video play run. Interface observation only; no binary is read | Exact still sequence and decoder setup on this firmware |
| T3 | Stock reference capture for section 9.4 scenes, with the rig | Quality baseline |
| T4 | Stock audio capture state | Section 5.8 |

## 12. Third-party material and licences

| Item | Licence, as read | Adoption |
|---|---|---|
| `glass-xec-mainline` scripts and tools (`glass-camera`, `glass-fb`, `glass-firmware`, `domx.h`) | MIT, 2026 Lasimeri **[V, LICENSE]** | **Adopt**, keeping the notice |
| OMX headers in `tools/glass-camera/omx` | Khronos (MIT-style) and TI BSD-style **[V, file headers]** | **Adopt** |
| AOSP `hardware/ti/omap4xxx` | Apache-2.0 and BSD (`MODULE_LICENSE_BSD_APL2`; camera files Apache-2.0) **[V]** | Read as reference. Copy only with notice |
| Linux kernel, `omap4iss` | GPL-2.0 **[V, repo]** | Kernel-side use only |
| XDAIS `xdm.h` | TI BSD **[V]** | Reference |
| sysbios-rpmsg, TI IPC, IPUMM source | BSD-3 **[R]** | Stage S1, S5 |
| TI IVA-HD codec binaries | TI text licence (TSPA): redistribution unmodified with notices; TI devices only; **no reverse engineering** **[V]** | Stage S5 only. Fetch at install unless the owner decides to ship them under its terms |
| Google Ducati firmware | **No licence text available** **[U]** | Extract from the user's own device only. Never redistribute |
| `tmlind/droid4-pm`, MobiAqua `ti-ducati-fw`, `fast-openISP` | No licence file **[R]** | Reference only, no copying |
| `libjpeg-turbo` | IJG, BSD-3, zlib **[from memory, verify at adoption]** | Proposed for A9 JPEG |
| `libopus` | BSD-3 **[from memory, verify]** | Proposed for audio |
| FFmpeg `libavcodec` | LGPL-2.1+ for the decoder configuration **[from memory, verify]** | Proposed for P1 |
| `openh264` | BSD-2 **[from memory, verify]** | Alternative for P1 |
| libcamera | LGPL-2.1+ library, parts differ **[R, verify per file]** | S4 |
| `colour-science` | BSD-3 **[R]** | Benchmarks |
| VMAF | GitHub reports NOASSERTION **[R]**: read the licence file first | Benchmarks |

No TI or Google binary was disassembled in this work. Searches of the factory system image were limited to file names. The factory kernel (GPL) was decompressed and searched for strings.

## 13. Questions for the owner

1. **Storage:** where does persistent media go (section 5.7)? Recommended: a directory on userdata. It also decides how a flashed Model B fits, given the 8 MiB boot partition.
2. **Privacy:** is the lit prism plus an unmutable sound enough, if no LED exists? Is 5 minutes per clip the right limit?
3. **Observation methods** (section 9.3 methods 2 to 4, and T2): acceptable under your clean-room rule?
4. **Audio codec:** Opus, with transcoding on the phone where needed, or AAC with its licensing?
5. **Where the open camera runs:** A9, with M3 only running codecs (section 9.1)? This departs from "rewrite the firmware".
6. **Quality thresholds** in section 9.4: accept or change them before baseline capture.
7. **Reference rig:** buy a ColorChecker (about EUR 100), or start with a printed chart and accept weaker colour numbers?
8. **Button off the head:** ignore it as designed, or allow it?

## 14. What could not be determined

- OV5680: registers, I2C bus and address, CSI-2 lanes, modes beyond those the firmware reports, and lens data. No public documentation found.
- Whether an OMX client works on 3.4.113. Everything points to yes, but this has never been run.
- Still port on this firmware, and hardware JPEG's location.
- Whether the camera's preview and video ports run together here, and whether Main or High profile is accepted.
- How the firmware requests sensor and ISS resources: no `rpmsg-resmgr` channel was logged.
- Power draw of the camera and encoder; resume latency; thermal time at 720p30.
- Whether the thermal cap affects M3 or IVA-HD.
- Whether a retail unit has a visible LED on GPIO 7 or 8.
- Working microphone route, gains and positions.
- What the Tesla firmware does (not opened), and whether a 2 GB Tesla image exists under another name.
- HS or GP silicon (local evidence suggests HS **[R]**), and any resulting limit on our own firmware.
- Which public commit, if any, matches the stock kernel `g1cee760235d`.
- Whether DCC files exist on stock, and how much stock quality depends on them.
- The licence covering Google's firmware as a whole.
- Whether TI's IVA-HD encoder binaries run on OMAP4430 outside Google's build.
