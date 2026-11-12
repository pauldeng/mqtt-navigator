//! The 0.1.0 launcher and the new startup share one update folder. On Windows, the 0.1.0
//! shortcut keeps starting the old launcher, so the old `prepare` can run before the new one at
//! any start. These tests run the real 0.1.0 code (`fixtures/swap_v0_1_0.rs`, copied from the
//! tag v0.1.0) against the new code.

#[path = "fixtures/swap_v0_1_0.rs"]
#[allow(dead_code, clippy::all)]
mod old_swap;

use std::fs;
use std::path::PathBuf;

use navigator_update::swap::{self, Install, Kind, State, commit_staged, staging_dir};

struct World {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    new: Install,
    old: old_swap::Install,
}

fn world(kind: Kind) -> World {
    let tmp = tempfile::tempdir().expect("dir");
    let root = fs::canonicalize(tmp.path()).expect("path");
    let dir = root.join("update");
    fs::create_dir(&dir).expect("update dir");
    let app = root.join("app");
    fs::create_dir(&app).expect("app dir");
    let (path, payload, old_kind) = match kind {
        Kind::Exe => {
            let p = app.join(swap::PAYLOAD);
            (p.clone(), p, old_swap::Kind::Exe)
        }
        Kind::AppImage => (
            app.join("x.AppImage"),
            app.join(swap::PAYLOAD),
            old_swap::Kind::AppImage,
        ),
        Kind::App => {
            let macos = app.join("M.app/Contents/MacOS");
            fs::create_dir_all(&macos).expect("bundle");
            (
                app.join("M.app"),
                macos.join(swap::PAYLOAD),
                old_swap::Kind::App,
            )
        }
    };
    // The old install: what the 0.1.0 launcher computes. The payload is the sibling that it starts.
    fs::write(if kind == Kind::App { &payload } else { &path }, "old").expect("old");
    World {
        _tmp: tmp,
        dir,
        new: Install {
            kind,
            path: path.clone(),
        },
        old: old_swap::Install {
            kind: old_kind,
            path,
            launcher: app.join("mqtt-navigator-launcher"),
            payload,
        },
    }
}

impl World {
    fn stage(&self, version: &str) {
        let staging = staging_dir(&self.dir);
        fs::create_dir_all(&staging).expect("staging");
        let payload = staging.join("payload");
        match self.new.kind {
            Kind::App => {
                fs::create_dir_all(payload.join("Contents/MacOS")).expect("app");
                fs::write(payload.join("Contents/MacOS").join(swap::PAYLOAD), "new").expect("new");
            }
            _ => fs::write(&payload, "new").expect("new"),
        }
        commit_staged(&self.dir, version, &self.new.path).expect("commit");
    }

    fn old_start(&self) -> old_swap::Plan {
        old_swap::prepare(&self.old, &self.dir, &mut |_| false).expect("not killed")
    }

    fn new_start(&self) -> swap::Plan {
        swap::prepare(&self.new, &self.dir, &mut |_| false).expect("not killed")
    }

    fn healthy(&self, v: &str) {
        swap::write_healthy(&self.dir, v).expect("healthy");
    }

    fn state(&self) -> String {
        fs::read_to_string(self.dir.join("state")).unwrap_or_default()
    }

    /// "old" or "new". Panics when the install is not whole.
    fn version(&self) -> String {
        let file = match self.new.kind {
            Kind::App => self.new.path.join("Contents/MacOS").join(swap::PAYLOAD),
            _ => self.new.path.clone(),
        };
        fs::read_to_string(file).expect("the install must be whole")
    }

    fn prev_exists(&self) -> bool {
        fs::symlink_metadata(self.new.prev()).is_ok()
    }

    /// Nothing was rolled back.
    fn assert_kept(&self, label: &str) {
        assert_eq!(self.version(), "new", "{label}");
        assert!(self.prev_exists(), "{label}: prev");
        assert_eq!(swap::read_failed(&self.dir), None, "{label}: failed");
    }
}

/// The old launcher applies the update and writes `started`. The new startup must not roll
/// back. The old launcher ends the trial at the next start.
#[test]
fn a_trial_of_the_old_launcher_is_never_rolled_back_by_the_new_startup() {
    for kind in [Kind::Exe, Kind::App] {
        for new_ends_it in [false, true] {
            let w = world(kind);
            let label = format!("{kind:?} new_ends_it={new_ends_it}");
            w.stage("0.1.1");
            let plan = w.old_start();
            assert_eq!(plan.exec, w.old.payload, "{label}");
            assert_eq!(w.state(), "started 0.1.1", "{label}");
            // The new startup, before the app wrote the marker: it goes on and leaves the state.
            let plan = w.new_start();
            assert_eq!(plan.exec, None, "{label}");
            assert_eq!(w.state(), "started 0.1.1", "{label}");
            w.assert_kept(&label);
            // Again, and with a newer update staged: still no rollback and no new swap.
            w.stage("0.1.2");
            assert_eq!(w.new_start().exec, None, "{label}");
            assert_eq!(w.state(), "started 0.1.1", "{label}");
            w.assert_kept(&label);
            assert!(swap::read_staged(&w.dir).is_some(), "{label}: staged stays");
            fs::remove_dir_all(swap::staged_dir(&w.dir)).expect("drop it");
            // The app opened a window.
            w.healthy("0.1.1");
            if new_ends_it {
                w.new_start();
            } else {
                w.old_start();
            }
            assert_eq!(w.state(), "", "{label}");
            w.assert_kept(&label);
        }
    }
}

