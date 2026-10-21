# glass-services

Clean-room Glass system services for Google Glass Explorer Edition: touchpad gestures, head gestures (look up to wake, nudge), on-head and wink detection through the glasshub, and the screen wake and sleep policy. Written in Rust, with no Android dependency, so the same daemon runs on an open Android build and on plain Linux. Part of [OpenMyGlass](https://github.com/OpenMyGlass).

| Document | Content |
|---|---|
| [`docs/api.md`](docs/api.md) | The interface between `glassd` and clients (launcher, apps, Android glue) |
| [`proto/glass/services/v1/services.proto`](proto/glass/services/v1/services.proto) | Its messages |
| [`docs/plans/`](docs/plans/) | Plans and decisions |
| `docs/specs/` | Facts: kernel interfaces (from the GPL kernel source) and clean-room specs of the stock components (added by their own PRs) |

## Crates

| Crate | Content |
|---|---|
| `crates/glass-api` | Types generated from the `.proto` at build time (prost-build + protox, no `protoc`), the framing codec, a blocking `Client`, the session rules as a pure `Hub`, and the in-process `MockService` for tests of clients |
| `crates/glass-gesture` | Touchpad recognizer (MT protocol A and B, tap, long press, swipes, scroll, screen-off encoded frames); pure, no I/O |
| `crates/glass-evdev` | Minimal evdev over `libc`: `input_event` parsing, `EVIOCGNAME`, `EVIOCGABS`, `EVIOCSCLOCKID` |
| `crates/glass-head` | Head gestures from the MPU-9150 through IIO: the buffer reader, look-up and look-down from gravity, our nudge detector, relative look-up, the inhibition and forwarding rules; pure except the device I/O |
| `crates/glass-hub` | glasshub over the kernel driver's sysfs (on-head, wink, thresholds; an allow-list of writable attributes) and the pure on-head state machine |
| `crates/glassd` | The daemon: `poll` loop, socket server, screen policy and back ends (`FBIOBLANK`, touchpad `sleepmode`), buttons, stored settings, `--mock` |

## Build and run

```sh
cargo test --workspace
cargo run -p glassd -- --mock --socket /tmp/glassd.sock -v     # then type: tap, swipe forward, wait 500, ...
cargo run -p glassd -- --device-name sensor00fn11              # on a Glass (default device name)
```

On a Glass, `glassd` also opens the glasshub (`--glasshub`, default `/sys/bus/i2c/devices/4-0035`),
the IMU (`--iio`, the IIO device named `mpu9150`), the camera and power buttons (found by name, or
`--camera-device` / `--power-device`), the touchpad's
`sleepmode` (`--touchpad-sysfs`), and blanks the panel with `FBIOBLANK` (`--display fb[:path]`, or
`--display log`). Settings are stored in `--state-dir` (default `$GLASSD_STATE_DIR`, else
`/data/glassd` on Android, `/var/lib/glassd` elsewhere). `glassd --help` lists every option.

Cross builds (as in CI): `cargo build --release --target armv7-linux-androideabi` with the NDK's
`armv7a-linux-androideabi22-clang` as linker, and `cargo zigbuild --release --target
armv7-unknown-linux-musleabihf` for a static Linux binary.

## Dependencies and licences

Every dependency is under a licence compatible with Apache-2.0.

| Crate | Licence | Use |
|---|---|---|
| `prost`, `prost-derive` | Apache-2.0 | protobuf runtime (linked) |
| `bytes` | MIT | used by `prost` (linked) |
| `libc` | MIT OR Apache-2.0 | system calls (linked) |
| `prost-build`, `prost-types` | Apache-2.0 | code generation (build only) |
| `protox`, `protox-parse`, `prost-reflect` | MIT OR Apache-2.0 | `.proto` compiler in Rust (build only) |
| `miette`, `miette-derive` | Apache-2.0 | build only |
| `anyhow`, `bitflags`, `cfg-if`, `either`, `equivalent`, `errno`, `fastrand`, `fixedbitset`, `fnv`, `hashbrown`, `heck`, `indexmap`, `itertools`, `log`, `logos`, `logos-codegen`, `logos-derive`, `multimap`, `once_cell`, `petgraph`, `prettyplease`, `proc-macro2`, `quote`, `regex`, `regex-automata`, `regex-syntax`, `syn`, `tempfile`, `thiserror`, `thiserror-impl`, `unicode-width`, `windows-link`, `windows-sys` | MIT OR Apache-2.0 | build only |
| `rustix`, `linux-raw-sys` | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | build only |
| `aho-corasick`, `memchr` | Unlicense OR MIT | build only |
| `foldhash` | Zlib | build only |
| `unicode-ident` | (MIT OR Apache-2.0) AND Unicode-3.0 | build only |

## Clean room

The stock Glass libraries are studied by a separate reverse-engineering agent, who writes only the specs in `docs/specs/`. The code here is written from those specs, the GPL kernel source and public Glass documentation, by people and agents who did not see the decompiled code. No Google file, and nothing derived by copying from one, is in this repository.

License: Apache-2.0 (see `LICENSE`).
