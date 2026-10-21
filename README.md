# glass-services

Clean-room Glass system services for Google Glass Explorer Edition: touchpad gestures, head gestures (look up to wake, nod), on-head and wink detection through the glasshub, and the screen wake and sleep policy. Written in Rust, with no Android dependency, so the same daemon runs on an open Android build and on plain Linux. Part of [OpenMyGlass](https://github.com/OpenMyGlass).

| Document | Content |
|---|---|
| [`docs/api.md`](docs/api.md) | The interface between `glassd` and clients (launcher, apps, Android glue) |
| [`proto/glass/services/v1/services.proto`](proto/glass/services/v1/services.proto) | Its messages |
| [`docs/plans/`](docs/plans/) | Plans and decisions |
| `docs/specs/` | Facts: kernel interfaces (from the GPL kernel source) and clean-room specs of the stock components (added by their own PRs) |

## Clean room

The stock Glass libraries are studied by a separate reverse-engineering agent, who writes only the specs in `docs/specs/`. The code here is written from those specs, the GPL kernel source and public Glass documentation, by people and agents who did not see the decompiled code. No Google file, and nothing derived by copying from one, is in this repository.

License: Apache-2.0 (see `LICENSE`).
