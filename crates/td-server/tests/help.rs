//! Every command and subcommand features.json maps to a binary this package builds prints its
//! help and exits 0. td-data and td-live run this file as their own `help` test.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BINARIES: [(&str, Option<&str>); 4] = [
    ("td-data", option_env!("CARGO_BIN_EXE_td-data")),
    ("td-live", option_env!("CARGO_BIN_EXE_td-live")),
    ("td-server", option_env!("CARGO_BIN_EXE_td-server")),
    ("td-bench", option_env!("CARGO_BIN_EXE_td-bench")),
];

fn help_exits_zero(exe: &str, args: &[&str]) {
    let mut child = Command::new(exe)
        .args(args)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("{exe}: {e}"));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("a child status") {
            assert!(status.success(), "{exe} {args:?} --help exited {status}");
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{exe} {args:?} --help did not exit within 30 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn every_mapped_command_prints_its_help() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../features.json");
    let text = std::fs::read_to_string(path).expect("features.json at the repo root");
    let map: serde_json::Value = serde_json::from_str(&text).expect("features.json is JSON");
    let mut ran = 0;
    for f in map["features"].as_array().expect("a features array") {
        if f["kind"] != "cli" {
            continue;
        }
        let words: Vec<&str> = f["entry"].as_str().expect("an entry").split(' ').collect();
        let exe = BINARIES
            .iter()
            .find(|(name, _)| *name == words[0])
            .and_then(|(_, exe)| *exe);
        if let Some(exe) = exe {
            help_exits_zero(exe, &words[1..]);
            ran += 1;
        }
    }
    assert!(ran > 0, "no mapped command of this package was run");
}
