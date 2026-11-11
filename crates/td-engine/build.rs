//! Compiles the private strategies in when their repo is checked out.
//!
//! The private repo `trading-desk-strategies` is cloned to `src/strategies/private/`, which git
//! ignores. Each `*.rs` file in it is one strategy in the same format as the examples in
//! `src/strategies/`. This script writes `$OUT_DIR/private_strategies.rs`, which declares a module
//! per file and lists their entries; `src/strategies/mod.rs` includes it and the registry adds the
//! entries after the examples. Without the folder the list is empty, so nothing tracked changes
//! either way.
//!
//! It also writes `$OUT_DIR/fixture_strategies.rs` from `tests/fixtures/private_strategies/`, which
//! the crate's tests compile in the same way.
//!
//! Cargo reruns this script when anything under `src/strategies/` changes. That folder always
//! exists, so cloning or deleting `private/` inside it is noticed too; watching a folder that may
//! not exist would rerun the script, and rebuild the crate, on every build.

#[path = "build/private_strategies.rs"]
mod private_strategies;

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=src/strategies");
    println!("cargo:rerun-if-changed=tests/fixtures/private_strategies");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets it"));
    for (dir, file) in [
        (
            manifest.join("src").join("strategies").join("private"),
            "private_strategies.rs",
        ),
        (
            manifest
                .join("tests")
                .join("fixtures")
                .join("private_strategies"),
            "fixture_strategies.rs",
        ),
    ] {
        let source = private_strategies::generate(&dir).unwrap_or_else(|e| panic!("{e}"));
        let path = out.join(file);
        // Unchanged source is left alone, so its timestamp does not trigger a rebuild.
        if fs::read_to_string(&path).ok().as_deref() != Some(source.as_str()) {
            fs::write(&path, source).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
    }
}
