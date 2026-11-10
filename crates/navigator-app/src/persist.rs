//! The settings file: its path, the cleanup of stale temp files and the debounced writer.
//!
//! All file I/O runs off the UI thread. The UI gives the writer the latest `Settings`. The writer
//! saves after 500 ms without a new change. It blocks when nothing is pending (no polling).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use navigator_core::settings::{self, Settings};

/// Quiet time before the writer saves.
pub const QUIET: Duration = Duration::from_millis(500);

/// `<config dir>/mqtt-navigator/settings.json`. `None` if the system has no config folder.
pub fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("mqtt-navigator").join("settings.json"))
}

/// Deletes `.settings.json.tmp-*` files next to the settings file. A crashed save leaves them.
/// Blocking: call it off the UI thread.
pub fn remove_stale_tmp(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let prefix = format!(".{}.tmp-", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_stale = entry.file_name().to_string_lossy().starts_with(&prefix)
            && entry.file_type().is_ok_and(|t| t.is_file());
        if is_stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The decision logic of the writer. It has no clock and no thread, so tests can run it.
#[derive(Debug)]
pub struct Debounce {
    quiet: Duration,
    /// When the last change came. `None`: nothing is pending.
    last_change: Option<Instant>,
}

impl Debounce {
    pub fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            last_change: None,
        }
    }

    /// A change came at `now`. The quiet time starts again.
    pub fn touch(&mut self, now: Instant) {
        self.last_change = Some(now);
    }

    /// How long the writer may sleep. `None`: nothing is pending, so it blocks until a message.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.last_change
            .map(|t| (t + self.quiet).saturating_duration_since(now))
    }

    /// True (once) when a change is pending and the quiet time is over.
    pub fn take_due(&mut self, now: Instant) -> bool {
        match self.last_change {
            Some(t) if now >= t + self.quiet => {
                self.last_change = None;
                true
            }
            _ => false,
        }
    }

    /// True (once) when a change is pending, whatever the time. For a flush on quit.
    pub fn take_now(&mut self) -> bool {
        self.last_change.take().is_some()
    }
}

/// A wake-up for the writer. It carries no data: the data is in the [`Slot`].
enum Msg {
    /// The settings changed. The quiet time starts again.
    Changed,
    /// A flush is waiting in the slot.
    Wake,
}

/// What the UI hands to the writer. The queue only wakes the writer, so a full queue cannot
/// lose a snapshot or a flush request: the writer reads the slot after every wake-up.
#[derive(Default)]
struct Slot {
    /// The newest snapshot. The UI replaces it. The writer takes it.
    settings: Option<Settings>,
    /// Receivers wait for an immediate save. The writer answers them after it has saved.
    flushes: Vec<tokio::sync::oneshot::Sender<()>>,
}

fn lock(slot: &Mutex<Slot>) -> std::sync::MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Handle of the writer thread.
pub struct Writer {
    slot: Arc<Mutex<Slot>>,
    tx: SyncSender<Msg>,
}

impl Writer {
    /// Starts the thread. A failed save sends its text to `errors` (`try_send`: never blocks).
    pub fn start(path: PathBuf, errors: tokio::sync::mpsc::Sender<String>) -> Self {
        let slot = Arc::new(Mutex::new(Slot::default()));
        let (tx, rx) = sync_channel(4);
        let shared = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("settings-writer".into())
            .spawn(move || run(&rx, &shared, &path, &errors));
        if let Err(e) = spawned {
            tracing::error!(error = %e, "Cannot start the settings writer");
        }
        Self { slot, tx }
    }

    /// Gives the writer the newest settings. The save happens after the quiet time.
    pub fn save(&self, settings: Settings) {
        lock(&self.slot).settings = Some(settings);
        // A full queue means that the writer has messages to read. It reads the slot later.
        let _ = self.tx.try_send(Msg::Changed);
    }

    /// Asks for an immediate save of the newest settings. The receiver is ready after the save.
    /// It also ends (with an error) if the writer is gone, so awaiting it never hangs.
    pub fn flush(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (ack, done) = tokio::sync::oneshot::channel();
        lock(&self.slot).flushes.push(ack);
        // The request is in the slot. A full queue means that the writer will read a message
        // and then the slot, so the wake-up can be lost without harm.
        if let Err(std::sync::mpsc::TrySendError::Disconnected(_)) = self.tx.try_send(Msg::Wake) {
            lock(&self.slot).flushes.clear();
        }
        done
    }
}

