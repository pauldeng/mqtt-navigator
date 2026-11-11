//! The launcher side: swap, trial, rollback, and kills between the steps (plan P6).

use std::fs;
use std::path::{Path, PathBuf};

use navigator_update::swap::{
    self, Install, Kind, State, Step, commit_staged, copy_any, prepare, read_state, staging_dir,
};

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    /// The update folder.
    dir: PathBuf,
    install: Install,
}

fn env(kind: Kind) -> Env {
    let tmp = tempfile::tempdir().expect("dir");
    let root = fs::canonicalize(tmp.path()).expect("path");
    let dir = root.join("update");
    fs::create_dir(&dir).expect("update dir");
    let app = root.join("app");
    fs::create_dir(&app).expect("app dir");
    let install = match kind {
        Kind::Exe | Kind::AppImage => {
            let path = app.join(if kind == Kind::Exe {
                swap::PAYLOAD
            } else {
                "x.AppImage"
            });
            fs::write(&path, "old").expect("old");
            Install {
                kind,
                path,
                launcher: app.join(swap::LAUNCHER),
                payload: app.join(swap::PAYLOAD),
            }
        }
        Kind::App => {
            let macos = app.join("M.app/Contents/MacOS");
            fs::create_dir_all(&macos).expect("bundle");
            fs::write(macos.join(swap::PAYLOAD), "old").expect("old");
            Install {
                kind,
                path: app.join("M.app"),
                launcher: macos.join(swap::LAUNCHER),
                payload: macos.join(swap::PAYLOAD),
            }
        }
    };
    Env {
        _tmp: tmp,
        root,
        dir,
        install,
    }
}

impl Env {
    /// A verified update in `staged/`.
    fn stage(&self, version: &str) {
        let staging = staging_dir(&self.dir);
        fs::create_dir_all(&staging).expect("staging");
        let payload = staging.join("payload");
        match self.install.kind {
            Kind::App => {
                fs::create_dir_all(payload.join("Contents/MacOS")).expect("app");
                fs::write(payload.join("Contents/MacOS").join(swap::PAYLOAD), "new").expect("new");
            }
            _ => fs::write(&payload, "new").expect("new"),
        }
        commit_staged(&self.dir, version, &self.install.path).expect("commit");
    }

    /// What the installed copy says: "old" or "new". Panics when it is not a whole version.
    fn version(&self) -> String {
        let file = match self.install.kind {
            Kind::App => self.install.path.join("Contents/MacOS").join(swap::PAYLOAD),
            _ => self.install.path.clone(),
        };
        fs::read_to_string(file).expect("the install must be whole")
    }

    fn prev_exists(&self) -> bool {
        fs::symlink_metadata(self.install.prev()).is_ok()
    }

    fn start(&self) -> swap::Plan {
        prepare(&self.install, &self.dir, &mut |_| false).expect("not killed")
    }

