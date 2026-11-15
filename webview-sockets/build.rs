//! Declares the cfgs this crate uses.
//!
//! `wasm_browser` is set for `wasm32-unknown-unknown` style targets, like in
//! iroh. `wasm_native_udp` is set by the user through `RUSTFLAGS`.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(wasm_browser)");
    println!("cargo::rustc-check-cfg=cfg(wasm_native_udp)");
    let family = std::env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if family.split(',').any(|f| f == "wasm") && os == "unknown" {
        println!("cargo::rustc-cfg=wasm_browser");
    }
}
