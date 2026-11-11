//! App settings: profiles, the last profile and the UI state. Stored as JSON with a
//! `schema_version`. The app owns the file path and decides when to save (debounce, thread).

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::profile::Profile;

/// The schema version that this build writes.
pub const CURRENT_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    Messages,
    Tree,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowHeight {
    OneLine,
    #[default]
    TwoLines,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub theme_mode: ThemeMode,
    /// Id of the colour theme preset (see the app's `themes` module). An unknown id means
    /// "default".
    pub theme_preset: String,
    pub view: ViewMode,
    pub row_height: RowHeight,
    pub pane_widths: Vec<f32>,
    pub window_size: Option<(f32, f32)>,
    /// Memory budget of the topic store, in MiB. The default is 128. The file value is clamped to
    /// [`STORE_BUDGET_MIB_MIN`]..=[`STORE_BUDGET_MIB_MAX`] when it loads.
    #[serde(deserialize_with = "clamped_mib")]
    pub store_budget_mib: u32,
    /// Check for a new version once at start (the only network traffic besides the broker). The
    /// palette entry "Check for updates…" works when this is off.
    pub check_updates: bool,
}

/// Any number in the file is accepted and clamped. A wrong value must not make the whole file
/// unreadable.
fn clamped_mib<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = f64::deserialize(d)?;
    Ok(v.clamp(
        f64::from(STORE_BUDGET_MIB_MIN),
        f64::from(STORE_BUDGET_MIB_MAX),
    ) as u32)
}

/// Limits of [`UiState::store_budget_mib`] (the same as the store budget limits).
pub const STORE_BUDGET_MIB_MIN: u32 = (crate::store::BUDGET_MIN >> 20) as u32;
pub const STORE_BUDGET_MIB_MAX: u32 = (crate::store::BUDGET_MAX >> 20) as u32;
const STORE_BUDGET_MIB_DEFAULT: u32 = (crate::store::BUDGET_DEFAULT >> 20) as u32;

impl Default for UiState {
    fn default() -> Self {
        Self {
            theme_mode: ThemeMode::default(),
            theme_preset: "default".into(),
            view: ViewMode::default(),
            row_height: RowHeight::default(),
            pane_widths: Vec::new(),
            window_size: None,
            store_budget_mib: STORE_BUDGET_MIB_DEFAULT,
            check_updates: true,
        }
    }
}

impl UiState {
    /// The store budget in bytes, for [`crate::store::Limits::with_budget`].
    pub fn store_budget_bytes(&self) -> usize {
        (self
            .store_budget_mib
            .clamp(STORE_BUDGET_MIB_MIN, STORE_BUDGET_MIB_MAX) as usize)
            << 20
    }
}

/// Missing fields take their defaults, so an older file loads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    pub profiles: Vec<Profile>,
    pub last_profile_id: Option<Uuid>,
    pub ui: UiState,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA,
            profiles: Vec::new(),
            last_profile_id: None,
            ui: UiState::default(),
        }
    }
}

/// The result of [`load`].
#[derive(Debug)]
pub struct Load {
    pub settings: Settings,
    /// A message for the user, if the file was damaged, too new or unreadable.
    pub notice: Option<String>,
    /// True: the app must never save, because that would replace a file that it could not read
    /// (a newer version wrote it, or the file could not be moved aside).
    pub read_only: bool,
}

/// `MIGRATIONS[i]` changes version `i + 1` into version `i + 2`. Empty until schema 2 exists.
const MIGRATIONS: &[fn(&mut serde_json::Value)] = &[];

fn parse(text: &str) -> Result<Result<Settings, u32>, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(text)?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .map_or(1, |v| u32::try_from(v).unwrap_or(u32::MAX))
        .max(1);
    if version > CURRENT_SCHEMA {
        return Ok(Err(version));
    }
    for migrate in &MIGRATIONS[(version - 1) as usize..] {
        migrate(&mut value);
    }
    let mut settings: Settings = serde_json::from_value(value)?;
    settings.schema_version = CURRENT_SCHEMA;
    Ok(Ok(settings))
}

fn fallback(notice: String, read_only: bool) -> Load {
    Load {
        settings: Settings::default(),
        notice: Some(notice),
        read_only,
    }
}

