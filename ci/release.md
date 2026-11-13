# Release builds

Publishing is tag-driven: pushing `v<version>` runs `.github/workflows/release.yml`,
which calls `.github/workflows/build.yml` (the same workflow that runs on pull
requests and main), then creates a GitHub release from the built archives.

`build.yml` is the single source of truth for the matrix: PR CI and a release
always build exactly the same targets with the same toolchain pins.

## Matrix

| Platform | Target | Linked | Archive |
| --- | --- | --- | --- |
| Linux embedded | `x86_64-unknown-linux-musl` | static (static-pie) | `.tar.gz` |
| Linux embedded | `aarch64-unknown-linux-musl` | static ET_EXEC | `.tar.gz` |
| Linux embedded | `armv7-unknown-linux-musleabihf` | static ET_EXEC | `.tar.gz` |
| Linux desktop | `x86_64-unknown-linux-gnu` | dynamic, glibc 2.35 floor | `.tar.gz` |
| Linux desktop | `aarch64-unknown-linux-gnu` | dynamic, glibc 2.35 floor | `.tar.gz` |
| Windows | `x86_64-pc-windows-msvc` | MSVC | `.zip` |
| macOS | `universal2-apple-darwin` (arm64 + x86_64) | SDK 11.0 | `.tar.gz` |

Pins are deliberate; bump them together after testing:

- Rust **1.99.1** (`dtolnay/rust-toolchain@1.99.1`)
- cargo-zigbuild **0.24.1** (must stay ≥ 0.23 on Rust ≥ 1.98)
- Zig **0.17.1**
- Runners: `ubuntu-24.04`, `windows-2025`, `macos-26` (never `-latest`)

The gnu targets pass `--target <triple>.2.35` to cargo-zigbuild and the job
asserts with `readelf` that no `GLIBC_*` symbol above 2.35 is required. Ubuntu
22.04 ships glibc 2.35, so these binaries are the desktop baseline.

## Router targets

The two musl ARM builds are for routers whose firmware already runs static
ELFs. CI asserts the exact shape, not just "it compiled":

| Artifact | Expected `file` output |
| --- | --- |
| `aarch64-unknown-linux-musl` | `ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, ... stripped` |
| `armv7-unknown-linux-musleabihf` | `ELF 32-bit LSB executable, ARM, EABI5 version 1 (SYSV), statically linked, ... stripped` |

Both must be **ET_EXEC** ("statically linked"), not static-pie: rustc enables
static-pie for `x86_64-unknown-linux-musl`, but not for these two targets, and
the router firmware format is the ET_EXEC one. The job also checks the machine
string and that the binary is stripped, then runs the binary under
`qemu-aarch64-static` / `qemu-arm-static` (`qemu-user-static`) and expects the
multicall usage exit code 64. That catches an ABI/CPU-feature regression before
a router ever sees the file.

`armv7-unknown-linux-musleabihf` needs an ARMv7-A CPU with VFP/EABI5 hard-float;
the target is built for `+v7,+thumb2,+vfp3d16`. Static musl does not depend on
the router's libc, only on the Linux kernel (Rust's stated minimum is 3.2).

Note that `file` cannot distinguish ARMv5 / ARMv6 / ARMv7 or hard vs soft float:
the ARM ELF `e_flags` only carries EABI5. On the router, check `uname -m`
(`armv7l` vs `armv6l`) and `grep -m1 Features /proc/cpuinfo` for `vfp`; if the
router is ARMv6/ARMv5 or soft-float, the armv7 hard-float artifact will SIGILL
and a separate target has to be added.

## Archive layout

Each archive extracts to `svcd-<version>-<target>/`:

    svcd[.exe]          the multicall binary
    svc[.exe]           hardlink (copy in the zip) to svcd
    plugin[.exe]        hardlink (copy in the zip) to svcd
    mcp[.exe]           hardlink (copy in the zip) to svcd
    LICENSE
    svcd.conf.example

The tag release also publishes `SHA256SUMS` over every archive. Archives are
deterministic: `tools/package_release.py` fixes entry metadata (mtime, uid/gid,
modes) so rebuilding the same binary produces the same bytes. The Unix
hardlinks are understood by GNU tar, bsdtar and BusyBox tar (OpenWrt's `tar`),
so the archive extracts directly on the routers. They are **not** code-signed;
Windows/macOS signing and notarization are part of #42.

## Cutting a release

1. Bump `version` in `Cargo.toml` (and `Cargo.lock` via `cargo check`).
2. Open a PR and merge to `main`; `build` runs the full matrix.
3. Push the tag:

        git tag v0.2.0
        git push origin v0.2.0

4. `release.yml` fails if the tag is not `v` + the version in `Cargo.toml`
   (pre-release versions such as `v0.2.0-rc.1` work when `Cargo.toml` carries the
   same pre-release version, and are published as GitHub pre-releases), then
   publishes.

Re-running a failed publish job is safe: an existing release gets its assets
re-uploaded with `--clobber`. To dry-run the matrix without tagging, run the
**build** workflow from the Actions tab (`workflow_dispatch`); it uploads the
archives as run artifacts but does not create a release.

If the publish job is denied, the repository's default workflow token is
read-only; the job already requests `contents: write`, so check that org/repo
policy does not cap it.

## Building one target locally

    cargo zigbuild --release --target x86_64-unknown-linux-musl
    python3 tools/package_release.py \
      --binary target/x86_64-unknown-linux-musl/release/svcd \
      --target x86_64-unknown-linux-musl \
      --version 0.1.0 \
      --extra LICENSE --extra svcd.conf.example \
      --outdir dist

On Windows use `cargo build --release --target x86_64-pc-windows-msvc` and
`python`, on macOS build `aarch64-apple-darwin` + `x86_64-apple-darwin` and
`lipo -create` them first. `tools/package_release.py` picks `.zip` whenever the
target name contains `windows`, `.tar.gz` otherwise.

## Deliberately out of scope

- **Installers and signing** (deb/rpm, MSI, pkg + notarization) belong to #42,
  which is blocked on the WebView work (#41).
- **Windows arm64** is not built; there is no mainstream server/desktop target
  for it yet and the MSVC ARM64 toolchain is not guaranteed on the runner.
- **Embedded ipk/apk** belongs to #43.
