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
//! the crate's tests compile in the same way, and `$OUT_DIR/example_hashes.rs`, the hash of each
//! example strategy's file. The private and fixture lists carry their files' hashes too; the
//! registry gives each strategy's with `code_hash`, and saved runs and verdicts record it.
//!
//! Cargo reruns this script when anything under `src/strategies/` changes. That folder always
//! exists, so cloning or deleting `private/` inside it is noticed too; watching a folder that may
//! not exist would rerun the script, and rebuild the crate, on every build.

#[path = "build/private_strategies.rs"]
mod private_strategies;

use std::path::{Path, PathBuf};
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
        write_if_changed(&out.join(file), &source);
    }
    let examples = private_strategies::example_hashes(&manifest.join("src").join("strategies"))
        .unwrap_or_else(|e| panic!("{e}"));
    write_if_changed(&out.join("example_hashes.rs"), &examples);
}

/// Unchanged source is left alone, so its timestamp does not trigger a rebuild.
fn write_if_changed(path: &Path, source: &str) {
    if fs::read_to_string(path).ok().as_deref() != Some(source) {
        fs::write(path, source).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}
