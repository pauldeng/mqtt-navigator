//! Linux only: the window icon and the desktop entry.
//!
//! - X11 takes the window icon from `WindowOptions.icon` (`window_icon`).
//! - Wayland takes only the app id. The shell then looks for a desktop entry with that name and
//!   shows its icon. The `.deb` installs this entry. For the AppImage and the bare binary, the app
//!   adds a user entry on its first start (`ensure_user_entry`).
//!
//! The entry text is `assets/linux/<app id>.desktop`. The packager installs the same file.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use navigator_update::Install;
use navigator_update::swap::Kind;

use crate::APP_ID;

const ENTRY_FILE: &str = "io.github.pauldeng.mqtt-navigator.desktop";
const ENTRY_TEMPLATE: &str =
    include_str!("../../../assets/linux/io.github.pauldeng.mqtt-navigator.desktop");

const ICON_256: &[u8] = include_bytes!("../../../assets/icon/icon-256.png");

/// The sizes that the user entry installs. The icon theme scales the rest.
const ICONS: [(u32, &[u8]); 4] = [
    (48, include_bytes!("../../../assets/icon/icon-48.png")),
    (128, include_bytes!("../../../assets/icon/icon-128.png")),
    (256, ICON_256),
    (512, include_bytes!("../../../assets/icon/icon-512.png")),
];

/// The X11 window icon (`_NET_WM_ICON`). Decoding 256 x 256 takes about 1 ms. Call it off the UI
/// thread. `None` if the embedded file is bad.
pub fn window_icon() -> Option<Arc<image::RgbaImage>> {
    image::load_from_memory_with_format(ICON_256, image::ImageFormat::Png)
        .ok()
        .map(|img| Arc::new(img.into_rgba8()))
}

/// Adds the user desktop entry and icons if the system has no entry for this app. It is
/// best effort: every failure is ignored. Blocking: call it off the UI thread.
pub fn ensure_user_entry() {
    let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
    else {
        return;
    };
    let Some(exec) = launcher_path() else {
        return;
    };
    let system = system_data_dirs(std::env::var_os("XDG_DATA_DIRS"));
    if matches!(install(&data_home, &system, &exec), Ok(true)) {
        // The shell watches the folders. These tools only refresh caches for other programs.
        let apps = data_home.join("applications");
        run("update-desktop-database", &[apps.as_os_str()]);
        let theme = data_home.join("icons/hicolor");
        run(
            "gtk-update-icon-cache",
            &["-f".as_ref(), "-t".as_ref(), theme.as_os_str()],
        );
    }
}

/// What the entry starts: the AppImage file, else the launcher next to the app, else the app.
fn launcher_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    match Install::locate(&exe, std::env::var_os("APPIMAGE")) {
        Some(i) if i.kind == Kind::AppImage => Some(i.path),
        Some(i) => Some(i.launcher),
        None => Some(exe),
    }
}

fn system_data_dirs(var: Option<OsString>) -> Vec<PathBuf> {
    match var.filter(|v| !v.is_empty()) {
        Some(v) => std::env::split_paths(&v).collect(),
        None => vec!["/usr/local/share".into(), "/usr/share".into()],
    }
}