fn run(
    rx: &Receiver<Msg>,
    slot: &Mutex<Slot>,
    path: &Path,
    errors: &tokio::sync::mpsc::Sender<String>,
) {
    let write = || {
        let snapshot = lock(slot).settings.take();
        if let Some(s) = snapshot
            && let Err(e) = settings::save_atomic(path, &s)
        {
            tracing::warn!(error = %e, "Cannot save the settings");
            let _ = errors.try_send(format!("The settings were not saved ({e})."));
        }
    };
    let mut debounce = Debounce::new(QUIET);
    loop {
        // With a pending change: sleep until the quiet time ends. Without: block.
        let msg = match debounce.wait(Instant::now()) {
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
            Some(d) => match rx.recv_timeout(d) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        match msg {
            Some(Msg::Changed) => debounce.touch(Instant::now()),
            Some(Msg::Wake) => {}
            None => {
                if debounce.take_due(Instant::now()) {
                    write();
                }
            }
        }
        // A flush request is in the slot, even if its wake-up did not fit in the queue.
        let acks = std::mem::take(&mut lock(slot).flushes);
        if !acks.is_empty() {
            debounce.take_now();
            write();
            for ack in acks {
                let _ = ack.send(());
            }
        }
    }
    if debounce.take_now() {
        write();
    }
}

#[cfg(test)]
mod tests {
    use super::{Debounce, QUIET, Writer, remove_stale_tmp};
    use std::time::{Duration, Instant};

    #[test]
    fn blocks_when_nothing_is_pending() {
        let mut d = Debounce::new(QUIET);
        let t0 = Instant::now();
        assert_eq!(d.wait(t0), None);
        assert!(!d.take_due(t0 + Duration::from_secs(60)));
        assert!(!d.take_now());
    }

    #[test]
    fn saves_after_the_quiet_time() {
        let mut d = Debounce::new(QUIET);
        let t0 = Instant::now();
        d.touch(t0);
        assert_eq!(
            d.wait(t0 + Duration::from_millis(100)),
            Some(Duration::from_millis(400))
        );
        assert!(!d.take_due(t0 + Duration::from_millis(499)));
        assert!(d.take_due(t0 + QUIET));
        // One change gives one save.
        assert_eq!(d.wait(t0 + QUIET), None);
        assert!(!d.take_due(t0 + Duration::from_secs(5)));
    }

    #[test]
    fn a_new_change_restarts_the_quiet_time() {
        let mut d = Debounce::new(QUIET);
        let t0 = Instant::now();
        d.touch(t0);
        d.touch(t0 + Duration::from_millis(400));
        assert!(!d.take_due(t0 + Duration::from_millis(600)));
        assert_eq!(
            d.wait(t0 + Duration::from_millis(600)),
            Some(Duration::from_millis(300))
        );
        assert!(d.take_due(t0 + Duration::from_millis(900)));
    }

    #[test]
    fn overdue_wait_is_zero_and_flush_takes_the_change() {
        let mut d = Debounce::new(QUIET);
        let t0 = Instant::now();
        d.touch(t0);
        assert_eq!(d.wait(t0 + Duration::from_secs(9)), Some(Duration::ZERO));
        assert!(d.take_now());
        assert!(!d.take_now());
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mqtt-navigator-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn removes_only_stale_temp_files() {
        let dir = temp_dir("stale");
        let path = dir.join("settings.json");
        for name in [
            "settings.json",
            ".settings.json.tmp-1-2-3",
            ".settings.json.tmp-9",
            "other.json",
        ] {
            std::fs::write(dir.join(name), "x").expect("write");
        }
        remove_stale_tmp(&path);
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["other.json", "settings.json"]);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_thread_saves_the_newest_settings_on_flush() {
        let dir = temp_dir("writer");
        let path = dir.join("settings.json");
        let (err_tx, mut err_rx) = tokio::sync::mpsc::channel(4);
        let writer = Writer::start(path.clone(), err_tx);
        let mut s = navigator_core::settings::Settings::default();
        writer.save(s.clone());
        s.last_profile_id = Some(uuid_from(7));
        writer.save(s.clone());
        let flush = writer.flush();
        // The test thread may block: it is not the UI thread.
        flush.blocking_recv().expect("flushed");
        assert!(err_rx.try_recv().is_err());
        let load = navigator_core::settings::load(&path);
        assert_eq!(load.settings.last_profile_id, s.last_profile_id);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_flush_survives_a_full_queue() {
        use super::{Msg, Slot, run};
        use std::sync::{Arc, Mutex, mpsc::sync_channel};
        let dir = temp_dir("saturated");
        let path = dir.join("settings.json");
        let (err_tx, mut err_rx) = tokio::sync::mpsc::channel(4);
        let (tx, rx) = sync_channel(1);
        let writer = Writer {
            slot: Arc::new(Mutex::new(Slot::default())),
            tx,
        };
        // Fill the queue before the writer thread exists: nothing reads it yet.
        writer.tx.try_send(Msg::Changed).expect("one message fits");
        let s = navigator_core::settings::Settings {
            last_profile_id: Some(uuid_from(9)),
            ..Default::default()
        };
        writer.save(s.clone());
        let done = writer.flush();
        assert!(matches!(
            writer.tx.try_send(Msg::Wake),
            Err(std::sync::mpsc::TrySendError::Full(_))
        ));
        let slot = Arc::clone(&writer.slot);
        let thread = std::thread::spawn({
            let path = path.clone();
            move || run(&rx, &slot, &path, &err_tx)
        });
        // The test thread may block: it is not the UI thread.
        done.blocking_recv().expect("flushed");
        assert!(err_rx.try_recv().is_err());
        let load = navigator_core::settings::load(&path);
        assert_eq!(load.settings.last_profile_id, s.last_profile_id);
        drop(writer);
        thread.join().expect("writer thread ends");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    fn uuid_from(n: u128) -> uuid::Uuid {
        uuid::Uuid::from_u128(n)
    }
}
