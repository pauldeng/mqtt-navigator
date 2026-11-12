//! Which kind of install this is, and whether it can update itself.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use crate::swap::{Install, Kind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateMode {
    SelfUpdate(Install),
    /// The app shows a notice and a link. It never writes. `reason` is for the user.
    NoticeOnly {
        reason: String,
    },
}

fn notice(reason: &str) -> UpdateMode {
    UpdateMode::NoticeOnly {
        reason: reason.into(),
    }
}

/// Blocking: it reads the file system.
pub fn detect() -> UpdateMode {
    match std::env::current_exe() {
        Ok(exe) => detect_from(&exe, std::env::var_os("APPIMAGE")),
        Err(_) => notice("The app cannot find its own file."),
    }
}

/// `exe` is the running program. `appimage` is the value of `$APPIMAGE`.
pub fn detect_from(exe: &Path, appimage: Option<OsString>) -> UpdateMode {
    let is_appimage = appimage.as_ref().is_some_and(|a| !a.is_empty());
    if cfg!(target_os = "linux") && !is_appimage && (exe.starts_with("/usr") || dpkg_owns(exe)) {
        return notice(
            "This copy belongs to the package manager. Update it with the package manager.",
        );
    }
    let Some(install) = Install::locate(exe, appimage) else {
        return notice("This is a development build. It does not update itself.");
    };
    if install.kind == Kind::App && install.path.starts_with("/Volumes") {
        return notice("Copy this app to Applications, then start it from there.");
    }
    // The swap renames next to the install. Try to create a file there: read-only flags lie.
    let parent = install.path.parent().unwrap_or(&install.path);
    let probe = parent.join(format!(".mqtt-navigator-write-test-{}", std::process::id()));
    if fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|_| fs::remove_file(&probe))
        .is_err()
    {
        return notice("The folder of this app is read-only for you.");
    }
    UpdateMode::SelfUpdate(install)
}

/// A `.list` file of dpkg names this path. Only files of our package names are read.
fn dpkg_owns(exe: &Path) -> bool {
    let Ok(entries) = fs::read_dir("/var/lib/dpkg/info") else {
        return false;
    };
    let want = exe.to_string_lossy();
    entries.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        name.starts_with("mqtt-navigator")
            && name.ends_with(".list")
            && fs::read_to_string(e.path()).is_ok_and(|t| t.lines().any(|l| l == want))
    })
}
