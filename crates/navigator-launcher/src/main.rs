//! The program that the user starts (ADR 0005). It has no dependencies. It applies a staged
//! update, keeps one previous copy, rolls back a version that never became healthy, and starts
//! the app (the payload) with the same arguments. All file work is in `swap.rs`, which the
//! app shares.
//!
//! Windows: the launcher uses the GUI subsystem, so it shows no console window.
#![cfg_attr(windows, windows_subsystem = "windows")]
#![forbid(unsafe_code)]

#[path = "../../navigator-update/src/swap.rs"]
#[allow(dead_code)] // The app uses the parts that the launcher does not.
mod swap;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn log(lines: &[String]) {
    let Some(dir) = swap::log_dir() else { return };
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("launcher.log"))
    {
        for line in lines {
            let _ = writeln!(f, "{secs} launcher: {line}");
        }
    }
}

/// Starts `program`. Unix: replaces this process. Windows: starts it and returns.
fn start(program: &PathBuf, args: &[OsString]) -> std::io::Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(cmd.exec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: the payload is a console program.
        cmd.spawn().map(|_| ())
    }
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("The launcher cannot find its own file.");
        std::process::exit(1);
    };
    let install = swap::Install::locate(&exe, std::env::var_os("APPIMAGE"));
    let dir = swap::update_dir();
    let plan = match (&install, &dir) {
        (Some(i), Some(d)) => swap::prepare(i, d, &mut |_| false).ok(),
        _ => None,
    };
    let (mut program, lines) = plan.map_or_else(
        || {
            let sibling = exe.with_file_name(swap::PAYLOAD);
            (sibling, Vec::new())
        },
        |p| (p.exec, p.log),
    );
    log(&lines);
    if let Err(e) = start(&program, &args) {
        let mut lines = vec![format!("cannot start {}: {e}", program.display())];
        // A swapped copy that does not start: put the previous copy back and try it.
        if let (Some(i), Some(d)) = (&install, &dir)
            && let Some(back) = swap::start_failed(i, d)
        {
            lines.push("rolled back after a failed start".into());
            program = back;
            log(&lines);
            if start(&program, &args).is_ok() {
                return;
            }
            lines = vec![format!("cannot start {}", program.display())];
        }
        log(&lines);
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
}
