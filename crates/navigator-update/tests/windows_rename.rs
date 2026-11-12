//! Windows: the update swap renames the running exe to `.prev` and puts the new exe at the old
//! path (ADR 0005). This test proves that Windows allows it.
#![cfg(windows)]

use std::fs;
use std::process::{Command, Stdio};

#[test]
fn a_running_exe_can_be_renamed_and_its_path_reused() {
    let tmp = tempfile::tempdir().expect("dir");
    let exe = tmp.path().join("mqtt-navigator.exe");
    let system = std::env::var_os("SystemRoot").expect("SystemRoot is set on Windows");
    fs::copy(std::path::Path::new(&system).join("System32/cmd.exe"), &exe).expect("copy cmd.exe");
    // The copy runs `ping`, so it stays alive for about 30 seconds.
    let mut child = Command::new(&exe)
        .args(["/c", "ping", "-n", "30", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the copy");
    let prev = tmp.path().join("mqtt-navigator.exe.prev");
    let renamed = fs::rename(&exe, &prev);
    // Write the new file at the old path while the old file still runs.
    let wrote = renamed.is_ok().then(|| fs::write(&exe, "new"));
    let still_running = child.try_wait().expect("status").is_none();
    let _ = child.kill();
    let _ = child.wait();
    renamed.expect("rename of the running exe");
    wrote
        .expect("renamed")
        .expect("write a new file at the old path");
    assert!(still_running, "the copy must run while it is renamed");
    assert!(prev.is_file());
    assert_eq!(fs::read_to_string(&exe).expect("new file"), "new");
}
