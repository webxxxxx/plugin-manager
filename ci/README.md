# Size gate

The core binary has a hard budget: **2,000,000 B embedded**, **30,000,000 B desktop**
(2026-11-12, from the volume-budget ticket). This directory holds the gate
configuration; the workflow is .github/workflows/size.yml.

## What is measured

- The single multicall binary (the three entry points are hardlinks to one file).
- Stripped, uncompressed, decimal bytes via stat / os.path.getsize.
- Compiled-in resources count; installed unit/config files and plugins do not.
- A hardlink is one inode, so it is counted once.

## Thresholds and failure

| Check | Semantics |
| --- | --- |
| size > 2,000,000 B (embedded) | **fail** the job |
| size > 30,000,000 B (desktop) | **fail** the job |
| size > baseline x 1.02 | warning annotation (does not block) |

The baseline lives in ci/size-baseline.json. Update it deliberately when growth
is intended:

    python tools/measure_size.py <binary> --target <triple> --update-baseline

## Where and when

- **PR / push to main**: ubuntu builds x86_64/aarch64/armv7 musl (cargo-zigbuild
  0.24.1 + Zig 0.17.1), windows builds x86_64-msvc.
- **Nightly**: adds aarch64-apple-darwin.
- Artifacts are kept 14 days.

## Local run

    cargo build --release --target x86_64-unknown-linux-musl
    python tools/measure_size.py target/x86_64-unknown-linux-musl/release/svcd \
      --target x86_64-unknown-linux-musl --limit 2000000

Offline builds work when the crates are cached: add --offline.

Release builds (the platform matrix, archive layout and how to cut a tag):
see release.md in this directory.
