//! The download: stream to `staging/`, check size and SHA-256, then rename to `staged/`.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::swap::{self, Install, Kind};
use crate::{Error, Policy, Release, Result, agent, http_err};

pub fn download(policy: &Policy, release: &Release, install: &Install, dir: &Path) -> Result<()> {
    let version = release.version.to_string();
    let want = &release.artifact;
    policy.check_url(&want.url)?;
    // The same update is staged already: nothing to do.
    if swap::read_staged(dir).is_some_and(|(v, t)| v == version && t == install.path) {
        return Ok(());
    }
    let staging = swap::staging_dir(dir);
    swap::remove_any(&staging)?;
    fs::create_dir_all(&staging)?;
    let part = staging.join("download.part");

    let body = agent(policy)
        .get(&want.url)
        .call()
        .map_err(http_err)?
        .into_body();
    // One byte more than expected: a longer body is caught, and nothing reads without a limit.
    let mut reader = body.into_reader().take(want.size + 1);
    let mut file = File::create(&part)?;
    let mut hash = Sha256::new();
    let (mut buf, mut total) = (vec![0u8; 64 * 1024], 0u64);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        total += n as u64;
    }
    file.sync_all()?;
    drop(file);
    if total != want.size {
        return Err(Error::SizeMismatch);
    }
    let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if !hex.eq_ignore_ascii_case(&want.sha256) {
        return Err(Error::HashMismatch);
    }

    let payload = staging.join("payload");
    if install.kind == Kind::App {
        extract_app(&part, &staging, &payload)?;
    } else if is_zip(&part)? {
        // Windows: a zip with one folder. Only the payload is taken from it.
        extract_payload(&part, &staging, &payload)?;
    } else {
        fs::rename(&part, &payload)?;
    }
    #[cfg(unix)]
    if install.kind != Kind::App {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&payload, fs::Permissions::from_mode(0o755))?;
    }
    swap::commit_staged(dir, &version, &install.path)?;
    Ok(())
}

fn is_zip(file: &Path) -> Result<bool> {
    let mut magic = [0u8; 4];
    Ok(File::open(file)?.read_exact(&mut magic).is_ok() && &magic == b"PK\x03\x04")
}

/// The archive tool. macOS: `ditto` keeps links and permissions. Windows 10 and later have
/// `tar`, which reads zip files. No zip library: the SHA-256 of the signed manifest already
/// proves that the archive is ours.
///
/// The hash does not make the content safe to unpack with a tool that trusts member names, so
/// there are three checks (a hostile or a broken archive can write outside the folder):
/// 1. Before extraction, `tar -tf` and `tar -tvf` list the members. It rejects an absolute
///    name, a `..` part, a drive letter, and any member that is not a plain file or folder
///    (a hard link, a device, and a symbolic link). With `links` (the `.app`), a symbolic link
///    is allowed when its target is relative and stays below the top folder, and no later
///    member is written through it.
/// 2. The tool extracts into a new empty folder.
/// 3. After extraction, [`check_tree`] walks the folder without following links. It rejects
///    everything but files and folders. The `.app` bundle may keep symbolic links (frameworks
///    have some), but only when the target stays inside the bundle.
fn unzip(zip: &Path, out: &Path, links: bool) -> bool {
    if !members_ok(zip, links) {
        return false;
    }
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("ditto");
        c.args(["-x", "-k"]).arg(zip).arg(out);
        c
    } else {
        let mut c = std::process::Command::new("tar");
        c.arg("-xf").arg(zip).arg("-C").arg(out);
        c
    };
    cmd.status().is_ok_and(|s| s.success())
}

fn tar_list(flag: &str, zip: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("tar")
        .arg(flag)
        .arg(zip)
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    out.status
        .success()
        .then(|| text.lines().map(String::from).collect())
}

fn members_ok(zip: &Path, links: bool) -> bool {
    match (tar_list("-tf", zip), tar_list("-tvf", zip)) {
        (Some(names), Some(long)) => listing_ok(&names, &long, links),
        _ => false,
    }
}

/// `names`: one member for each line (`tar -tf`). `long`: the same members (`tar -tvf`), whose
/// first character is the type. A line count that differs (a name with a new line) is a reject.
fn listing_ok(names: &[String], long: &[String], links: bool) -> bool {
    if names.is_empty() || names.len() != long.len() || !names.iter().all(|n| name_ok(n)) {
        return false;
    }
    let mut link_names: Vec<&str> = Vec::new();
    for (name, line) in names.iter().zip(long) {
        let name = name.trim_end_matches('/');
        // Written through an earlier link: the name leaves the folder.
        if link_names.iter().any(|l| {
            name.strip_prefix(l)
                .is_some_and(|rest| rest.starts_with('/'))
        }) {
            return false;
        }
        match line.chars().next() {
            Some('-' | 'd') => {}
            Some('l') if links => match line.rsplit_once(" -> ") {
                Some((_, target)) if target_ok(name, target) => link_names.push(name),
                _ => return false,
            },
            _ => return false,
        }
    }
    true
}

