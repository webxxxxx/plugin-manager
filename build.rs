//! Select the compile-time memory profile.
//!
//! `MEM_LEVEL=low|middle|high` controls the hard ceilings in `src/limits.rs`.
//! Default is `high`, preserving desktop behaviour.
//!
//! The target triple is compiled into `package::target()`; keep that even when
//! the memory level is not set.

use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use flate2::{Compression, GzBuilder};

fn main() {
    let target = env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=SVCD_TARGET={target}");

    println!("cargo:rerun-if-env-changed=MEM_LEVEL");
    println!("cargo:rustc-check-cfg=cfg(mem_low)");
    println!("cargo:rustc-check-cfg=cfg(mem_middle)");
    println!("cargo:rustc-check-cfg=cfg(mem_high)");

    match env::var("MEM_LEVEL").as_deref() {
        Ok("low") => println!("cargo:rustc-cfg=mem_low"),
        Ok("middle") => println!("cargo:rustc-cfg=mem_middle"),
        Ok("high") | Ok("") | Err(_) => println!("cargo:rustc-cfg=mem_high"),
        Ok(other) => panic!("MEM_LEVEL must be one of low|middle|high, got {other:?}"),
    }

    // The console is embedded as gzip streams so the browser gets ~12 KiB of
    // HTML instead of the ~46 KiB source. The sources stay the single readable
    // artifact; this step only runs at build time. A fixed mtime and OS byte
    // keep the embedded bytes deterministic across machines and targets.
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets manifest dir"));
    let console_dir =
        PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("console");
    fs::create_dir_all(&console_dir).expect("create console asset directory");
    for name in ["index.html", "favicon.svg"] {
        let relative = format!("src/assets/static/{name}");
        println!("cargo:rerun-if-changed={relative}");
        let source = manifest.join(&relative);
        let bytes =
            fs::read(&source).unwrap_or_else(|error| panic!("read {}: {error}", source.display()));
        let file = File::create(console_dir.join(format!("{name}.gz")))
            .expect("create compressed console asset");
        let mut encoder = GzBuilder::new()
            .mtime(0)
            .operating_system(255)
            .write(file, Compression::best());
        encoder
            .write_all(&bytes)
            .expect("write compressed console asset");
        encoder.finish().expect("finish compressed console asset");
    }
}
