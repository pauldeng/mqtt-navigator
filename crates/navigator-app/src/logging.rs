//! Log file and crash file. The UI thread never does file I/O for logs: lines go through a
//! bounded queue to one background thread.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use tracing_subscriber::EnvFilter;

const LOG_NAME: &str = "mqtt-navigator.log";
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const QUEUE_LINES: usize = 1024;
/// The binary crate is `mqtt_navigator`. The two library crates have their own names.
const DEFAULT_FILTER: &str =
    "warn,mqtt_navigator=info,navigator_app=info,navigator_core=info,ms_mqtt_client=off";
const MAX_PANIC_TEXT: usize = 500;

/// A log file with a size cap. When a line does not fit, the file becomes `<name>.1` (the old
/// `.1` is deleted) and a new file starts.
struct RotatingFile {
    path: PathBuf,
    cap: u64,
    file: Option<File>,
    size: u64,
}

impl RotatingFile {
    fn new(path: PathBuf, cap: u64) -> Self {
        Self {
            path,
            cap,
            file: None,
            size: 0,
        }
    }

    fn rotated_path(&self) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(".1");
        PathBuf::from(name)
    }

    fn open(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                fs::create_dir_all(dir)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            self.size = file.metadata()?.len();
            self.file = Some(file);
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is not open"))
    }

    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        self.open()?;
        if self.size > 0 && self.size + line.len() as u64 > self.cap {
            self.file = None;
            let old = self.rotated_path();
            let _ = fs::remove_file(&old); // Windows cannot rename over a file.
            fs::rename(&self.path, &old)?;
            self.open()?;
        }
        let file = self.open()?;
        file.write_all(line)?;
        self.size += line.len() as u64;
        Ok(())
    }
}

/// The `tracing` writer. It never blocks: a full queue drops the line and counts it.
#[derive(Clone)]
struct QueueWriter {
    tx: SyncSender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
}

impl Write for QueueWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.tx.try_send(buf.to_vec())
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn drain(rx: Receiver<Vec<u8>>, dropped: Arc<AtomicU64>, mut file: RotatingFile) {
    while let Ok(line) = rx.recv() {
        let n = dropped.swap(0, Ordering::Relaxed);
        if n > 0 {
            let _ = file.write_line(format!("WARN logging: {n} lines dropped\n").as_bytes());
        }
        if file.write_line(&line).is_err() {
            // The disk is full or the folder is gone. Drop the line. The UI must not fail.
        }
    }
}

fn log_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("mqtt-navigator").join("logs"))
}

/// Starts logging and the panic hook. Call it first in `main`. It does no file I/O itself.
pub fn init() {
    let Some(dir) = log_dir() else {
        return;
    };
    let (tx, rx) = sync_channel(QUEUE_LINES);
    let dropped = Arc::new(AtomicU64::new(0));
    let file = RotatingFile::new(dir.join(LOG_NAME), MAX_LOG_BYTES);
    let thread_dropped = dropped.clone();
    if std::thread::Builder::new()
        .name("log-writer".into())
        .spawn(move || drain(rx, thread_dropped, file))
        .is_err()
    {
        return;
    }
    let writer = QueueWriter { tx, dropped };
    let filter = EnvFilter::try_from_env("MQTT_NAVIGATOR_LOG")
        .or_else(|_| EnvFilter::try_new(DEFAULT_FILTER))
        .unwrap_or_else(|_| EnvFilter::new("off"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return;
    }
    install_panic_hook(dir);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "MQTT Navigator starts");
}

fn install_panic_hook(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_crash_file(&dir, info);
        previous(info);
    }));
}

/// Writes the message, the location and the thread name. The process is panicking, so this is
/// synchronous. It never touches message payloads.
fn write_crash_file(dir: &Path, info: &std::panic::PanicHookInfo<'_>) {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no text)".into());
    let message: String = message.chars().take(MAX_PANIC_TEXT).collect();
    let location = info
        .location()
        .map(|l| format!("{}:{}", l.file(), l.line()))
        .unwrap_or_else(|| "(unknown)".into());
    let thread = std::thread::current();
    let text = format!(
        "MQTT Navigator {}\nthread: {}\nlocation: {location}\nmessage: {message}\n",
        env!("CARGO_PKG_VERSION"),
        thread.name().unwrap_or("(unnamed)"),
    );
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let _ = fs::create_dir_all(dir);
    let _ = fs::write(dir.join(format!("crash-{secs}.txt")), text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mqtt-nav-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn rotates_at_the_cap_and_keeps_one_old_file() {
        let dir = temp_dir("rotate");
        let path = dir.join(LOG_NAME);
        let mut f = RotatingFile::new(path.clone(), 20);
        for line in ["aaaaaaaa\n", "bbbbbbbb\n", "cccccccc\n", "dddddddd\n"] {
            f.write_line(line.as_bytes()).expect("write");
        }
        // 9 + 9 = 18 fits. "c" rotates. "c" + "d" = 18 fits.
        assert_eq!(
            fs::read_to_string(&path).expect("log"),
            "cccccccc\ndddddddd\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("mqtt-navigator.log.1")).expect("old log"),
            "aaaaaaaa\nbbbbbbbb\n"
        );
        f.write_line(b"eeeeeeee\n").expect("write");
        f.write_line(b"ffffffff\n").expect("write");
        // The first old file is gone. Only one old file stays.
        assert_eq!(
            fs::read_to_string(dir.join("mqtt-navigator.log.1")).expect("old log"),
            "cccccccc\ndddddddd\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_line_is_written_to_an_empty_file() {
        let dir = temp_dir("long");
        let path = dir.join(LOG_NAME);
        let mut f = RotatingFile::new(path.clone(), 4);
        f.write_line(b"0123456789\n").expect("write");
        assert_eq!(fs::read_to_string(&path).expect("log"), "0123456789\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_queue_drops_and_counts() {
        let (tx, rx) = sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut w = QueueWriter {
            tx,
            dropped: dropped.clone(),
        };
        w.write_all(b"one\n").expect("write");
        w.write_all(b"two\n").expect("write");
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(rx.recv().expect("line"), b"one\n");
    }
}
