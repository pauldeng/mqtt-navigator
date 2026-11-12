//! The update contract: what the app accepts and rejects (ADR 0005, plan P6).

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use common::{Keys, keys, manifest, manifest_with, route, serve, sha256_hex};
use navigator_update::swap::{self, Install, Kind};
use navigator_update::{Error, Outcome, Policy, UpdateMode, check_and_stage, release_page, verify};

const TARGET: &str = "x86_64-unknown-linux-gnu";
const GOOD_URL: &str =
    "https://github.com/pauldeng/mqtt-navigator/releases/download/v2.0.0/mqtt-navigator.AppImage";

fn signed(k: &Keys, json: &str) -> (Vec<u8>, Vec<u8>) {
    (
        json.as_bytes().to_vec(),
        k.sign(json.as_bytes()).into_bytes(),
    )
}

fn policy() -> Policy {
    // The production address rules, for the target of the test manifests. The https prefix
    // rejects every http address.
    Policy::for_test("1.0.0", TARGET, navigator_update::RELEASE_PREFIX)
}

fn good_manifest() -> String {
    manifest("2.0.0", TARGET, GOOD_URL, b"payload")
}

#[test]
fn a_good_manifest_is_accepted() {
    let k = keys();
    let (m, s) = signed(&k, &good_manifest());
    let r = verify(&m, &s, &k.pubkey, &policy()).expect("valid");
    assert_eq!(r.version.to_string(), "2.0.0");
    assert_eq!(r.artifact.size, 7);
    assert_eq!(r.artifact.sha256, sha256_hex(b"payload"));
}

/// CI signs with `minisign -S`, which writes a pre-hashed signature (algorithm "ED", which
/// starts with "RU" in base64). The legacy kind ("Ed", "RWQ") is not accepted.
#[test]
fn a_prehashed_signature_of_the_cli_format_is_accepted() {
    let k = keys();
    let (m, s) = signed(&k, &good_manifest());
    let line = String::from_utf8(s.clone()).expect("text");
    assert!(
        line.lines()
            .nth(1)
            .expect("signature line")
            .starts_with("RU")
    );
    assert!(line.starts_with("untrusted comment:") && line.contains("trusted comment:"));
    assert!(verify(&m, &s, &k.pubkey, &policy()).is_ok());
}

#[test]
fn a_bad_signature_is_rejected() {
    let (k, other) = (keys(), keys());
    let json = good_manifest();
    // Signed by another key.
    let (m, s) = signed(&other, &json);
    assert!(matches!(
        verify(&m, &s, &k.pubkey, &policy()),
        Err(Error::BadSignature)
    ));
    // Changed after signing: the new hash would point to another file.
    let (_, s) = signed(&k, &json);
    let evil = json.replace(&sha256_hex(b"payload"), &sha256_hex(b"evil"));
    assert!(matches!(
        verify(evil.as_bytes(), &s, &k.pubkey, &policy()),
        Err(Error::BadSignature)
    ));
    // Not a signature at all.
    assert!(matches!(
        verify(json.as_bytes(), b"junk", &k.pubkey, &policy()),
        Err(Error::BadSignature)
    ));
}

#[test]
fn a_build_without_a_key_fails_closed() {
    let k = keys();
    let (m, s) = signed(&k, &good_manifest());
    let placeholder = "# No update key yet.\n";
    let err = verify(&m, &s, placeholder, &policy()).expect_err("no key");
    assert!(matches!(err, Error::NoKey));
    assert_eq!(err.to_string(), "No update key in this build.");
    // The file in the repository is the placeholder, or a real key that parses.
    let embedded = navigator_update::PUBKEY;
    assert!(
        verify(&m, &s, embedded, &policy()).is_err(),
        "a manifest of a test key never matches the embedded key"
    );
}

#[test]
fn the_same_or_an_older_version_is_rejected() {
    let k = keys();
    for v in ["1.0.0", "0.9.9", "1.0.0-rc.1"] {
        let (m, s) = signed(&k, &manifest(v, TARGET, GOOD_URL, b"payload"));
        assert!(
            matches!(verify(&m, &s, &k.pubkey, &policy()), Err(Error::NotNewer)),
            "{v}"
        );
    }
    let (m, s) = signed(&k, &manifest("1.0.1", TARGET, GOOD_URL, b"payload"));
    assert!(verify(&m, &s, &k.pubkey, &policy()).is_ok());
}

