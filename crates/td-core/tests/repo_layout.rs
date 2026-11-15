//! The repository's directory rule: every tracked file sits under one of the known top-level folders
//! or is one of the known top-level files.

use std::path::Path;
use std::process::Command;

const TOP_LEVEL_FOLDERS: &[&str] = &[
    ".github", "assets", "crates", "gui", "harness", "mock", "setup",
];

const TOP_LEVEL_FILES: &[&str] = &[
    ".gitignore",
    ".lint-baseline.json",
    "CLAUDE.md",
    "Cargo.lock",
    "Cargo.toml",
    "LICENSE",
    "README.md",
    "features.json",
    "hush.toml",
    "rust-toolchain.toml",
];

fn misplaced<'a>(tracked: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    tracked
        .into_iter()
        .filter(|path| match path.split_once('/') {
            Some((folder, _)) => !TOP_LEVEL_FOLDERS.contains(&folder),
            None => !TOP_LEVEL_FILES.contains(path),
        })
        .collect()
}

fn tracked_files() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(&root)
        .output()
        .expect("git must be on PATH to list the tracked files");
    assert!(
        output.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("tracked paths are UTF-8")
}

#[test]
fn every_tracked_file_sits_in_a_known_place() {
    let listing = tracked_files();
    let tracked: Vec<&str> = listing.split('\0').filter(|p| !p.is_empty()).collect();
    assert!(
        tracked.contains(&"Cargo.toml"),
        "git ls-files listed no Cargo.toml"
    );
    let bad = misplaced(tracked);
    assert!(
        bad.is_empty(),
        "these tracked files sit outside the known layout: {bad:?}. Move each under one of the top-level \
         folders {TOP_LEVEL_FOLDERS:?}, or, for a new top-level file the repo needs, add it to \
         TOP_LEVEL_FILES {TOP_LEVEL_FILES:?} (or a new folder to TOP_LEVEL_FOLDERS) in \
         crates/td-core/tests/repo_layout.rs"
    );
}

#[test]
fn misplaced_names_unknown_folders_and_files() {
    let paths = [
        "crates/td-core/src/lib.rs",
        "README.md",
        "notes.md",
        "scripts/run.sh",
        "gui/app/package.json",
        ".github/workflows/ci.yml",
    ];
    assert_eq!(misplaced(paths), vec!["notes.md", "scripts/run.sh"]);
}