    fn healthy(&self, v: &str) {
        swap::write_healthy(&self.dir, v).expect("healthy");
    }
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

#[test]
fn a_staged_update_is_swapped_and_the_previous_copy_is_kept() {
    for kind in [Kind::Exe, Kind::App] {
        let e = env(kind);
        e.stage("2.0.0");
        let plan = e.start();
        assert_eq!(e.version(), "new", "{kind:?}");
        assert!(e.prev_exists());
        assert_eq!(plan.exec, e.install.run_path());
        assert_eq!(read_state(&e.dir), State::Started("2.0.0".into()));
        assert!(!exists(&swap::staged_dir(&e.dir)));
        // The app opens a window: the trial ends and the previous copy stays.
        e.healthy("2.0.0");
        e.start();
        assert_eq!(read_state(&e.dir), State::None);
        assert_eq!(e.version(), "new");
        assert!(e.prev_exists());
    }
}

#[test]
fn a_new_version_that_never_becomes_healthy_is_rolled_back() {
    for kind in [Kind::Exe, Kind::App, Kind::AppImage] {
        let e = env(kind);
        // An older version wrote a marker. It must not count for the new version.
        e.healthy("1.0.0");
        e.stage("2.0.0");
        let plan = e.start();
        assert_eq!(e.version(), "new");
        if kind == Kind::AppImage {
            // The old launcher starts the new image. The new launcher begins the trial.
            assert_eq!(plan.exec, e.install.path);
            assert_eq!(read_state(&e.dir), State::Swapped("2.0.0".into()));
            let plan = e.start();
            assert_eq!(
                plan.exec, e.install.payload,
                "no rollback at the first start"
            );
            assert_eq!(read_state(&e.dir), State::Started("2.0.0".into()));
            assert_eq!(e.version(), "new");
        }
        // The next start finds no marker of 2.0.0.
        let plan = e.start();
        assert_eq!(e.version(), "old", "{kind:?}");
        assert_eq!(plan.exec, e.install.run_path());
        assert!(
            plan.log.iter().any(|l| l.contains("rolled back")),
            "{:?}",
            plan.log
        );
        assert_eq!(read_state(&e.dir), State::None);
        assert_eq!(swap::read_failed(&e.dir).as_deref(), Some("2.0.0"));
        assert!(!e.prev_exists());
        // Nothing more happens.
        e.start();
        assert_eq!(e.version(), "old");
    }
}

#[test]
fn a_staged_update_for_another_install_is_left_alone() {
    let e = env(Kind::Exe);
    e.stage("2.0.0");
    fs::write(swap::staged_dir(&e.dir).join("for"), "/somewhere/else").expect("for");
    e.start();
    assert_eq!(e.version(), "old");
    assert!(swap::read_staged(&e.dir).is_some());
}

#[test]
fn a_failed_start_after_the_swap_is_undone() {
    let e = env(Kind::Exe);
    e.stage("2.0.0");
    e.start();
    assert_eq!(
        swap::start_failed(&e.install, &e.dir),
        Some(e.install.run_path())
    );
    assert_eq!(e.version(), "old");
    assert_eq!(read_state(&e.dir), State::None);
}

/// A launcher that is killed after any step of the swap. The next start must leave a whole
/// install, and either finish the update or go back to the old version.
#[test]
fn a_launcher_killed_between_the_steps_recovers() {
    for kind in [Kind::Exe, Kind::App, Kind::AppImage] {
        for kill_at in [
            Step::StateWritten,
            Step::PrevRemoved,
            Step::CurrentMoved,
            Step::StagedMoved,
        ] {
            let e = env(kind);
            e.stage("2.0.0");
            let killed = prepare(&e.install, &e.dir, &mut |s| s == kill_at);
            assert!(killed.is_err(), "{kind:?} {kill_at:?}");
            // The next launcher start repairs the install.
            e.start();
            let label = format!("{kind:?} killed at {kill_at:?}");
            // Whole install, either version.
            let v = e.version();
            assert!(v == "old" || v == "new", "{label}: {v}");
            match v.as_str() {
                // Updated: the trial runs, the state says so, and no half file is left.
                "new" => assert!(
                    matches!(read_state(&e.dir), State::Started(_) | State::Swapped(_)),
                    "{label}: {:?}",
                    read_state(&e.dir)
                ),
                _ => assert_eq!(read_state(&e.dir), State::None, "{label}"),
            }
            assert!(!exists(&swap::staging_dir(&e.dir)), "{label}");
            if kill_at != Step::StagedMoved {
                // The kill came before the new file moved: the update completes at once.
                assert_eq!(v, "new", "{label}");
            }
            // Whatever happened, a version that is never healthy ends on "old".
            for _ in 0..3 {
                e.start();
            }
            assert_eq!(e.version(), "old", "{label}: back to the old version");
            assert_eq!(read_state(&e.dir), State::None, "{label}");
        }
    }
}

#[test]
fn a_kill_after_the_old_copy_moved_restores_it_before_anything_else() {
    let e = env(Kind::Exe);
    e.stage("2.0.0");
    let _ = prepare(&e.install, &e.dir, &mut |s| s == Step::CurrentMoved);
    assert!(!exists(&e.install.path), "the install is missing now");
    assert!(e.prev_exists());
    // The staged file is still there: the next start finishes the update.
    e.start();
    assert_eq!(e.version(), "new");
    assert_eq!(fs::read_to_string(e.install.prev()).expect("prev"), "old");
}

#[test]
fn copy_keeps_links_and_trees() {
    let e = env(Kind::Exe);
    let src = e.root.join("src");
    fs::create_dir_all(src.join("a/b")).expect("tree");
    fs::write(src.join("a/b/f"), "data").expect("file");
    #[cfg(unix)]
    std::os::unix::fs::symlink("a/b/f", src.join("link")).expect("link");
    let dst = e.root.join("dst");
    copy_any(&src, &dst).expect("copy");
    assert_eq!(fs::read_to_string(dst.join("a/b/f")).expect("file"), "data");
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(dst.join("link")).expect("link"),
        Path::new("a/b/f")
    );
}