/// A relative link target that stays below the top folder of the member `name`.
fn target_ok(name: &str, target: &str) -> bool {
    let mut depth = name.split('/').filter(|p| !matches!(*p, "" | ".")).count();
    depth = depth.saturating_sub(1);
    if target.starts_with('/') || target.contains('\\') {
        return false;
    }
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." if depth > 1 => depth -= 1,
            ".." => return false,
            _ => depth += 1,
        }
    }
    true
}

fn name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !(name.is_empty()
        || name.starts_with(['/', '\\'])
        || (b.len() > 1 && b[1] == b':')
        || name.split(['/', '\\']).any(|part| part == ".."))
}

/// Rejects everything under `root` that is not a file or a folder. Nothing is followed. With
/// `links`, a symbolic link is allowed when its target stays inside `root`.
fn check_tree(root: &Path, links: bool) -> bool {
    let Ok(canon) = fs::canonicalize(root) else {
        return false;
    };
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else { return false };
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_dir() {
                todo.push(path);
            } else if kind.is_symlink() {
                if !(links && link_inside(&path, root, &canon)) {
                    return false;
                }
            } else if !kind.is_file() {
                return false;
            }
        }
    }
    true
}

/// The link target is relative, never leaves `root` by its own `..` parts, and the whole chain
/// resolves (the real path) to a place inside `root`.
fn link_inside(link: &Path, root: &Path, canon_root: &Path) -> bool {
    use std::path::Component;
    let (Ok(target), Some(parent)) = (fs::read_link(link), link.parent()) else {
        return false;
    };
    let Ok(rel) = parent.strip_prefix(root) else {
        return false;
    };
    let mut depth = rel.components().count();
    for part in target.components() {
        match part {
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::CurDir => {}
            _ => return false,
        }
    }
    fs::canonicalize(link).is_ok_and(|real| real.starts_with(canon_root))
}

/// macOS: the artifact is a zip of the `.app`.
fn extract_app(zip: &Path, staging: &Path, payload: &Path) -> Result<()> {
    let out = staging.join("unzipped");
    fs::create_dir(&out)?;
    // A real folder named `*.app` (a link of that name is not one).
    let apps: Vec<_> = if unzip(zip, &out, true) {
        fs::read_dir(&out)?
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "app"))
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .collect()
    } else {
        Vec::new()
    };
    match apps.as_slice() {
        [app] if check_tree(&app.path(), true) => {
            fs::rename(app.path(), payload)?;
            fs::remove_file(zip)?;
            swap::remove_any(&out)?;
            Ok(())
        }
        _ => Err(bad_archive()),
    }
}

/// Windows: a zip with one top-level folder that holds the payload and the launcher. Only the
/// payload is taken. The launcher is never replaced. The zip has no links.
fn extract_payload(zip: &Path, staging: &Path, payload: &Path) -> Result<()> {
    let out = staging.join("unzipped");
    fs::create_dir(&out)?;
    let found = (unzip(zip, &out, false) && check_tree(&out, false))
        .then(|| find_payload(&out))
        .flatten();
    let Some(found) = found else {
        return Err(bad_archive());
    };
    fs::rename(found, payload)?;
    fs::remove_file(zip)?;
    swap::remove_any(&out)?;
    Ok(())
}

/// `PAYLOAD` in `root` or in one folder below it. Only a regular file counts: a link is not
/// followed.
fn find_payload(root: &Path) -> Option<std::path::PathBuf> {
    let plain = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.is_file());
    let direct = root.join(swap::PAYLOAD);
    if plain(&direct) {
        return Some(direct);
    }
    let mut dirs = fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()));
    let (first, rest) = (dirs.next()?, dirs.next());
    let inner = first.path().join(swap::PAYLOAD);
    (rest.is_none() && plain(&inner)).then_some(inner)
}

fn bad_archive() -> Error {
    Error::Io(std::io::Error::other("The update archive is not valid."))
}

#[cfg(test)]
mod tests {
    use super::{check_tree, extract_app, extract_payload, find_payload, listing_ok, name_ok};
    use crate::swap::PAYLOAD;

