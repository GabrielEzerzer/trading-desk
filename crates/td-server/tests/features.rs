//! features.json at the repo root maps every binary and example cargo metadata lists, its clap
//! subcommands, the server's routes, the app's sections, the GUI's package scripts and every
//! standalone script, and every source and test file it names exists.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

struct Target {
    name: String,
    package: String,
    src: PathBuf,
    dir: PathBuf,
    example: bool,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

fn features() -> Vec<Value> {
    let map: Value =
        serde_json::from_str(&read(&root().join("features.json"))).expect("features.json is JSON");
    map["features"]
        .as_array()
        .expect("features.json has a features array")
        .clone()
}

fn field<'a>(feature: &'a Value, key: &str) -> &'a str {
    match feature[key].as_str() {
        Some(text) if !text.trim().is_empty() => text,
        _ => panic!("{feature} has no {key}"),
    }
}

fn targets() -> Vec<Target> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ])
        .arg("--manifest-path")
        .arg(root().join("Cargo.toml"))
        .output()
        .expect("cargo metadata runs");
    assert!(out.status.success(), "cargo metadata failed: {out:?}");
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata prints JSON");
    let mut found = Vec::new();
    for package in meta["packages"].as_array().expect("a packages array") {
        let manifest = Path::new(package["manifest_path"].as_str().expect("a manifest path"));
        for target in package["targets"].as_array().expect("a targets array") {
            let kinds = target["kind"].as_array().expect("a target kind");
            let example = kinds.iter().any(|k| k == "example");
            if example || kinds.iter().any(|k| k == "bin") {
                found.push(Target {
                    name: target["name"].as_str().expect("a target name").into(),
                    package: package["name"].as_str().expect("a package name").into(),
                    src: target["src_path"].as_str().expect("a source path").into(),
                    dir: manifest.parent().expect("a package folder").into(),
                    example,
                });
            }
        }
    }
    assert!(
        found.iter().any(|t| !t.example),
        "cargo metadata listed no binaries"
    );
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("a source folder") {
        let path = entry.expect("a source entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn kebab(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('-');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn subcommands(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    rust_files(&dir.join("src"), &mut files);
    let mut names = Vec::new();
    for file in files {
        let text = read(&file);
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            if !(line.contains("derive(") && line.contains("Subcommand")) {
                continue;
            }
            lines.find(|l| l.contains("enum "));
            for l in lines.by_ref().take_while(|l| *l != "}") {
                let variant = l.strip_prefix("    ").unwrap_or("");
                if variant.starts_with(|c: char| c.is_ascii_uppercase()) {
                    let end = variant
                        .find(|c: char| !c.is_ascii_alphanumeric())
                        .unwrap_or(variant.len());
                    names.push(kebab(&variant[..end]));
                }
            }
        }
    }
    names
}

fn routes() -> Vec<String> {
    let text = read(&root().join("crates/td-server/src/api/mod.rs"));
    let mut found = Vec::new();
    for piece in text.split(".route(").skip(1) {
        let piece = piece.split(".fallback(").next().unwrap_or(piece);
        let path = piece.split('"').nth(1).expect("a route path");
        for method in ["get", "post", "put", "patch", "delete"] {
            let call = format!("{method}(");
            let called = piece.match_indices(&call).any(|(i, _)| {
                i > 0 && matches!(piece.as_bytes()[i - 1], b' ' | b'.' | b'(' | b'\n')
            });
            if called {
                found.push(format!("{} {path}", method.to_ascii_uppercase()));
            }
        }
    }
    found
}

fn sections() -> Vec<String> {
    let text = read(&root().join("gui/app/src/renderer/src/SectionToggle.tsx"));
    text.split("{ id: '")
        .skip(1)
        .map(|rest| rest.split('\'').next().expect("a section id").to_string())
        .collect()
}

fn package_scripts() -> Vec<(String, String)> {
    let package: Value = serde_json::from_str(&read(&root().join("gui/package.json")))
        .expect("gui/package.json is JSON");
    package["scripts"]
        .as_object()
        .expect("gui/package.json has scripts")
        .iter()
        .map(|(name, command)| (name.clone(), command.as_str().unwrap_or("").to_string()))
        .collect()
}

fn files_in(dir: &str, extension: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root().join(dir)).expect("a script folder") {
        let path = entry.expect("a script entry").path();
        if path.is_file() && path.extension().is_some_and(|e| e == extension) {
            let name = path.file_name().expect("a file name").to_string_lossy();
            found.push((format!("{dir}/{name}"), read(&path)));
        }
    }
    found
}

