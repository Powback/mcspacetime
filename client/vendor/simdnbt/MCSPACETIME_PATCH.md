# vendored simdnbt 0.6.1

Copied verbatim from crates.io (azalea 0.10.3 depends on simdnbt ^0.6) with one change:
`is_plain_ascii` in src/mutf8.rs no longer uses `slice::array_chunks`, a nightly-only API that
was removed from Rust in 2025, so the crate compiles on a 2026 toolchain. `#![feature(array_chunks)]`
was dropped from src/lib.rs accordingly. Nothing else was touched.