#[test]
fn an_unknown_target_is_rejected() {
    let k = keys();
    let (m, s) = signed(
        &k,
        &manifest("2.0.0", "riscv64-unknown-linux-gnu", GOOD_URL, b"x"),
    );
    assert!(matches!(
        verify(&m, &s, &k.pubkey, &policy()),
        Err(Error::UnknownTarget(_))
    ));
}

#[test]
fn only_https_github_release_urls_are_accepted() {
    let k = keys();
    for url in [
        "http://github.com/pauldeng/mqtt-navigator/releases/download/v2/x",
        "https://example.com/pauldeng/mqtt-navigator/releases/download/v2/x",
        "https://github.com.evil.example/pauldeng/mqtt-navigator/releases/download/v2/x",
        "https://github.com/pauldeng/other/releases/download/v2/x",
        "https://github.com/pauldeng/mqtt-navigator/releases/download/../../../evil/x",
        "https://github.com/pauldeng/mqtt-navigator/releases/download/v2/x?a=b",
        "https://github.com/pauldeng/mqtt-navigator/releases/download/",
        "file:///etc/passwd",
        "",
    ] {
        let (m, s) = signed(&k, &manifest("2.0.0", TARGET, url, b"x"));
        assert!(
            matches!(verify(&m, &s, &k.pubkey, &policy()), Err(Error::BadUrl(_))),
            "{url}"
        );
    }
}

#[test]
fn a_size_over_the_cap_or_zero_is_rejected() {
    let k = keys();
    let sha = sha256_hex(b"x");
    for size in [0, navigator_update::MAX_ARTIFACT_BYTES + 1, u64::MAX] {
        let (m, s) = signed(&k, &manifest_with("2.0.0", TARGET, GOOD_URL, &sha, size));
        assert!(
            matches!(verify(&m, &s, &k.pubkey, &policy()), Err(Error::BadSize(_))),
            "{size}"
        );
    }
}

// --- download ------------------------------------------------------------------------------

/// A server with a signed manifest for `artifact_bytes`, and the artifact route.
struct Fixture {
    k: Keys,
    base: String,
    dir: tempfile::TempDir,
    install: Install,
}

fn fixture(version: &str, advertised: &[u8], served: Vec<u8>, cut: Option<usize>) -> Fixture {
    let k = keys();
    let base = serve(|base| {
        let json = manifest(version, TARGET, &format!("{base}/dl/payload"), advertised);
        let sig = k.sign(json.as_bytes());
        let mut routes = HashMap::new();
        routes.insert("/latest.json".to_string(), route(json));
        routes.insert("/latest.json.minisig".to_string(), route(sig));
        routes.insert(
            "/dl/payload".to_string(),
            common::Route { body: served, cut },
        );
        routes
    });
    let dir = tempfile::tempdir().expect("dir");
    let app = dir.path().join("app");
    fs::create_dir(&app).expect("app dir");
    fs::write(app.join(swap::PAYLOAD), "old").expect("payload");
    let install = Install {
        kind: Kind::Exe,
        path: app.join(swap::PAYLOAD),
    };
    Fixture {
        k,
        base,
        dir,
        install,
    }
}

impl Fixture {
    fn policy(&self) -> Policy {
        Policy::for_test("1.0.0", TARGET, &format!("{}/dl/", self.base))
    }

    fn run(&self) -> navigator_update::Result<Outcome> {
        let mode = UpdateMode::SelfUpdate(self.install.clone());
        check_and_stage(
            &self.policy(),
            &self.k.pubkey,
            &format!("{}/latest.json", self.base),
            &mode,
            &self.dir.path().join("update"),
        )
    }

    fn update_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("update")
    }
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

#[test]
fn a_good_download_is_staged_after_the_hash_matched() {
    let f = fixture("2.0.0", b"new payload", b"new payload".to_vec(), None);
    assert_eq!(
        f.run().expect("ok"),
        Outcome::Ready {
            version: "2.0.0".into()
        }
    );
    let d = f.update_dir();
    assert_eq!(
        fs::read(swap::staged_dir(&d).join("payload")).expect("payload"),
        b"new payload"
    );
    assert_eq!(
        swap::read_staged(&d),
        Some(("2.0.0".into(), f.install.path.clone()))
    );
    assert!(!exists(&swap::staging_dir(&d)), "no staging left");
    // The same update again does nothing.
    assert_eq!(
        f.run().expect("again"),
        Outcome::Ready {
            version: "2.0.0".into()
        }
    );
}

