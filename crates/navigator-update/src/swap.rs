//! The on-disk update protocol. It uses only `std`. The app runs [`prepare`] at the very start of
//! `main()` (see `navigator_update::startup`). The app also uses this file for staging and for
//! the health marker.
//!
//! Version 0.1.0 had a separate launcher program that ran the same protocol. A 0.1.0 Windows
//! install keeps that launcher as the shortcut target, and it runs its own `prepare` first.
//! So the update folder format must stay as 0.1.0 reads it. A test (`swap_compat.rs`) runs the
//! 0.1.0 code against this code.
//!
//! Update folder `<data_local_dir>/mqtt-navigator/update/`:
//!
//! | Name | Meaning |
//! |---|---|
//! | `staging/` | A download in progress. It never becomes `staged` before the hash matched. |
//! | `staged/payload` | The verified new file (AppImage, exe) or `.app` folder. |
//! | `staged/version`, `staged/for` | Its version, and the install path that it is meant for. |
//! | `state` | `swapping V`, `swapped V`, `running V` or `started V`. No file: no update. |
//! | `healthy` | The version of the app that opened a window. |
//! | `failed` | The last version that was rolled back. The app does not offer it again, and `prepare` never applies a staged update of that version. |
//!
//! State words, and what [`prepare`] does with each at the start:
//!
//! | State | Written by | Action at the start |
//! |---|---|---|
//! | none | | Apply a matching `staged/` update. Start it. |
//! | `swapping V` | the swap | The swap was interrupted. Repair it, finish it, and start the install. |
//! | `swapped V` | the swap (and the 0.1.0 launcher, AppImage only) | First start of V. Write `running V`. Go on. |
//! | `running V` | the first start of V | `healthy` is V: the trial ends. Else: roll back, and start the old copy. |
//! | `started V` | the 0.1.0 launcher only | It owns the trial. Never roll back. `healthy` is V: end it. Else: leave it. |

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const PAYLOAD: &str = if cfg!(windows) {
    "mqtt-navigator.exe"
} else {
    "mqtt-navigator"
};

/// The same folder as `dirs::data_local_dir()` (a test checks it), without the `dirs` crate.
pub fn data_local_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    #[cfg(windows)]
    return var("LOCALAPPDATA").filter(|p| p.is_absolute());
    #[cfg(target_os = "macos")]
    return var("HOME").map(|h| h.join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    return var("XDG_DATA_HOME")
        .filter(|p| p.is_absolute())
        .or_else(|| var("HOME").map(|h| h.join(".local/share")));
}

pub fn update_dir() -> Option<PathBuf> {
    Some(data_local_dir()?.join("mqtt-navigator").join("update"))
}