fn standalone_scripts() -> Vec<String> {
    let mut found = Vec::new();
    let node: Vec<(String, String)> = ["gui/scripts", "gui/app/mock"]
        .into_iter()
        .flat_map(|dir| files_in(dir, "mjs"))
        .collect();
    let run_by_package: String = package_scripts().into_iter().map(|(_, c)| c).collect();
    for (path, _) in &node {
        let file = path.rsplit('/').next().expect("a file name");
        let imported = node
            .iter()
            .any(|(_, text)| text.contains(&format!("./{file}")));
        if !file.contains(".test.") && !imported && !run_by_package.contains(file) {
            found.push(path.clone());
        }
    }
    for entry in fs::read_dir(root().join("harness")).expect("the harness folder") {
        let dir = entry.expect("a harness entry").path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir.file_name().expect("a folder name").to_string_lossy();
        for (path, text) in files_in(&format!("harness/{name}"), "py") {
            if text.contains("if __name__ == \"__main__\":") {
                found.push(path);
            }
        }
    }
    found
}

fn discovered() -> BTreeSet<String> {
    let mut entries = BTreeSet::new();
    for target in targets() {
        if !target.example && target.name == target.package {
            for sub in subcommands(&target.dir) {
                entries.insert(format!("{} {sub}", target.name));
            }
        }
        entries.insert(target.name);
    }
    entries.extend(routes());
    entries.extend(sections().into_iter().map(|s| format!("gui:{s}")));
    entries.extend(
        package_scripts()
            .into_iter()
            .map(|(n, _)| format!("pnpm {n}")),
    );
    entries.extend(standalone_scripts());
    entries
}

#[test]
fn every_entry_point_is_in_the_map() {
    let found = discovered();
    for expected in [
        "td-data download",
        "td-live start",
        "td-bench",
        "demo-smoke",
        "crosscheck",
        "POST /sessions",
        "gui:trading",
        "pnpm dev",
        "gui/scripts/quit-check.mjs",
        "gui/app/mock/generate.mjs",
        "harness/crosscheck/run.py",
    ] {
        assert!(found.contains(expected), "discovery missed {expected}");
    }
    let mapped: BTreeSet<String> = features()
        .iter()
        .map(|f| field(f, "entry").to_string())
        .collect();
    let missing: Vec<&String> = found.difference(&mapped).collect();
    assert!(missing.is_empty(), "not in features.json: {missing:?}");
}

#[test]
fn every_subcommand_belongs_to_a_binary_named_after_its_package() {
    let targets = targets();
    for target in &targets {
        if !subcommands(&target.dir).is_empty() {
            assert!(
                targets
                    .iter()
                    .any(|t| !t.example && t.name == target.package && t.package == target.package),
                "{} defines subcommands but builds no binary named after it",
                target.package
            );
        }
    }
}

#[test]
fn every_mapped_entry_has_its_code_and_its_test() {
    let found = discovered();
    let targets = targets();
    let sections = sections();
    let mut entries = BTreeSet::new();
    let mut names = BTreeSet::new();
    for f in features() {
        let entry = field(&f, "entry");
        assert!(entries.insert(entry.to_string()), "{entry} is mapped twice");
        assert!(
            names.insert(field(&f, "name").to_string()),
            "{f} reuses a name"
        );
        for key in ["description", "inputs", "outputs"] {
            field(&f, key);
        }
        let source = root().join(field(&f, "source"));
        assert!(
            source.is_file(),
            "{entry}: {} does not exist",
            source.display()
        );
        if let Some(target) = targets.iter().find(|t| t.name == entry) {
            assert!(
                same_file(&source, &target.src),
                "{entry}: source is not its target's main file"
            );
        }
        match field(&f, "kind") {
            "cli" => {
                let binary = entry.split(' ').next().unwrap_or("");
                assert!(
                    targets.iter().any(|t| !t.example && t.name == binary),
                    "{entry} does not start with a binary"
                );
                assert!(found.contains(entry), "{entry} is not in the code")
            }
            "http" | "script" => {
                assert!(found.contains(entry), "{entry} is not in the code")
            }
            "example" => assert!(
                targets.iter().any(|t| t.example && t.name == entry),
                "{entry} is not an example"
            ),
            "internal" => {
                assert!(found.contains(entry), "{entry} is not in the code");
                field(&f, "reason");
            }
            "gui" => {
                let section = entry
                    .strip_prefix("gui:")
                    .and_then(|rest| rest.split('/').next())
                    .unwrap_or("");
                assert!(
                    sections.iter().any(|s| s == section),
                    "{entry} is not an app section"
                );
            }
            kind => panic!("{entry} has an unknown kind {kind}"),
        }
        match field(&f, "test") {
            "none" => assert!(
                f["issue"].as_u64().is_some_and(|n| n > 0),
                "{entry} has no test and no issue number"
            ),
            workflow if workflow.starts_with(".github/workflows/") => {
                let text = read(&root().join(workflow));
                let runs = text.match_indices(entry).any(|(i, _)| {
                    text[i + entry.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| c.is_whitespace() || c == '"' || c == '\'')
                });
                assert!(runs, "{workflow} never runs {entry}");
            }
            test => assert!(
                root().join(test).is_file(),
                "{entry}: {test} does not exist"
            ),
        }
    }
}