/// Starts the trial of "2.0.0": the new copy is installed, no `healthy` marker yet.
fn into_trial(e: &Env) {
    e.stage("2.0.0");
    e.start();
    if e.install.kind == Kind::AppImage {
        e.start(); // The new launcher begins the trial.
    }
    assert_eq!(read_state(&e.dir), State::Started("2.0.0".into()));
    assert_eq!(e.version(), "new");
}

/// After a rollback is over, at any kill point: the previous copy, no state, no leftovers.
fn assert_rolled_back(e: &Env, label: &str) {
    assert_eq!(e.version(), "old", "{label}");
    assert_eq!(read_state(&e.dir), State::None, "{label}");
    assert_eq!(
        swap::read_failed(&e.dir).as_deref(),
        Some("2.0.0"),
        "{label}"
    );
    assert!(!e.prev_exists(), "{label}: prev");
    assert!(!exists(&e.install.aside()), "{label}: bad");
    assert!(!exists(&swap::staged_dir(&e.dir)), "{label}: staged");
}

/// A launcher killed after any step of a rollback. The install path must always hold a whole
/// copy, or the previous copy must still be on disk. The next start ends the rollback.
#[test]
fn a_rollback_killed_between_the_steps_recovers() {
    for kind in [Kind::Exe, Kind::App, Kind::AppImage] {
        for kill_at in [
            Step::BadMoved,
            Step::PrevRestored,
            Step::BadRemoved,
            Step::FailedWritten,
        ] {
            let e = env(kind);
            into_trial(&e);
            let label = format!("{kind:?} killed at {kill_at:?}");
            let killed = prepare(&e.install, &e.dir, &mut |s| s == kill_at);
            assert!(killed.is_err(), "{label}");
            // The state still says `started`: the rollback is not over.
            assert_eq!(
                read_state(&e.dir),
                State::Started("2.0.0".into()),
                "{label}"
            );
            match kill_at {
                // The bad copy is aside, and nothing is installed. The previous copy waits.
                Step::BadMoved => {
                    assert!(!exists(&e.install.path), "{label}");
                    assert!(e.prev_exists() && exists(&e.install.aside()), "{label}");
                }
                _ => assert_eq!(e.version(), "old", "{label}"),
            }
            let plan = e.start();
            // After the restore, the user starts the restored file: the sibling is right.
            let ok = plan.exec == e.install.run_path() || plan.exec == e.install.payload;
            assert!(ok, "{label}: {:?}", plan.log);
            assert_rolled_back(&e, &label);
            e.start();
            assert_rolled_back(&e, &label);
        }
    }
}

