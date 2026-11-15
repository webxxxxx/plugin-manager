//! Select the compile-time memory profile.
//!
//! `MEM_LEVEL=low|middle|high` controls the hard ceilings in `src/limits.rs`.
//! Default is `high`, preserving desktop behaviour.
//!
//! The target triple is compiled into `package::target()`; keep that even when
//! the memory level is not set.

use std::env;

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
}