/// Reads the settings. This does blocking I/O: call it off the UI thread.
///
/// A missing file gives the defaults. A damaged file is renamed to `<name>.bad-<unix time>`
/// (never deleted), and the defaults are used. A file with a newer `schema_version` is not
/// touched: the result is `read_only`.
pub fn load(path: &Path) -> Load {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Load {
                settings: Settings::default(),
                notice: None,
                read_only: false,
            };
        }
        Err(e) => {
            return fallback(
                format!(
                    "The settings file cannot be read ({e}). Default settings are in use, and changes are not saved."
                ),
                true,
            );
        }
    };
    match parse(&text) {
        Ok(Ok(settings)) => Load {
            settings,
            notice: None,
            read_only: false,
        },
        Ok(Err(version)) => fallback(
            format!(
                "The settings file is from a newer version of MQTT Navigator (schema {version}). It was not changed. Default settings are in use, and changes are not saved."
            ),
            true,
        ),
        Err(_) => match keep_bad(path) {
            Ok(name) => fallback(
                format!(
                    "The settings file was damaged. It was kept as {name}. Default settings are in use."
                ),
                false,
            ),
            Err(e) => fallback(
                format!(
                    "The settings file was damaged, and it could not be moved aside ({e}). Default settings are in use, and changes are not saved."
                ),
                true,
            ),
        },
    }
}

/// Renames the file to a name that does not exist yet. Returns the new file name.
fn keep_bad(path: &Path) -> io::Result<String> {
    let stem = path.file_name().map_or_else(
        || "settings.json".into(),
        |n| n.to_string_lossy().into_owned(),
    );
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut name = format!("{stem}.bad-{secs}");
    let mut n = 2;
    while path.with_file_name(&name).exists() {
        name = format!("{stem}.bad-{secs}-{n}");
        n += 1;
    }
    fs::rename(path, path.with_file_name(&name))?;
    Ok(name)
}

/// A step of [`save_atomic_with_hook`]. For crash tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checkpoint {
    /// Half of the temp file is written.
    PartialWrite,
    /// The temp file is complete, synced and closed. The old file is still in place.
    TempSynced,
    /// The temp file replaced the old file. The directory is not synced yet.
    Renamed,
}

/// Writes the settings so that a crash leaves either the old file or the new file, never a mix.
/// It writes a temp file in the same directory, syncs it, renames it over the old file and syncs
/// the directory (Unix). It creates the directory if needed. Blocking: call it off the UI thread.
pub fn save_atomic(path: &Path, settings: &Settings) -> io::Result<()> {
    save_atomic_with_hook(path, settings, |_| {})
}