#[test]
fn a_rolled_back_version_is_never_applied_again() {
    let e = env(Kind::Exe);
    into_trial(&e);
    e.start();
    assert_rolled_back(&e, "first");
    // The same version is staged again (an old download, or a race).
    e.stage("2.0.0");
    let plan = e.start();
    assert_eq!(e.version(), "old");
    assert!(!e.prev_exists());
    assert!(!exists(&swap::staged_dir(&e.dir)));
    assert_eq!(read_state(&e.dir), State::None);
    assert!(plan.log.iter().any(|l| l.contains("rolled back before")));
    // A newer version is applied.
    e.stage("2.1.0");
    e.start();
    assert_eq!(e.version(), "new");
    assert_eq!(read_state(&e.dir), State::Started("2.1.0".into()));
}

#[test]
fn a_rollback_drops_a_staged_copy_of_the_failed_version_but_keeps_a_newer_one() {
    for (staged, kept) in [("2.0.0", false), ("2.1.0", true)] {
        let e = env(Kind::Exe);
        into_trial(&e);
        e.stage(staged); // The app staged it during the trial.
        e.start();
        assert_eq!(e.version(), "old", "{staged}");
        assert_eq!(swap::read_staged(&e.dir).is_some(), kept, "{staged}");
    }
}

#[test]
fn the_trial_state_is_written_only_after_the_staged_copy_is_gone() {
    let e = env(Kind::Exe);
    e.stage("2.0.0");
    let mut seen = Vec::new();
    let killed = prepare(&e.install, &e.dir, &mut |s| {
        seen.push((s, exists(&swap::staged_dir(&e.dir))));
        s == Step::StagedMoved
    });
    assert!(killed.is_err());
    // At the last step before `finish`, the state is still `swapping`.
    assert_eq!(read_state(&e.dir), State::Swapping("2.0.0".into()));
    e.start();
    assert!(!exists(&swap::staged_dir(&e.dir)));
    assert_eq!(read_state(&e.dir), State::Started("2.0.0".into()));
}

/// The update folder on another volume than the install: `place` must copy. Skipped when
/// `/dev/shm` is missing or on the same device as the temp folder.
#[cfg(unix)]
#[test]
fn a_copy_across_volumes_leaves_no_staged_folder() {
    use std::os::unix::fs::MetadataExt;
    for kind in [Kind::Exe, Kind::App] {
        let mut e = env(kind);
        let Ok(other) = tempfile::tempdir_in("/dev/shm") else {
            eprintln!("skipped: no /dev/shm");
            return;
        };
        let (a, b) = (fs::metadata(&e.root), fs::metadata(other.path()));
        if a.expect("meta").dev() == b.expect("meta").dev() {
            eprintln!("skipped: /dev/shm is on the same device as the temp folder");
            return;
        }
        e.dir = other.path().join("update");
        fs::create_dir(&e.dir).expect("dir");
        e.stage("2.0.0");
        e.start();
        assert_eq!(e.version(), "new", "{kind:?}");
        assert!(
            !exists(&swap::staged_dir(&e.dir)),
            "{kind:?}: staged is gone"
        );
        assert!(!exists(&e.install.path.with_extension("new")));
        assert_eq!(read_state(&e.dir), State::Started("2.0.0".into()));
        // Never healthy: the previous copy returns, and the version never comes back.
        e.start();
        assert_eq!(e.version(), "old");
        assert!(!exists(&swap::staged_dir(&e.dir)));
    }
}

/// A kill after the copy, before the last rename: the whole new copy waits in `<path>.new`.
#[test]
fn a_kill_between_the_copy_and_the_last_rename_finishes_the_swap() {
    let e = env(Kind::Exe);
    let new = {
        let mut n = e.install.path.clone().into_os_string();
        n.push(".new");
        PathBuf::from(n)
    };
    fs::rename(&e.install.path, e.install.prev()).expect("prev");
    fs::write(&new, "new").expect("new");
    swap::write_state(&e.dir, &State::Swapping("2.0.0".into())).expect("state");
    let plan = e.start();
    assert_eq!(e.version(), "new", "{:?}", plan.log);
    assert_eq!(fs::read_to_string(e.install.prev()).expect("prev"), "old");
    assert_eq!(read_state(&e.dir), State::Started("2.0.0".into()));
}