#[test]
fn a_hash_mismatch_is_never_staged() {
    // Same size, other bytes.
    let f = fixture("2.0.0", b"new payload", b"NEW payload".to_vec(), None);
    assert!(matches!(f.run(), Err(Error::HashMismatch)));
    assert!(!exists(&swap::staged_dir(&f.update_dir())));
}

#[test]
fn a_size_mismatch_is_never_staged() {
    for served in [b"new payload and more".to_vec(), b"new".to_vec()] {
        let f = fixture("2.0.0", b"new payload", served.clone(), None);
        assert!(matches!(f.run(), Err(Error::SizeMismatch)), "{served:?}");
        assert!(!exists(&swap::staged_dir(&f.update_dir())));
    }
}

#[test]
fn an_interrupted_download_never_becomes_staged() {
    let body = vec![7u8; 200_000];
    let f = fixture("2.0.0", &body, body.clone(), Some(90_000));
    assert!(f.run().is_err());
    assert!(
        !exists(&swap::staged_dir(&f.update_dir())),
        "partial file must not be staged"
    );
}

#[test]
fn a_failed_download_keeps_an_older_staged_update() {
    let good = fixture("2.0.0", b"v2", b"v2".to_vec(), None);
    good.run().expect("v2 staged");
    // A newer release, but the download breaks. The old staged update stays whole.
    let body = vec![1u8; 100_000];
    let bad = fixture("3.0.0", &body, body.clone(), Some(10));
    let (k, base) = (&bad.k, &bad.base);
    let mode = UpdateMode::SelfUpdate(good.install.clone());
    let r = check_and_stage(
        &Policy::for_test("1.0.0", TARGET, &format!("{base}/dl/")),
        &k.pubkey,
        &format!("{base}/latest.json"),
        &mode,
        &good.update_dir(),
    );
    assert!(r.is_err());
    assert_eq!(
        swap::read_staged(&good.update_dir()).map(|s| s.0),
        Some("2.0.0".into())
    );
    assert_eq!(
        fs::read(swap::staged_dir(&good.update_dir()).join("payload")).expect("payload"),
        b"v2"
    );
}

#[test]
fn a_notice_only_install_downloads_nothing() {
    let f = fixture("2.0.0", b"x", b"x".to_vec(), None);
    let mode = UpdateMode::NoticeOnly {
        reason: "deb".into(),
    };
    let out = check_and_stage(
        &f.policy(),
        &f.k.pubkey,
        &format!("{}/latest.json", f.base),
        &mode,
        &f.update_dir(),
    )
    .expect("ok");
    let Outcome::Notice {
        version,
        url,
        reason,
    } = out
    else {
        panic!("a notice");
    };
    assert_eq!((version.as_str(), reason.as_str()), ("2.0.0", "deb"));
    assert_eq!(
        url,
        "https://github.com/pauldeng/mqtt-navigator/releases/tag/v2.0.0"
    );
    assert!(!exists(&swap::staging_dir(&f.update_dir())));
    assert!(!exists(&swap::staged_dir(&f.update_dir())));
    assert_eq!(release_page(&"2.0.0".parse().expect("v")), url);
    assert!(navigator_update::is_release_page(&url));
    for bad in [
        "http://github.com/pauldeng/mqtt-navigator/releases/tag/v2",
        "https://github.com/pauldeng/mqtt-navigator/releases/tag/",
        "https://github.com/pauldeng/mqtt-navigator/releases/tag/v2/../../x",
        "https://evil.example/pauldeng/mqtt-navigator/releases/tag/v2",
        "javascript:alert(1)",
    ] {
        assert!(!navigator_update::is_release_page(bad), "{bad}");
    }
}

#[test]
fn a_rolled_back_version_is_not_offered_again() {
    let f = fixture("2.0.0", b"x", b"x".to_vec(), None);
    fs::create_dir_all(f.update_dir()).expect("dir");
    fs::write(f.update_dir().join("failed"), "2.0.0").expect("failed");
    assert_eq!(f.run().expect("ok"), Outcome::UpToDate);
    assert!(!exists(&swap::staged_dir(&f.update_dir())));
}

#[test]
fn a_manifest_with_a_bad_signature_from_the_server_is_an_error() {
    let f = fixture("2.0.0", b"x", b"x".to_vec(), None);
    let other = keys();
    let mode = UpdateMode::SelfUpdate(f.install.clone());
    let r = check_and_stage(
        &f.policy(),
        &other.pubkey,
        &format!("{}/latest.json", f.base),
        &mode,
        &f.update_dir(),
    );
    assert!(matches!(r, Err(Error::BadSignature)));
}

