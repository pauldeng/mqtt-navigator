//! Signed self-update (ADR 0005). Everything here blocks: the app runs it on a background
//! thread. No gpui.
//!
//! Flow: [`check_and_stage`] fetches `latest.json` and `latest.json.minisig`, runs [`verify`],
//! downloads the artifact of this build's target to `staging/`, checks size and SHA-256, and
//! only then renames `staging/` to `staged/`. [`startup`] applies `staged/` at the next start.
//!
//! The target triple comes from `build.rs` (`env!("TARGET")`), so the manifest key is exact.
#![forbid(unsafe_code)]

mod detect;
mod download;
pub mod swap;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use semver::Version;
use serde::Deserialize;

pub use detect::{UpdateMode, detect, detect_from};
pub use swap::Install;

/// The Rust target triple of this build.
pub const TARGET: &str = env!("TARGET");
/// The minisign public key of this build. A file with only a comment means: no key.
pub const PUBKEY: &str = include_str!("../../../assets/update/minisign.pub");
/// Every artifact URL of a manifest must start with this text.
pub const RELEASE_PREFIX: &str = "https://github.com/pauldeng/mqtt-navigator/releases/download/";
pub const MANIFEST_URL: &str =
    "https://github.com/pauldeng/mqtt-navigator/releases/latest/download/latest.json";
const RELEASE_PAGE: &str = "https://github.com/pauldeng/mqtt-navigator/releases/tag/";
/// A release artifact is an app of about 40 MB. This is the hard cap.
pub const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("No update key in this build.")]
    NoKey,
    #[error("The update key is not valid.")]
    BadKey,
    #[error("The update signature is not valid.")]
    BadSignature,
    #[error("The update list is not valid: {0}")]
    BadManifest(String),
    #[error("This update has no file for {0}.")]
    UnknownTarget(String),
    #[error("The update is not newer than this version.")]
    NotNewer,
    #[error("The update address is not allowed: {0}")]
    BadUrl(String),
    #[error("The update size is not allowed: {0} bytes.")]
    BadSize(u64),
    #[error("The size of the download is wrong.")]
    SizeMismatch,
    #[error("The checksum of the download is wrong.")]
    HashMismatch,
    #[error("Network error: {0}")]
    Http(String),
    #[error("File error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The signed file `latest.json`.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// One entry for each Rust target triple.
    pub targets: BTreeMap<String, Artifact>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// What a manifest must satisfy for this program.
#[derive(Clone, Debug)]
pub struct Policy {
    running: Version,
    target: String,
    prefix: String,
    allow_http: bool,
}

impl Policy {
    /// The policy of this build: its target, GitHub releases over https.
    pub fn new(running: &str) -> Result<Self> {
        Ok(Self {
            running: Version::parse(running).map_err(|e| Error::BadManifest(e.to_string()))?,
            target: TARGET.into(),
            prefix: RELEASE_PREFIX.into(),
            allow_http: false,
        })
    }

    /// Tests only: any target, any URL prefix, and plain http.
    #[cfg(feature = "test-http")]
    pub fn for_test(running: &str, target: &str, prefix: &str) -> Self {
        Self {
            running: Version::parse(running).expect("test version"),
            target: target.into(),
            prefix: prefix.into(),
            allow_http: true,
        }
    }

    fn check_url(&self, url: &str) -> Result<()> {
        let bad = || Error::BadUrl(url.chars().take(80).collect());
        let rest = url.strip_prefix(&self.prefix).ok_or_else(bad)?;
        let plain = rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-~%+/".contains(c));
        if rest.is_empty() || !plain || rest.contains("..") || !self.scheme_ok(url) {
            return Err(bad());
        }
        Ok(())
    }

    fn scheme_ok(&self, url: &str) -> bool {
        url.starts_with("https://") || (self.allow_http && url.starts_with("http://"))
    }
}

/// A manifest that passed every check, reduced to the entry for this target.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    pub artifact: Artifact,
}