#[test]
fn an_appimage_trial_of_the_old_launcher_moves_to_the_new_trial() {
    let w = world(Kind::AppImage);
    w.stage("0.1.1");
    w.old_start();
    assert_eq!(w.state(), "swapped 0.1.1");
    // The new startup is the first start of 0.1.1.
    assert_eq!(w.new_start().exec, None);
    assert_eq!(w.state(), "running 0.1.1");
    w.assert_kept("first start");
    // The old launcher does not know `running`: it leaves the install and the state alone.
    assert_eq!(w.old_start().exec, w.old.payload);
    assert_eq!(w.state(), "running 0.1.1");
    w.assert_kept("old start");
    w.healthy("0.1.1");
    w.old_start();
    w.assert_kept("old start, healthy");
    w.new_start();
    assert_eq!(w.state(), "");
    w.assert_kept("new start, healthy");
    w.old_start();
    w.assert_kept("old start, no state");
}

/// The new code swaps while the old launcher is still next to the exe.
#[test]
fn the_new_swap_works_with_the_old_launcher_next_to_the_exe() {
    for kind in [Kind::Exe, Kind::App, Kind::AppImage] {
        // The old launcher runs first and applies the staged copy itself.
        let w = world(kind);
        w.stage("0.1.2");
        w.old_start();
        assert_eq!(
            w.state(),
            if kind == Kind::AppImage {
                "swapped 0.1.2"
            } else {
                "started 0.1.2"
            }
        );
        w.new_start();
        w.assert_kept(&format!("{kind:?}: old applies"));

        // The new startup swaps. Its start fails, or the user starts the old launcher: the old
        // launcher sees `swapped` and writes `started`. The new startup must not roll back.
        let w = world(kind);
        w.stage("0.1.2");
        assert_eq!(w.new_start().exec, Some(w.new.run_path()));
        assert_eq!(w.state(), "swapped 0.1.2");
        w.old_start();
        w.assert_kept(&format!("{kind:?}: old sees swapped"));
        w.new_start();
        w.assert_kept(&format!("{kind:?}: new sees started"));
        w.healthy("0.1.2");
        w.old_start();
        assert_eq!(w.state(), "");
        w.assert_kept(&format!("{kind:?}: ended"));

        // The new trial runs. A newer copy is staged. The old launcher applies it first.
        let w = world(kind);
        w.stage("0.1.2");
        w.new_start();
        w.new_start();
        assert_eq!(w.state(), "running 0.1.2");
        w.stage("0.1.3");
        w.old_start(); // `running` is unknown to it: it applies 0.1.3 and writes its own state.
        assert!(w.state().ends_with(" 0.1.3"), "{}", w.state());
        w.assert_kept(&format!("{kind:?}: old applies the second update"));
        w.new_start();
        w.assert_kept(&format!("{kind:?}: new keeps the second update"));
    }
}

/// Only the new code: swap, first start, no marker, rollback.
#[test]
fn a_new_trial_without_a_health_marker_is_rolled_back() {
    for kind in [Kind::Exe, Kind::App, Kind::AppImage] {
        let w = world(kind);
        w.healthy("0.1.0");
        w.stage("0.1.2");
        assert_eq!(w.new_start().exec, Some(w.new.run_path()));
        assert_eq!(w.state(), "swapped 0.1.2");
        assert_eq!(w.new_start().exec, None);
        assert_eq!(w.state(), "running 0.1.2");
        // The next start has no marker of 0.1.2.
        let plan = w.new_start();
        assert_eq!(plan.exec, Some(w.new.run_path()), "{kind:?}");
        assert_eq!(w.version(), "old", "{kind:?}");
        assert_eq!(w.state(), "", "{kind:?}");
        assert_eq!(swap::read_failed(&w.dir).as_deref(), Some("0.1.2"));
        assert_eq!(swap::read_state(&w.dir), State::None);
        // The old launcher finds nothing to do.
        w.old_start();
        assert_eq!(w.version(), "old", "{kind:?}");
    }
}
