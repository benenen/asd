//! Build the webview's vendor JS bundle with npm + esbuild.
//!
//! `package.json` owns the dependency list and the bundling command
//! (`npm run build` → esbuild → `dist/vendor.js`, one minified IIFE that puts
//! each dependency on `window` — see `assets/vendor-entry.js`). This script only
//! drives npm and copies the result into `OUT_DIR` for `include_str!`; adding
//! or upgrading npm dependencies never requires touching it.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=package.json");
    println!("cargo:rerun-if-changed=package-lock.json");
    println!("cargo:rerun-if-changed=assets/vendor-entry.js");

    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR must be set"));

    run(
        Command::new("npm")
            .arg("install")
            .current_dir(&manifest_dir),
        "npm install (node/npm are required to build asd-dioxus; \
         configure a registry mirror via .npmrc if the default is unreachable)",
    );
    run(
        Command::new("npm")
            .args(["run", "build"])
            .current_dir(&manifest_dir),
        "npm run build (esbuild vendor bundle)",
    );

    let bundle = manifest_dir.join("dist/vendor.js");
    std::fs::copy(&bundle, out_dir.join("vendor.js"))
        .unwrap_or_else(|e| panic!("copying {}: {e}", bundle.display()));
}

fn run(command: &mut Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|e| panic!("failed to execute {context}: {e}"));
    assert!(status.success(), "{context} failed with status {status}");
}