/// Checks the signature first, then the content: newer version, known target, allowed address,
/// sane size. Nothing of the manifest is used before the signature is good.
pub fn verify(manifest: &[u8], minisig: &[u8], pubkey: &str, policy: &Policy) -> Result<Release> {
    if pubkey
        .lines()
        .all(|l| l.trim().is_empty() || l.starts_with('#'))
    {
        return Err(Error::NoKey);
    }
    let key = minisign_verify::PublicKey::decode(pubkey).map_err(|_| Error::BadKey)?;
    let sig = std::str::from_utf8(minisig)
        .ok()
        .and_then(|s| minisign_verify::Signature::decode(s).ok())
        .ok_or(Error::BadSignature)?;
    // allow_legacy = false: the signature must be a pre-hashed one (the default of `minisign -S`).
    key.verify(manifest, &sig, false)
        .map_err(|_| Error::BadSignature)?;
    let m: Manifest =
        serde_json::from_slice(manifest).map_err(|e| Error::BadManifest(e.to_string()))?;
    let version = Version::parse(&m.version).map_err(|e| Error::BadManifest(e.to_string()))?;
    if version <= policy.running {
        return Err(Error::NotNewer);
    }
    let artifact = m
        .targets
        .get(&policy.target)
        .ok_or_else(|| Error::UnknownTarget(policy.target.clone()))?;
    policy.check_url(&artifact.url)?;
    if artifact.size == 0 || artifact.size > MAX_ARTIFACT_BYTES {
        return Err(Error::BadSize(artifact.size));
    }
    let hex = &artifact.sha256;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::BadManifest("the checksum is not valid".into()));
    }
    Ok(Release {
        version,
        artifact: artifact.clone(),
    })
}

/// The release page of a version. The text is built from a parsed version, never from the
/// manifest, so it is always an https GitHub address.
pub fn release_page(version: &Version) -> String {
    format!("{RELEASE_PAGE}v{version}")
}

/// True for an https release page of this project, as [`release_page`] builds it. The app
/// checks this again before it opens a link.
pub fn is_release_page(url: &str) -> bool {
    url.strip_prefix(RELEASE_PAGE).is_some_and(|v| {
        !v.is_empty()
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
    })
}

pub(crate) fn agent(policy: &Policy) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(!policy.allow_http)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(15 * 60)))
        .build()
        .into()
}

pub(crate) fn http_err(e: ureq::Error) -> Error {
    Error::Http(e.to_string())
}

fn fetch(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    agent
        .get(url)
        .call()
        .map_err(http_err)?
        .into_body()
        .into_with_config()
        .limit(MAX_MANIFEST_BYTES)
        .read_to_vec()
        .map_err(http_err)
}

/// Fetches and verifies the manifest. `Ok(None)`: no newer version, or this version was rolled
/// back before.
pub fn check(
    policy: &Policy,
    pubkey: &str,
    manifest_url: &str,
    dir: &Path,
) -> Result<Option<Release>> {
    if !policy.scheme_ok(manifest_url) {
        return Err(Error::BadUrl(manifest_url.chars().take(80).collect()));
    }
    let agent = agent(policy);
    let manifest = fetch(&agent, manifest_url)?;
    let minisig = fetch(&agent, &format!("{manifest_url}.minisig"))?;
    match verify(&manifest, &minisig, pubkey, policy) {
        Ok(r) if swap::read_failed(dir).is_some_and(|f| f == r.version.to_string()) => Ok(None),
        Ok(r) => Ok(Some(r)),
        Err(Error::NotNewer) => Ok(None),
        Err(e) => Err(e),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    /// Downloaded, verified and staged. [`startup`] applies it at the next start.
    Ready {
        version: String,
    },
    /// This install cannot update itself. Show `url` (an https GitHub page) to the user.
    Notice {
        version: String,
        url: String,
        reason: String,
    },
}

/// Check, and for a self-updating install, download and stage.
pub fn check_and_stage(
    policy: &Policy,
    pubkey: &str,
    manifest_url: &str,
    mode: &UpdateMode,
    dir: &Path,
) -> Result<Outcome> {
    let Some(release) = check(policy, pubkey, manifest_url, dir)? else {
        return Ok(Outcome::UpToDate);
    };
    let version = release.version.to_string();
    match mode {
        UpdateMode::NoticeOnly { reason } => Ok(Outcome::Notice {
            url: release_page(&release.version),
            version,
            reason: reason.clone(),
        }),
        UpdateMode::SelfUpdate(install) => {
            download::download(policy, &release, install, dir)?;
            Ok(Outcome::Ready { version })
        }
    }
}

/// [`check_and_stage`] with the values of this build. Blocking. It also returns the detected
/// mode, so the app can restart later.
pub fn run_check(running: &str) -> Result<(Outcome, UpdateMode)> {
    let dir = swap::update_dir()
        .ok_or_else(|| Error::Io(io::Error::other("This system has no data folder.")))?;
    let mode = detect();
    let outcome = check_and_stage(&Policy::new(running)?, PUBKEY, MANIFEST_URL, &mode, &dir)?;
    Ok((outcome, mode))
}

/// Starts this app again with `args`. The caller quits next. The new process runs [`startup`],
/// which applies the staged update. Windows allows a rename of a running exe, so the new process
/// can move the old exe aside while the old process still closes.
pub fn restart(install: &Install, args: impl IntoIterator<Item = OsString>) -> io::Result<()> {
    Command::new(install.run_path())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Call this first in `main()`, before anything else. It runs the update protocol
/// ([`swap::prepare`]): it finishes or undoes an interrupted swap, ends or rolls back a trial,
/// and applies a staged update. When another copy must run (a new version, or the old one after
/// a rollback), it starts that copy with the same arguments. Then it replaces this process
/// (Unix) or ends it (Windows). A development build does nothing. Every error goes to
/// `update.log`, and the app goes on. It never uses the network.
pub fn startup() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let appimage = std::env::var_os("APPIMAGE");
    if let (Some(inst), Some(dir)) = (Install::locate(&exe, appimage), swap::update_dir()) {
        startup_at(&inst, &dir, &args, &mut reexec);
    }
}

/// Starts `program`. Unix: replaces this process, so it returns only an error. Windows: starts
/// it detached and ends this process, so it also returns only an error.
fn reexec(program: &Path, args: &[OsString]) -> io::Error {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.exec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008); // DETACHED_PROCESS: the new app has no console.
        match cmd.spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => e,
        }
    }
}