#[test]
fn the_real_manifest_address_is_https_and_plain_http_is_refused_in_production() {
    assert!(
        navigator_update::MANIFEST_URL
            .starts_with(navigator_update::RELEASE_PREFIX.trim_end_matches("download/"))
    );
    let p = Policy::new("1.0.0").expect("p");
    let dir = tempfile::tempdir().expect("dir");
    let r = navigator_update::check(&p, "key", "http://127.0.0.1:1/latest.json", dir.path());
    assert!(matches!(r, Err(Error::BadUrl(_))));
}

// --- detection and folders ---------------------------------------------------------------------

#[test]
fn detect_finds_self_update_and_notice_cases() {
    use navigator_update::detect_from;
    let dir = tempfile::tempdir().expect("dir");
    let app = fs::canonicalize(dir.path()).expect("path").join("app");
    fs::create_dir(&app).expect("dir");
    let exe = app.join(swap::PAYLOAD);
    let UpdateMode::SelfUpdate(i) = detect_from(&exe, None) else {
        panic!("self update");
    };
    assert_eq!((i.kind, &i.path), (Kind::Exe, &exe));
    // A development build: a folder above the exe has the CACHEDIR.TAG that cargo writes in
    // `target/`. Notice only.
    let dev = app.join("target/release");
    fs::create_dir_all(&dev).expect("target");
    fs::write(
        app.join("target/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55",
    )
    .expect("tag");
    let UpdateMode::NoticeOnly { reason } = detect_from(&dev.join(swap::PAYLOAD), None) else {
        panic!("a development build must not update itself");
    };
    assert!(reason.contains("development build"), "{reason}");
    // An AppImage is never a development build.
    let UpdateMode::SelfUpdate(i) = detect_from(
        &dev.join(swap::PAYLOAD),
        Some(app.join("x.AppImage").into()),
    ) else {
        panic!("appimage");
    };
    assert_eq!(i.kind, Kind::AppImage);
    // AppImage: the file is the install.
    let ai = app.join("x.AppImage");
    let UpdateMode::SelfUpdate(i) = detect_from(&exe, Some(ai.clone().into())) else {
        panic!("appimage");
    };
    assert_eq!((i.kind, i.path), (Kind::AppImage, ai));
    // macOS layout.
    let bundle = dir.path().join("Mqtt.app");
    let macos = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos).expect("bundle");
    let UpdateMode::SelfUpdate(i) = detect_from(&macos.join(swap::PAYLOAD), None) else {
        panic!("bundle");
    };
    assert_eq!(i.kind, Kind::App);
    assert_eq!(
        i.run_path(),
        bundle.join("Contents/MacOS").join(swap::PAYLOAD)
    );
    assert_eq!(i.prev(), dir.path().join("Mqtt.app.prev"));
    // A bundle on a disk image.
    let vol = Path::new("/Volumes/Mqtt/Mqtt.app/Contents/MacOS").join(swap::PAYLOAD);
    assert!(matches!(
        detect_from(&vol, None),
        UpdateMode::NoticeOnly { .. }
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn deb_and_read_only_folders_get_a_notice() {
    use navigator_update::detect_from;
    use std::os::unix::fs::PermissionsExt;
    assert!(matches!(
        detect_from(Path::new("/usr/bin/mqtt-navigator"), None),
        UpdateMode::NoticeOnly { .. }
    ));
    let dir = tempfile::tempdir().expect("dir");
    let app = fs::canonicalize(dir.path()).expect("path").join("app");
    fs::create_dir(&app).expect("dir");
    fs::set_permissions(&app, fs::Permissions::from_mode(0o555)).expect("chmod");
    let mode = detect_from(&app.join(swap::PAYLOAD), None);
    fs::set_permissions(&app, fs::Permissions::from_mode(0o755)).expect("chmod back");
    // A root user can write anywhere. Everybody else gets the notice.
    if std::env::var("USER").as_deref() != Ok("root") {
        assert!(matches!(mode, UpdateMode::NoticeOnly { .. }), "{mode:?}");
    }
}

#[test]
fn the_update_code_and_the_app_use_the_same_data_folder() {
    assert_eq!(swap::data_local_dir(), dirs::data_local_dir());
}
