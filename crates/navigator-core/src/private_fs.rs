//! Private files and folders. The settings, the logs and the crash files can hold host names
//! and user names. Only the owner may read them.
//!
//! - Unix: new folders get mode 0700 and new files get mode 0600, whatever the umask is. The
//!   umask can only remove more bits. [`tighten_dir`] also makes an existing app folder 0700.
//! - Windows: no change. New files in the user profile (`%APPDATA%`, `%LOCALAPPDATA%`) inherit
//!   the access list of the profile, and other users have no access to it.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;

/// Like `create_dir_all`. On Unix, each folder that this call creates gets mode 0700. An
/// existing folder keeps its mode.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// Sets mode 0700 on an existing folder of this app if group or others can use it. Call it
/// only for a folder that the app owns (for example `~/.config/mqtt-navigator`), never for a
/// shared one like `/tmp`. It does nothing on Windows.
pub fn tighten_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if std::fs::metadata(dir)?.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Makes `options` create a file with mode 0600 on Unix. An existing file keeps its mode.
pub fn private_file(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).expect("metadata").mode() & 0o777
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mqtt-nav-fs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn new_dirs_are_0700_and_new_files_are_0600() {
        let root = temp("new");
        let dir = root.join("a").join("b");
        create_dir(&dir).expect("create");
        assert_eq!((mode(&root), mode(&dir)), (0o700, 0o700));
        let file = dir.join("f");
        private_file(OpenOptions::new().write(true).create_new(true))
            .open(&file)
            .expect("open");
        assert_eq!(mode(&file), 0o600);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn create_dir_keeps_an_existing_dir_and_tighten_changes_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp("old");
        std::fs::create_dir_all(&dir).expect("create");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        create_dir(&dir).expect("create");
        assert_eq!(mode(&dir), 0o755);
        tighten_dir(&dir).expect("tighten");
        assert_eq!(mode(&dir), 0o700);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