/// The folder of the app log (the same as `logging::log_dir` of the app).
pub fn log_dir() -> Option<PathBuf> {
    Some(data_local_dir()?.join("mqtt-navigator").join("logs"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A Linux AppImage file.
    AppImage,
    /// A macOS `.app` folder.
    App,
    /// The exe itself (Windows, or a Linux folder).
    Exe,
}

/// What an update replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub kind: Kind,
    /// The thing that the swap replaces: the AppImage file, the `.app` folder or the exe.
    pub path: PathBuf,
}

impl Install {
    /// Finds the install from the path of the running program.
    /// `None`: a development build (the exe is in a cargo `target` folder), or a `Contents/MacOS`
    /// folder that is not in an `.app`. A development build never updates itself.
    pub fn locate(exe: &Path, appimage: Option<OsString>) -> Option<Self> {
        let dir = exe.parent()?;
        let (kind, path) = if let Some(ai) = appimage.filter(|a| !a.is_empty()) {
            (Kind::AppImage, PathBuf::from(ai))
        } else if in_cargo_target(exe) {
            return None;
        } else if dir.ends_with("Contents/MacOS") {
            let bundle = dir.parent()?.parent()?;
            if bundle.extension()? != "app" {
                return None;
            }
            (Kind::App, bundle.to_path_buf())
        } else {
            (Kind::Exe, exe.to_path_buf())
        };
        Some(Self { kind, path })
    }

    /// The program to start after a swap or a rollback. The old copy may be gone or moved.
    pub fn run_path(&self) -> PathBuf {
        match self.kind {
            Kind::AppImage | Kind::Exe => self.path.clone(),
            Kind::App => self.path.join("Contents/MacOS").join(PAYLOAD),
        }
    }

    /// The previous copy: `<path>.prev`.
    pub fn prev(&self) -> PathBuf {
        suffixed(&self.path, ".prev")
    }

    /// Where a rollback parks the bad copy until the previous copy is in place: `<path>.bad`.
    pub fn aside(&self) -> PathBuf {
        suffixed(&self.path, ".bad")
    }
}

/// Cargo writes `CACHEDIR.TAG` into its `target` folder. A folder above the exe has it: a
/// development build.
fn in_cargo_target(exe: &Path) -> bool {
    exe.ancestors()
        .skip(1)
        .any(|d| d.join("CACHEDIR.TAG").is_file())
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

// ---------------------------------------------------------------------------------------------
// Files of the update folder

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    None,
    /// The swap started. It may be half done.
    Swapping(String),
    /// The swap is done. The new version did not start yet. The 0.1.0 launcher writes it for an
    /// AppImage.
    Swapped(String),
    /// The new version runs its trial. A `healthy` marker of this version ends it.
    Running(String),
    /// A trial that the 0.1.0 launcher started. Only the 0.1.0 launcher rolls it back.
    Started(String),
}

pub fn read_state(dir: &Path) -> State {
    let Ok(text) = fs::read_to_string(dir.join("state")) else {
        return State::None;
    };
    match text.trim().split_once(' ') {
        Some(("swapping", v)) => State::Swapping(v.into()),
        Some(("swapped", v)) => State::Swapped(v.into()),
        Some(("running", v)) => State::Running(v.into()),
        Some(("started", v)) => State::Started(v.into()),
        _ => State::None,
    }
}

pub fn write_state(dir: &Path, state: &State) -> io::Result<()> {
    let text = match state {
        State::None => return clear_state(dir),
        State::Swapping(v) => format!("swapping {v}"),
        State::Swapped(v) => format!("swapped {v}"),
        State::Running(v) => format!("running {v}"),
        State::Started(v) => format!("started {v}"),
    };
    write_atomic(&dir.join("state"), text.as_bytes())
}

pub fn clear_state(dir: &Path) -> io::Result<()> {
    remove_any(&dir.join("state"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// The app calls this (in the background) after its first window opens.
pub fn write_healthy(dir: &Path, version: &str) -> io::Result<()> {
    write_atomic(&dir.join("healthy"), version.as_bytes())
}

pub fn read_healthy(dir: &Path) -> Option<String> {
    Some(fs::read_to_string(dir.join("healthy")).ok()?.trim().into())
}

/// The version that was rolled back last, if any.
pub fn read_failed(dir: &Path) -> Option<String> {
    Some(fs::read_to_string(dir.join("failed")).ok()?.trim().into())
}

pub fn staging_dir(dir: &Path) -> PathBuf {
    dir.join("staging")
}

pub fn staged_dir(dir: &Path) -> PathBuf {
    dir.join("staged")
}

/// A verified update that waits for the next start: its version and the install path it is for.
pub fn read_staged(dir: &Path) -> Option<(String, PathBuf)> {
    let s = staged_dir(dir);
    if fs::symlink_metadata(s.join("payload")).is_err() {
        return None;
    }
    let version = fs::read_to_string(s.join("version"))
        .ok()?
        .trim()
        .to_string();
    let target = fs::read_to_string(s.join("for")).ok()?.trim().into();
    Some((version, target))
}

/// Makes `staging/` the `staged/` update. This is the only way to create `staged`. The caller
/// has put the verified `payload` into `staging/`.
pub fn commit_staged(dir: &Path, version: &str, install: &Path) -> io::Result<()> {
    let staging = staging_dir(dir);
    fs::write(staging.join("version"), version)?;
    fs::write(staging.join("for"), install.to_string_lossy().as_bytes())?;
    let staged = staged_dir(dir);
    remove_any(&staged)?;
    fs::rename(staging, staged)
}

pub fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Copies a file or a folder tree. Symbolic links stay links (a `.app` has some).
pub fn copy_any(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_any(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, meta.permissions())
    } else if meta.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(fs::read_link(src)?, dst);
        #[cfg(not(unix))]
        return Err(io::Error::other("symbolic links are not supported here"));
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}

// ---------------------------------------------------------------------------------------------
// The start steps

/// A point of [`prepare`] where a test can "kill" the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    StateWritten,
    PrevRemoved,
    CurrentMoved,
    StagedMoved,
    /// Rollback: the bad copy was moved to `.bad`. The install path is empty now.
    BadMoved,
    /// Rollback: the previous copy is the install again. `.bad` is still there.
    PrevRestored,
    /// Rollback: `.bad` is deleted. The state still says `running`.
    BadRemoved,
    /// Rollback: the `failed` marker is written. The state still says `running`.
    FailedWritten,
}

fn step(hook: &mut dyn FnMut(Step) -> bool, s: Step) -> Result<(), Killed> {
    if hook(s) { Err(Killed) } else { Ok(()) }
}

/// The hook asked to stop here, like a power loss. Nothing is undone.
#[derive(Debug)]
pub struct Killed;

/// What the program does next.
#[derive(Debug)]
pub struct Plan {
    /// `Some`: start this program with the same arguments, and end this process. `None`: go on
    /// in this process.
    pub exec: Option<PathBuf>,
    /// Lines for the update log.
    pub log: Vec<String>,
}

/// Does all file work of one start, but does not start anything. It finishes or undoes an
/// interrupted swap, ends or rolls back a trial, and applies a staged update.
/// `hook` is called after each step. When it returns `true`, the function stops at once.
pub fn prepare(
    inst: &Install,
    dir: &Path,
    hook: &mut dyn FnMut(Step) -> bool,
) -> Result<Plan, Killed> {
    let mut log = Vec::new();
    match read_state(dir) {
        State::None => {}
        State::Swapped(v) => {
            // The first start of the new version. The next start asks for the health marker.
            let _ = write_state(dir, &State::Running(v.clone()));
            log.push(format!("update {v}: first start"));
            return Ok(Plan { exec: None, log });
        }
        State::Running(v) => {
            if read_healthy(dir).as_deref() == Some(v.as_str()) {
                let _ = clear_state(dir);
                log.push(format!("update {v}: healthy, trial ended"));
            } else {
                let exec = rollback(inst, dir, &v, hook, &mut log)?;
                return Ok(Plan { exec, log });
            }
        }
        State::Started(v) => {
            // The 0.1.0 launcher in front of us owns this trial and rolls it back. Never roll
            // back here. While it runs, do not apply an update: that would delete its `.prev`.
            if read_healthy(dir).as_deref() == Some(v.as_str()) {
                let _ = clear_state(dir);
                log.push(format!("update {v}: healthy, trial ended"));
            } else {
                log.push(format!("update {v}: the 0.1.0 launcher owns this trial"));
                return Ok(Plan { exec: None, log });
            }
        }
        State::Swapping(v) => {
            // Interrupted swap. A missing install: the kill came between the two renames.
            // Either the new copy was complete in `.new` (a copy across volumes, and the
            // staged file is gone), or the previous copy goes back. The install is whole again.
            if !exists(&inst.path) {
                let new = suffixed(&inst.path, ".new");
                if exists(&new) && !exists(&staged_dir(dir).join("payload")) {
                    let _ = fs::rename(new, &inst.path);
                    log.push(format!("update {v}: interrupted swap, new copy placed"));
                } else if exists(&inst.prev()) {
                    let _ = fs::rename(inst.prev(), &inst.path);
                    log.push(format!(
                        "update {v}: interrupted swap, previous copy restored"
                    ));
                }
            }
            if read_staged(dir).is_some() {
                // The new file did not move yet. Start the swap again below.
                let _ = clear_state(dir);
            } else {
                log.push(format!("update {v}: interrupted swap was complete"));
                return Ok(Plan {
                    exec: finish(inst, dir, &v).ok(),
                    log,
                });
            }
        }
    }
    if let Some((v, target)) = read_staged(dir)
        && target == inst.path
    {
        if read_failed(dir).as_deref() == Some(v.as_str()) {
            // This version was rolled back. Never apply it again.
            let _ = remove_any(&staged_dir(dir));
            log.push(format!(
                "update {v}: rolled back before, staged copy dropped"
            ));
        } else if let Some(exec) = swap(inst, dir, &v, hook, &mut log)? {
            return Ok(Plan {
                exec: Some(exec),
                log,
            });
        }
    }
    Ok(Plan { exec: None, log })
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Marks the swap as done (`swapped`). Returns the program to start. It removes `staged/` first
/// and checks that it is gone: a staged copy that stays would be applied again after a
/// rollback. When `staged/` cannot be removed, nothing is written and the caller undoes the swap.
fn finish(inst: &Install, dir: &Path, v: &str) -> io::Result<PathBuf> {
    let staged = staged_dir(dir);
    let removed = remove_any(&staged);
    if exists(&staged) {
        return Err(removed
            .err()
            .unwrap_or_else(|| io::Error::other("still there")));
    }
    let _ = remove_any(&dir.join("healthy"));
    let _ = write_state(dir, &State::Swapped(v.into()));
    Ok(inst.run_path())
}

fn swap(
    inst: &Install,
    dir: &Path,
    v: &str,
    hook: &mut dyn FnMut(Step) -> bool,
    log: &mut Vec<String>,
) -> Result<Option<PathBuf>, Killed> {
    let staged = staged_dir(dir).join("payload");
    let prev = inst.prev();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&staged)
        .status();
    if let Err(e) = write_state(dir, &State::Swapping(v.into())) {
        log.push(format!("update {v}: cannot write the state ({e})"));
        return Ok(None);
    }
    step(hook, Step::StateWritten)?;
    let _ = remove_any(&prev);
    step(hook, Step::PrevRemoved)?;
    if let Err(e) = rename_retry(&inst.path, &prev) {
        log.push(format!("update {v}: cannot move the current copy ({e})"));
        let _ = clear_state(dir);
        return Ok(None);
    }
    step(hook, Step::CurrentMoved)?;
    if let Err(e) = place(&staged, &inst.path) {
        // The staged copy may be half deleted: drop it. The app downloads it again.
        let _ = remove_any(&staged_dir(dir));
        return Ok(undo(
            inst,
            dir,
            v,
            &format!("cannot place the new copy ({e})"),
            log,
        ));
    }
    step(hook, Step::StagedMoved)?;
    match finish(inst, dir, v) {
        Ok(exec) => {
            log.push(format!("update {v}: swapped"));
            Ok(Some(exec))
        }
        Err(e) => Ok(undo(
            inst,
            dir,
            v,
            &format!("cannot remove the staged copy ({e})"),
            log,
        )),
    }
}

/// Puts the previous copy back after a failed swap and ends the swap. Returns `None`.
fn undo(inst: &Install, dir: &Path, v: &str, why: &str, log: &mut Vec<String>) -> Option<PathBuf> {
    match restore_prev(inst, &mut |_| false) {
        Ok(Ok(())) => {
            let _ = clear_state(dir);
            log.push(format!("update {v}: {why}, undone"));
        }
        // The state stays `swapping`: the next start repairs the install.
        _ => log.push(format!("update {v}: {why}, and the undo failed")),
    }
    None
}

/// Windows: the old app may still be closing, so the first rename can fail for a short time.
fn rename_retry(from: &Path, to: &Path) -> io::Result<()> {
    let tries = if cfg!(windows) { 25 } else { 1 };
    let mut result = fs::rename(from, to);
    for _ in 1..tries {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        result = fs::rename(from, to);
    }
    result
}

/// Moves `from` to `to`. When the two are on different volumes, it copies next to `to` first,
/// deletes `from`, and renames, so `to` is never half written and `from` is gone before `to`
/// appears. A kill in between leaves the whole copy in `to.new` (see `prepare`).
fn place(from: &Path, to: &Path) -> io::Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let tmp = suffixed(to, ".new");
    remove_any(&tmp)?;
    if let Err(e) = copy_any(from, &tmp)
        .and_then(|()| remove_any(from))
        .and_then(|()| fs::rename(&tmp, to))
    {
        let _ = remove_any(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Makes the previous copy the install. The bad copy is renamed aside first and deleted last,
/// so a kill (or a failed delete) at any point leaves one whole copy at the install path or
/// both copies on disk: a repeat of this function finishes the job.
fn restore_prev(
    inst: &Install,
    hook: &mut dyn FnMut(Step) -> bool,
) -> Result<io::Result<()>, Killed> {
    let bad = inst.aside();
    // A `.bad` that is left from an earlier kill is junk: the previous copy is still `.prev`.
    if let Err(e) = remove_any(&bad) {
        return Ok(Err(e));
    }
    if exists(&inst.path) {
        if let Err(e) = rename_retry(&inst.path, &bad) {
            return Ok(Err(e));
        }
        step(hook, Step::BadMoved)?;
    }
    if let Err(e) = rename_retry(&inst.prev(), &inst.path) {
        let _ = fs::rename(&bad, &inst.path);
        return Ok(Err(e));
    }
    step(hook, Step::PrevRestored)?;
    let _ = remove_any(&bad);
    step(hook, Step::BadRemoved)?;
    Ok(Ok(()))
}

/// Puts the previous copy back and remembers the failed version. Returns the program to start,
/// or `None` when there is no previous copy. Every step can be repeated after a kill, and the
/// state is cleared last, so the next start continues here.
fn rollback(
    inst: &Install,
    dir: &Path,
    v: &str,
    hook: &mut dyn FnMut(Step) -> bool,
    log: &mut Vec<String>,
) -> Result<Option<PathBuf>, Killed> {
    let mut exec = None;
    if exists(&inst.prev()) {
        match restore_prev(inst, hook)? {
            Ok(()) => {
                log.push(format!("update {v}: did not become healthy, rolled back"));
                exec = Some(inst.run_path());
            }
            Err(e) => {
                log.push(format!("update {v}: rollback failed ({e})"));
                return Ok(None);
            }
        }
    } else {
        // A kill after the restore leaves `.bad` and no `.prev`: the install is the old copy.
        let done = exists(&inst.aside()) && exists(&inst.path);
        let _ = remove_any(&inst.aside());
        if done {
            log.push(format!("update {v}: rollback finished"));
            exec = Some(inst.run_path());
        } else {
            log.push(format!(
                "update {v}: did not become healthy, no previous copy"
            ));
        }
    }
    let _ = write_atomic(&dir.join("failed"), v.as_bytes());
    // A staged copy of the failed version must not come back. A newer one stays.
    if read_staged(dir).is_some_and(|(sv, _)| sv == v) {
        let _ = remove_any(&staged_dir(dir));
    }
    step(hook, Step::FailedWritten)?;
    let _ = clear_state(dir);
    Ok(exec)
}

/// The program of a [`Plan`] did not start after a swap. Undoes the swap. Returns the program
/// to start instead.
pub fn start_failed(inst: &Install, dir: &Path) -> Option<PathBuf> {
    match read_state(dir) {
        State::Swapped(v) => rollback(inst, dir, &v, &mut |_| false, &mut Vec::new())
            .ok()
            .flatten(),
        _ => None,
    }
}