    #[test]
    fn the_payload_is_found_in_the_one_folder_of_the_zip() {
        let d = tempfile::tempdir().expect("dir");
        assert!(find_payload(d.path()).is_none());
        let inner = d.path().join("mqtt-navigator-1.0.0");
        std::fs::create_dir(&inner).expect("dir");
        std::fs::write(inner.join("LICENSE"), "x").expect("file");
        assert!(find_payload(d.path()).is_none(), "no payload in the folder");
        std::fs::write(inner.join(PAYLOAD), "x").expect("file");
        assert_eq!(find_payload(d.path()), Some(inner.join(PAYLOAD)));
        // Two folders: not an update archive of ours.
        std::fs::create_dir(d.path().join("other")).expect("dir");
        assert!(find_payload(d.path()).is_none());
    }

    #[test]
    fn member_names_and_types_are_checked_before_extraction() {
        for bad in ["/abs", "\\abs", "../x", "a/../../x", "a\\..\\x", "C:/x", ""] {
            assert!(!name_ok(bad), "{bad:?}");
        }
        for good in ["a/b", "./a", "a/..b/c", "M.app/Contents/"] {
            assert!(name_ok(good), "{good:?}");
        }
        let v = |l: &[&str]| l.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let n = v(&["M.app/a", "M.app/b"]);
        let file = "-rw-r--r-- 0 u/g 1 d M.app/a";
        let dir = "drwxr-xr-x 0 u/g 0 d M.app/b";
        let link = |t: &str| format!("lrwxrwxrwx 0 u/g 0 d M.app/b -> {t}");
        assert!(listing_ok(&n, &v(&[file, dir]), false));
        // A link: never for a zip payload, and for a bundle only when the target stays inside.
        assert!(!listing_ok(&n, &v(&[file, &link("a")]), false));
        assert!(listing_ok(&n, &v(&[file, &link("a")]), true));
        for t in ["/", "/etc", "../x", "../../x", "a/../../x", "..\\x"] {
            assert!(!listing_ok(&n, &v(&[file, &link(t)]), true), "{t}");
        }
        let hard = "hrw-r--r-- 0 u/g 0 d M.app/b link to a";
        assert!(!listing_ok(&n, &v(&[file, hard]), true));
        assert!(!listing_ok(&n, &v(&[file]), true), "counts differ");
        assert!(!listing_ok(&[], &[], true));
        // A member written through an earlier link.
        let n = v(&["M.app/l", "M.app/l/x"]);
        let through = v(&[
            "lrwxrwxrwx 0 u/g 0 d M.app/l -> .",
            "-rw-r--r-- 0 u/g 1 d M.app/l/x",
        ]);
        assert!(!listing_ok(&n, &through, true));
    }

