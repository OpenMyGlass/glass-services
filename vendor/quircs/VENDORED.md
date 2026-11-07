# quircs, vendored

The crates.io release 0.10.3 (MIT in its metadata; the LICENSE file is quirc's ISC licence), copied
whole, without its examples, profiles and CI files. glass-qr uses it through `[patch.crates-io]` in
the workspace `Cargo.toml`.

One change, in `find_alignment_pattern` (`src/identify.rs`, marked "OpenMyGlass"): the estimate of a
module's area is computed in i128 and the search ends when it is larger than a module of a 21-module
code filling the image. Before it, wrong finder patterns could make one frame take seconds: the
spiral search runs about 100 steps per pixel of that estimate. `glass-qr`'s `slow_frames` test holds
frames that did.

Not sent upstream: that is the owner's decision.