fn run(program: &str, args: &[&std::ffi::OsStr]) {
    let _ = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `Ok(true)`: files written or removed. `Ok(false)`: nothing to do.
fn install(data_home: &Path, system: &[PathBuf], exec: &Path) -> io::Result<bool> {
    let entry = data_home.join("applications").join(ENTRY_FILE);
    // The package owns the entry. The user folder is searched first, so an entry that this app
    // wrote earlier would shadow the package: remove it. Never touch an entry we did not write.
    if system
        .iter()
        .any(|d| d.join("applications").join(ENTRY_FILE).exists())
    {
        if std::fs::read_to_string(&entry).is_ok_and(|t| is_ours(&t)) {
            std::fs::remove_file(&entry)?;
            // Our icons are also searched first. Remove them, so they do not hide the package icons.
            for (size, _) in ICONS {
                let _ = std::fs::remove_file(icon_path(data_home, size));
            }
            return Ok(true);
        }
        return Ok(false);
    }
    match std::fs::read_to_string(&entry) {
        Ok(text) => {
            // Repair only an entry that this app wrote, when its program is gone.
            let stale = is_ours(&text) && exec_target(&text).is_some_and(|t| !program_exists(&t));
            if !stale {
                return Ok(false);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let Some(text) = render(exec) else {
        return Ok(false);
    };
    // Icons first, so that the shell never sees the entry without its icon.
    for (size, png) in ICONS {
        write_file(&icon_path(data_home, size), png)?;
    }
    write_file(&entry, text.as_bytes())?;
    Ok(true)
}

fn icon_path(data_home: &Path, size: u32) -> PathBuf {
    data_home.join(format!("icons/hicolor/{size}x{size}/apps/{APP_ID}.png"))
}

/// The template with `Exec` set to `exec`. `None`: the path cannot be in an entry.
fn render(exec: &Path) -> Option<String> {
    let exec = quote_exec(exec.to_str()?)?;
    let mut out = String::new();
    for line in ENTRY_TEMPLATE.lines() {
        if line.starts_with("Exec=") {
            out.push_str("Exec=");
            out.push_str(&exec);
            out.push('\n');
            // The mark that this app wrote the entry: only such an entry is repaired or removed.
            out.push_str(MARK);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    Some(out)
}

/// Desktop Entry spec, "The Exec key": quote an argument that has reserved characters. Inside
/// quotes, `"`, `` ` ``, `$` and `\` need a backslash, and a string value doubles each backslash.
/// `%` is refused: GLib looks up the program before it turns `%%` into `%`, so no entry works.
fn quote_exec(path: &str) -> Option<String> {
    if path.chars().any(|c| c.is_control() || c == '%') {
        return None;
    }
    let reserved = " \t\n\"'\\><~|&;$*?#()`";
    if !path.chars().any(|c| reserved.contains(c)) {
        return Some(path.to_owned());
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    Some(out)
}

/// The program of the first `Exec=` line, without quotes (the reverse of `quote_exec`).
fn exec_target(entry: &str) -> Option<PathBuf> {
    let value = entry.lines().find_map(|l| l.strip_prefix("Exec="))?;
    // Step 1: the string value escape `\\` stands for one backslash.
    let mut plain = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some('\\')) => {
                chars.next();
                plain.push('\\');
            }
            _ => plain.push(c),
        }
    }
    // Step 2: the first argument of the Exec key.
    let mut out = String::new();
    let mut chars = plain.chars();
    if plain.starts_with('"') {
        chars.next();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => out.push(chars.next()?),
                c => out.push(c),
            }
        }
    } else {
        out.extend(chars.take_while(|c| !c.is_whitespace()));
    }
    (!out.is_empty()).then(|| PathBuf::from(out))
}

/// The key that marks an entry written by this app (an `X-` key is allowed by the spec).
const MARK: &str = "X-MQTT-Navigator-Generated=true";

/// The entry was written by this app (it has the mark).
fn is_ours(entry: &str) -> bool {
    entry.lines().any(|l| l.trim() == MARK)
}

/// The program of an `Exec` line exists: an absolute path is checked as is, a bare name is
/// searched on `PATH` (as the desktop does).
fn program_exists(target: &Path) -> bool {
    if target.is_absolute() {
        return target.exists();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(target).is_file()))
}

/// Writes through a temp file and a rename, so a reader never sees half a file. Normal modes
/// (0644): these files are not secrets.
fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_decodes_to_256() {
        let icon = window_icon().expect("embedded icon decodes");
        assert_eq!((icon.width(), icon.height()), (256, 256));
    }

    #[test]
    fn exec_round_trip() {
        for p in [
            "/usr/bin/mqtt-navigator-launcher",
            "/home/a b/My Apps/MQTT $x\"y`z\\w.AppImage",
        ] {
            let text = render(Path::new(p)).expect("renders");
            assert!(text.contains("StartupWMClass=io.github.pauldeng.mqtt-navigator\n"));
            assert_eq!(exec_target(&text), Some(PathBuf::from(p)), "{text}");
        }
        assert!(render(Path::new("/a\nb")).is_none());
        assert!(render(Path::new("/a/100%.AppImage")).is_none());
    }

    #[test]
    fn template_matches_app_id() {
        assert_eq!(ENTRY_FILE, format!("{APP_ID}.desktop"));
        assert!(ENTRY_TEMPLATE.contains(&format!("Icon={APP_ID}\n")));
    }

    #[test]
    fn writes_once_and_repairs_only_stale_own_entry() {
        let home = tempfile::tempdir().expect("tempdir");
        let sys = tempfile::tempdir().expect("tempdir");
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let launcher = bin.join("mqtt-navigator-launcher");
        std::fs::write(&launcher, b"").expect("write");
        let system = [sys.path().to_path_buf()];

        assert!(install(home.path(), &system, &launcher).expect("first"));
        let entry = home.path().join("applications").join(ENTRY_FILE);
        assert!(
            std::fs::read_to_string(&entry)
                .expect("read")
                .contains(launcher.to_str().expect("utf8"))
        );
        for size in [48, 128, 256, 512] {
            let png = home
                .path()
                .join(format!("icons/hicolor/{size}x{size}/apps/{APP_ID}.png"));
            assert!(png.is_file(), "{png:?}");
        }
        // A valid entry stays.
        assert!(!install(home.path(), &system, &launcher).expect("second"));

        // Our entry that starts a missing program is repaired.
        std::fs::remove_file(&launcher).expect("rm");
        let moved = bin.join("MQTT.AppImage");
        std::fs::write(&moved, b"").expect("write");
        assert!(install(home.path(), &system, &moved).expect("repair"));
        assert!(
            std::fs::read_to_string(&entry)
                .expect("read")
                .contains("MQTT.AppImage")
        );

        // An entry without our mark is never changed, even for a missing AppImage.
        std::fs::write(&entry, "[Desktop Entry]\nExec=/opt/other/Other.AppImage\n").expect("write");
        assert!(!install(home.path(), &system, &moved).expect("foreign"));
        assert!(
            std::fs::read_to_string(&entry)
                .expect("read")
                .contains("Other.AppImage")
        );

        // A bare program name found on PATH is not stale.
        assert!(program_exists(Path::new("sh")));
        assert!(!program_exists(Path::new("no-such-program-mqtt-navigator")));

        // A system entry wins: our earlier entry is removed so it does not shadow the package.
        assert!(install(home.path(), &[], &moved).is_ok());
        std::fs::remove_file(&entry).expect("rm");
        assert!(install(home.path(), &[], &moved).expect("ours again"));
        assert!(is_ours(&std::fs::read_to_string(&entry).expect("read")));
        let apps = sys.path().join("applications");
        std::fs::create_dir_all(&apps).expect("mkdir");
        std::fs::write(apps.join(ENTRY_FILE), "x").expect("write");
        assert!(icon_path(home.path(), 48).exists());
        assert!(install(home.path(), &system, &moved).expect("system"));
        assert!(!entry.exists());
        assert!(!icon_path(home.path(), 48).exists());
        assert!(!install(home.path(), &system, &moved).expect("system, nothing left"));

        // A foreign user entry stays even when a system entry exists.
        std::fs::write(&entry, "[Desktop Entry]\nExec=/opt/custom/run\n").expect("write");
        assert!(!install(home.path(), &system, &moved).expect("system, foreign"));
        assert!(entry.exists());
    }
}