    #[cfg(unix)]
    #[test]
    fn links_in_an_extracted_tree_are_checked_without_following_them() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().expect("dir");
        std::fs::create_dir(d.path().join("outside")).expect("dir");
        let root = d.path().join("M.app");
        let fw = root.join("Contents/Frameworks/F.framework");
        std::fs::create_dir_all(fw.join("Versions/A")).expect("tree");
        std::fs::write(root.join("Contents/Info.plist"), "x").expect("file");
        // The usual framework layout: allowed with `links`, never without.
        std::fs::write(fw.join("Versions/A/F"), "x").expect("file");
        symlink("A", fw.join("Versions/Current")).expect("link");
        symlink("Versions/Current/F", fw.join("F")).expect("link");
        symlink("../../../../Info.plist", fw.join("Versions/A/i")).expect("link");
        assert!(check_tree(&root, true));
        assert!(!check_tree(&root, false));
        // Each of these leaves the bundle.
        for (i, target) in ["../outside", "/etc/passwd", "../../.."].iter().enumerate() {
            let l = root.join(format!("bad{i}"));
            symlink(target, &l).expect("link");
            assert!(!check_tree(&root, true), "{target}");
            std::fs::remove_file(l).expect("rm");
        }
        // A chain: the first link is fine, the second goes up through it.
        symlink("Contents", root.join("c")).expect("link");
        symlink("c/../../outside", root.join("chain")).expect("link");
        assert!(!check_tree(&root, true), "chain");
    }

    /// The archive tests use `tar` to build and to unpack (a tar archive; the real artifact is
    /// a zip). They run where `tar` is also the extraction tool: not on macOS (`ditto`).
    #[cfg(all(unix, not(target_os = "macos")))]
    mod archives {
        use super::*;
        use std::path::{Path, PathBuf};
        use std::process::Command;

        /// Builds `<tmp>/a.tar` from `members`, relative to `cwd`.
        fn archive(tmp: &Path, cwd: &Path, extra: &[&str], members: &[&str]) -> PathBuf {
            let out = tmp.join("a.tar");
            let status = Command::new("tar")
                .arg("-cf")
                .arg(&out)
                .args(extra)
                .args(members)
                .current_dir(cwd)
                .status()
                .expect("tar");
            assert!(status.success(), "tar cannot build the test archive");
            out
        }

        fn app_src(tmp: &Path) -> PathBuf {
            let src = tmp.join("src");
            std::fs::create_dir_all(src.join("M.app/Contents/MacOS")).expect("tree");
            std::fs::write(src.join("M.app/Contents/MacOS").join(PAYLOAD), "new").expect("file");
            src
        }

        fn run_app(tmp: &Path, zip: &Path) -> crate::Result<()> {
            let staging = tmp.join("staging");
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir(&staging).expect("dir");
            extract_app(zip, &staging, &staging.join("payload"))
        }

        #[test]
        fn a_good_app_bundle_with_an_inner_link_is_accepted() {
            let t = tempfile::tempdir().expect("dir");
            let src = app_src(t.path());
            std::os::unix::fs::symlink("MacOS", src.join("M.app/Contents/Current")).expect("link");
            let zip = archive(t.path(), &src, &[], &["M.app"]);
            run_app(t.path(), &zip).expect("accepted");
            let p = t.path().join("staging/payload");
            assert!(p.join("Contents/MacOS").join(PAYLOAD).is_file());
            let link = std::fs::symlink_metadata(p.join("Contents/Current")).expect("link");
            assert!(link.is_symlink());
        }

        #[test]
        fn a_bundle_link_to_outside_is_rejected() {
            let t = tempfile::tempdir().expect("dir");
            let src = app_src(t.path());
            std::os::unix::fs::symlink("../../../../x", src.join("M.app/Contents/out"))
                .expect("link");
            let zip = archive(t.path(), &src, &[], &["M.app"]);
            assert!(run_app(t.path(), &zip).is_err());
            assert!(!t.path().join("staging/payload").exists());
        }

        #[test]
        fn a_top_level_link_named_app_is_rejected() {
            let t = tempfile::tempdir().expect("dir");
            let src = t.path().join("src");
            std::fs::create_dir(&src).expect("dir");
            std::os::unix::fs::symlink(".", src.join("M.app")).expect("link");
            let zip = archive(t.path(), &src, &[], &["M.app"]);
            assert!(run_app(t.path(), &zip).is_err());
        }

        #[test]
        fn dot_dot_and_absolute_members_are_rejected_before_anything_is_written() {
            let t = tempfile::tempdir().expect("dir");
            let src = app_src(t.path());
            let deep = src.join("deep");
            std::fs::create_dir(&deep).expect("dir");
            std::fs::write(src.join("x"), "evil").expect("file");
            // `../M.app` and `../x`, stored from the folder `deep`.
            let dotdot = archive(t.path(), &deep, &["-P"], &["../M.app", "../x"]);
            assert!(run_app(t.path(), &dotdot).is_err(), "../");
            // An absolute member name (`-P` keeps the slash).
            let abs = src.join("x");
            let abs = archive(t.path(), &src, &["-P"], &[abs.to_str().expect("utf8")]);
            assert!(run_app(t.path(), &abs).is_err(), "absolute");
            assert!(!t.path().join("staging/x").exists());
            let unzipped = t.path().join("staging/unzipped");
            assert_eq!(std::fs::read_dir(unzipped).expect("dir").count(), 0);
        }

        #[test]
        fn a_zip_payload_with_a_link_is_rejected_and_a_plain_one_is_taken() {
            let t = tempfile::tempdir().expect("dir");
            let src = t.path().join("src");
            std::fs::create_dir_all(src.join("d")).expect("tree");
            std::fs::write(src.join("d").join(PAYLOAD), "new").expect("file");
            let run = |zip: &Path| {
                let staging = t.path().join("staging");
                let _ = std::fs::remove_dir_all(&staging);
                std::fs::create_dir(&staging).expect("dir");
                extract_payload(zip, &staging, &staging.join("payload")).is_ok()
            };
            assert!(run(&archive(t.path(), &src, &[], &["d"])));
            // The payload is a link to a file outside.
            std::fs::remove_file(src.join("d").join(PAYLOAD)).expect("rm");
            std::os::unix::fs::symlink("/etc/passwd", src.join("d").join(PAYLOAD)).expect("link");
            assert!(!run(&archive(t.path(), &src, &[], &["d"])));
        }
    }
}