#[doc(hidden)]
pub fn save_atomic_with_hook(
    path: &Path,
    settings: &Settings,
    hook: impl Fn(Checkpoint),
) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let json = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let name = path
        .file_name()
        .map_or_else(|| "settings".into(), |n| n.to_string_lossy().into_owned());
    let tmp: PathBuf = dir.join(format!(
        ".{name}.tmp-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        let (first, rest) = json.split_at(json.len() / 2);
        file.write_all(first)?;
        hook(Checkpoint::PartialWrite);
        file.write_all(rest)?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp); // our own temp file, never the settings file
        return Err(e);
    }
    hook(Checkpoint::TempSynced);
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    hook(Checkpoint::Renamed);
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_name(name: &str) -> Settings {
        Settings {
            profiles: vec![Profile {
                name: name.into(),
                ..Profile::new()
            }],
            ..Settings::default()
        }
    }

    #[test]
    fn round_trip_and_defaults() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("sub/settings.json");
        let mut s = with_name("a");
        s.last_profile_id = Some(s.profiles[0].id);
        s.ui = UiState {
            theme_mode: ThemeMode::Dark,
            theme_preset: "tokyo-night".into(),
            view: ViewMode::Tree,
            row_height: RowHeight::OneLine,
            pane_widths: vec![200.0, 300.5],
            window_size: Some((1200.0, 800.0)),
            store_budget_mib: 512,
            check_updates: false,
        };
        save_atomic(&path, &s).expect("save");
        let l = load(&path);
        assert_eq!((l.notice, l.read_only), (None, false));
        assert_eq!(l.settings, s);
        // A second save replaces the content.
        save_atomic(&path, &with_name("b")).expect("save 2");
        assert_eq!(load(&path).settings.profiles[0].name, "b");
        // Missing file: defaults. Missing fields: defaults.
        let none = load(&dir.path().join("none.json"));
        assert_eq!((none.settings, none.notice), (Settings::default(), None));
        fs::write(&path, r#"{"profiles":[{"name":"x"}]}"#).expect("write");
        let l = load(&path);
        assert_eq!(l.settings.schema_version, 1);
        assert_eq!(l.settings.ui, UiState::default());
        assert_eq!(l.settings.ui.theme_preset, "default");
        assert_eq!(l.settings.ui.store_budget_mib, 128);
        assert!(
            l.settings.ui.check_updates,
            "an old file keeps the update check on"
        );
        assert_eq!(l.settings.profiles[0].port, 8883);
    }

    #[test]
    fn store_budget_is_clamped_on_load() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        for (text, want) in [
            (r#"{"ui":{"store_budget_mib":1}}"#, 32),
            (r#"{"ui":{"store_budget_mib":0}}"#, 32),
            (r#"{"ui":{"store_budget_mib":100000}}"#, 2048),
            (r#"{"ui":{"store_budget_mib":777}}"#, 777),
            (r#"{"ui":{"store_budget_mib":-5}}"#, 32),
            (r#"{"ui":{"store_budget_mib":300.7}}"#, 300),
            (r#"{"ui":{}}"#, 128),
        ] {
            fs::write(&path, text).expect("write");
            let ui = load(&path).settings.ui;
            assert_eq!(ui.store_budget_mib, want, "{text}");
            assert_eq!(ui.store_budget_bytes(), (want as usize) << 20);
        }
        assert_eq!(UiState::default().store_budget_mib, 128);
    }

    #[test]
    fn corrupt_file_is_kept_never_deleted() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        for round in 0..2 {
            fs::write(&path, format!("{{ not json {round}")).expect("write");
            let l = load(&path);
            assert_eq!(l.settings, Settings::default());
            assert!(!l.read_only && !path.exists());
            let notice = l.notice.expect("notice");
            let name = notice
                .strip_prefix("The settings file was damaged. It was kept as ")
                .and_then(|n| n.strip_suffix(". Default settings are in use."))
                .expect("text");
            assert!(name.starts_with("settings.json.bad-"));
            assert_eq!(
                fs::read_to_string(dir.path().join(name)).expect("kept"),
                format!("{{ not json {round}")
            );
        }
        // Two damaged files in the same second have two different names.
        let bad = fs::read_dir(dir.path()).expect("ls").count();
        assert_eq!(bad, 2);
    }

    #[test]
    fn newer_schema_is_read_only_and_untouched() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        let text = r#"{"schema_version":99,"future":true}"#;
        fs::write(&path, text).expect("write");
        let l = load(&path);
        assert!(l.read_only && l.notice.is_some());
        assert_eq!(l.settings, Settings::default());
        assert_eq!(fs::read_to_string(&path).expect("read"), text);
    }

    /// Re-runs this test binary and aborts it at a checkpoint (see `crash_child`).
    fn crash_at(checkpoint: &str, path: &Path) -> std::process::ExitStatus {
        std::process::Command::new(std::env::current_exe().expect("exe"))
            .args([
                "--exact",
                "settings::tests::crash_child",
                "--test-threads=1",
            ])
            .env("NAV_CRASH_AT", checkpoint)
            .env("NAV_CRASH_PATH", path)
            .output()
            .expect("spawn")
            .status
    }

    /// The child side. Without the env var it does nothing. With it, it saves a big file and
    /// aborts at the checkpoint, like a power loss between two steps.
    #[test]
    fn crash_child() {
        let (Ok(at), Ok(path)) = (
            std::env::var("NAV_CRASH_AT"),
            std::env::var("NAV_CRASH_PATH"),
        ) else {
            return;
        };
        let big = Settings {
            profiles: (0..5000)
                .map(|i| Profile {
                    name: format!("new {i}"),
                    ..Profile::new()
                })
                .collect(),
            ..Settings::default()
        };
        let stop = match at.as_str() {
            "partial" => Checkpoint::PartialWrite,
            "synced" => Checkpoint::TempSynced,
            _ => Checkpoint::Renamed,
        };
        let _ = save_atomic_with_hook(Path::new(&path), &big, |c| {
            if c == stop {
                std::process::abort();
            }
        });
    }

    #[test]
    fn crash_during_save_keeps_a_whole_file() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        for (at, new_content) in [("partial", false), ("synced", false), ("renamed", true)] {
            save_atomic(&path, &with_name("original")).expect("save");
            assert!(!crash_at(at, &path).success(), "child must abort at {at}");
            let l = load(&path);
            assert_eq!(l.notice, None, "{at}: file must parse");
            assert_eq!(
                l.settings.profiles.len(),
                if new_content { 5000 } else { 1 },
                "{at}"
            );
            if !new_content {
                assert_eq!(l.settings.profiles[0].name, "original");
            }
        }
    }
}
