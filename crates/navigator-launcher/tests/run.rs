//! Runs the real launcher binary with shell scripts as payloads (Unix only).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn script(path: &Path, label: &str) {
    // The payload writes its label and arguments to the file that $MARK names.
    fs::write(
        path,
        format!("#!/bin/sh\necho \"{label} $*\" >> \"$MARK\"\n"),
    )
    .expect("script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[test]
fn the_launcher_applies_an_update_starts_it_and_rolls_it_back() {
    let tmp = tempfile::tempdir().expect("dir");
    let root = fs::canonicalize(tmp.path()).expect("path");
    let app = root.join("app");
    fs::create_dir(&app).expect("app");
    let launcher = app.join("mqtt-navigator-launcher");
    fs::copy(env!("CARGO_BIN_EXE_mqtt-navigator-launcher"), &launcher).expect("copy");
    let payload = app.join("mqtt-navigator");
    script(&payload, "old");

    // The update folder as the launcher computes it.
    let data = if cfg!(target_os = "macos") {
        root.join("Library/Application Support")
    } else {
        root.join("xdg")
    };
    let dir = data.join("mqtt-navigator/update");
    let staging = dir.join("staging");
    fs::create_dir_all(&staging).expect("staging");
    script(&staging.join("payload"), "new");
    fs::write(staging.join("version"), "2.0.0").expect("version");
    fs::write(staging.join("for"), payload.to_str().expect("utf8")).expect("for");
    fs::rename(&staging, dir.join("staged")).expect("staged");

    let mark = root.join("mark");
    let run = |args: &[&str]| {
        let out = Command::new(&launcher)
            .args(args)
            .env("HOME", &root)
            .env("XDG_DATA_HOME", root.join("xdg"))
            .env("MARK", &mark)
            .env_remove("APPIMAGE")
            .output()
            .expect("run");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        fs::read_to_string(&mark).unwrap_or_default()
    };

    // First start: the update is applied, and the new payload gets the arguments.
    assert_eq!(run(&["--host", "b"]), "new --host b\n");
    assert!(app.join("mqtt-navigator.prev").exists());
    // The new version did not write a healthy marker: the next start rolls it back.
    assert_eq!(run(&["x"]), "new --host b\nold x\n");
    assert!(!app.join("mqtt-navigator.prev").exists());
    assert_eq!(
        fs::read_to_string(dir.join("failed")).expect("failed"),
        "2.0.0"
    );
    // The launcher wrote its log lines.
    let log = fs::read_to_string(data.join("mqtt-navigator/logs/launcher.log")).expect("log");
    assert!(log.contains("rolled back"), "{log}");
}