/// [`startup`] for a known install. `start` gets the program to start. It returns only on error.
fn startup_at(
    inst: &Install,
    dir: &Path,
    args: &[OsString],
    start: &mut dyn FnMut(&Path, &[OsString]) -> io::Error,
) {
    let Ok(plan) = swap::prepare(inst, dir, &mut |_| false) else {
        return;
    };
    // The log comes first: a successful start replaces this process.
    write_log(&plan.log);
    let Some(program) = plan.exec else { return };
    let e = start(&program, args);
    let mut lines = vec![format!("cannot start {}: {e}", program.display())];
    // A new copy that does not start: put the previous copy back and try it.
    if let Some(back) = swap::start_failed(inst, dir) {
        lines.push("rolled back after a failed start".into());
        write_log(&lines);
        let e = start(&back, args);
        lines = vec![format!("cannot start {}: {e}", back.display())];
    }
    write_log(&lines);
}

/// Best effort: appends the lines to `<log_dir>/update.log`.
fn write_log(lines: &[String]) {
    use std::io::Write as _;
    let Some(dir) = swap::log_dir().filter(|_| !lines.is_empty()) else {
        return;
    };
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("update.log"))
    {
        for line in lines {
            let _ = writeln!(f, "{secs} update: {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use swap::{State, commit_staged, staging_dir};

    /// An exe install with a staged "2.0.0".
    fn staged_install() -> (tempfile::TempDir, Install, PathBuf) {
        let tmp = tempfile::tempdir().expect("dir");
        let root = std::fs::canonicalize(tmp.path()).expect("path");
        let dir = root.join("update");
        let path = root.join(swap::PAYLOAD);
        std::fs::write(&path, "old").expect("old");
        std::fs::create_dir_all(staging_dir(&dir)).expect("staging");
        std::fs::write(staging_dir(&dir).join("payload"), "new").expect("new");
        commit_staged(&dir, "2.0.0", &path).expect("commit");
        let inst = Install {
            kind: swap::Kind::Exe,
            path,
        };
        (tmp, inst, dir)
    }

    #[test]
    fn a_swap_starts_the_new_copy_with_the_same_arguments() {
        let (_tmp, inst, dir) = staged_install();
        let args = vec![OsString::from("--tcp")];
        let started = RefCell::new(Vec::new());
        startup_at(&inst, &dir, &args, &mut |p, a| {
            started.borrow_mut().push((p.to_path_buf(), a.to_vec()));
            io::Error::other("test")
        });
        // The start fails, so the swap is undone and the old copy is tried.
        let started = started.into_inner();
        assert_eq!(started.len(), 2, "{started:?}");
        assert_eq!(started[0], (inst.path.clone(), args.clone()));
        assert_eq!(started[1], (inst.path.clone(), args));
        assert_eq!(std::fs::read_to_string(&inst.path).expect("exe"), "old");
        assert_eq!(swap::read_state(&dir), State::None);
        assert_eq!(swap::read_failed(&dir).as_deref(), Some("2.0.0"));
    }

    #[test]
    fn a_start_that_needs_no_other_copy_starts_nothing() {
        let (_tmp, inst, dir) = staged_install();
        std::fs::remove_dir_all(swap::staged_dir(&dir)).expect("no update");
        startup_at(&inst, &dir, &[], &mut |_, _| panic!("no start expected"));
    }
}
