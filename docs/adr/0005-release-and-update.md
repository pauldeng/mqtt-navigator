# ADR 0005: Release packaging and auto-update

Status: accepted (2026-11-11). Advice: Cursor agent (grok-4.7-high); Codex had no credits.

## Decision

### Packaging (D1)

`cargo-packager` 0.11.8 builds:

| OS | Files |
|---|---|
| Windows x64 | per-user NSIS installer, portable zip |
| macOS arm64, x64 | ad-hoc-signed DMG; a zip of the `.app` for updates |
| Linux x64 | AppImage, deb |

No MSI: the update swap needs plain files.

### Updater (D2, D6)

Our own updater in the app, built on `ureq` (native-tls), `minisign-verify` and `sha2`. Not `cargo-packager-updater`: it signs only the artifact, so the version, the target and the size are not signed, and it has no rollback.

- The app applies a staged update itself, at the very start of `main()` (the startup step, see the update of 2026-11-12). It keeps one previous copy and writes a trial state. Version 0.1.0 did this in a separate launcher program. That program is gone.
- The app writes `healthy` after its first window opens. When the next start finds a trial without `healthy`, it restores the previous copy.
- The check runs once at start, never on a timer. A setting turns it off.
- Self-update: the AppImage, the Windows payload, and the macOS `.app`. A deb install, or a folder that the user cannot write, gets a notice with a link to the release instead.
- From pdfcraft (`storytold/craft-libs`, MIT/Apache, patterns): a swap for each install type, and the notice for package-manager-owned or read-only copies.

### Manifest and signing (D3)

- `latest.json`: `version`, and for each Rust target triple: `url`, `sha256`, `size`.
- CI signs the exact bytes of `latest.json` with minisign: `latest.json.minisig` lies beside it. The secret key and its password are GitHub secrets; the public key is in the repository and in the app.
- The app rejects a bad signature, a hash or size mismatch, an unknown target, and any version not newer than the running one.
- `SHA256SUMS` is for people; the app trusts only the signed manifest.

### Windows Authenticode (D4)

On `windows-2022`, sign `mqtt-navigator.exe` and the NSIS installer with the owner's PFX (`signtool`, SHA-256, timestamp), then `signtool verify /pa`.

Update 2026-11-11: the owner has no publicly trusted certificate. A self-signed certificate does not stop the SmartScreen warning, and Azure Artifact Signing and SignPath were not chosen. The Windows files ship unsigned. The workflow signs them only when the secret `WINDOWS_PFX_BASE64` is set. The minisign manifest still protects every update.

### macOS without a Developer ID (D5)

Ad-hoc sign the `.app` from the inside out (`codesign -s -`). Updates swap the bundle by rename and keep one previous bundle. Remove `com.apple.quarantine` from the staged app. A bundle on a disk image (`/Volumes`) or in a folder that the user cannot write gets "Copy this app to Applications". The user guide documents the first launch: right-click, then Open.

### HTTPS client (D7)

`ureq` with native-tls (SChannel, Security.framework, and the vendored OpenSSL that the binary already has). No rustls: a second TLS stack would use most of the size margin (ADR 0004).

### Release workflow (D8)

A `v*` tag workflow beside `ci.yml`: a matrix of `ubuntu-24.04`, `windows-2022` and `macos-15` (one universal app: `aarch64` and `x86_64` built on the arm64 runner and merged with `lipo`; update 2026-11-12, after pdfcraft), `fail-fast: false`, a draft release first, then checksums, minisign, a CycloneDX SBOM, provenance attestations, and `cargo-about` licence text appended to `THIRD-PARTY-NOTICES.md`. Tools pinned with `--locked`; actions pinned by commit.

Without a push we test: Linux packaging, a minisign round trip, the update contract tests, `cargo-about` and the SBOM. `signtool` and the Gatekeeper check run on their own machines.

## Not in scope (D9)

MSI, rpm, Flatpak, winget, scoop, Homebrew, delta updates, notarization, periodic checks, download resume, an in-app changelog, and removal of the 0.1.0 launcher file from an existing Windows install.

## Update 2026-11-12: one program

Status: accepted. This update replaces the launcher part of D2.

### Decision

Each platform ships one program: `mqtt-navigator` (`mqtt-navigator.exe` on Windows). The separate launcher program is deleted.

- **Startup step.** `navigator_update::startup()` is the first call in `main()`. It runs before gpui starts and before the app parses the command line. It uses no network. It runs `swap::prepare`, writes the log lines to `update.log`, and starts another copy when it must. On Unix it replaces the process (`exec`). On Windows it starts the copy detached and ends the process. The copy gets the same arguments.
- **Development builds never update.** `Install::locate` finds no install when a folder above the exe has a `CACHEDIR.TAG` file. Cargo writes this file into `target/`. The notice says "This is a development build. It does not update itself." An AppImage is never a development build.
- **Restart to update.** The app starts itself again (the AppImage file, the program in the `.app`, or the exe) and quits. The new process applies the staged update at its start. Windows allows a rename of a running exe, so the new process moves the old exe to `.prev` while the old process still closes. A test proves this (`windows_rename.rs`).
- **Windows.** The exe uses the GUI subsystem in release builds, so it opens no console. For `--help` and for errors, the app attaches to the console of its parent (`AttachConsole`).
- **Packaging.** The main binary is `mqtt-navigator`. The zip, the tar.gz and the installer have one program.

### New state word

The state file has a new word, `running V`. The new code uses it for its own trials.

| State | Action at the start |
|---|---|
| `swapping V` | The swap was interrupted. Repair it, finish it, and start the install. |
| `swapped V` | First start of V. Write `running V`. Go on. |
| `running V` | `healthy` is V: clear the state. Else: roll back, write `failed`, and start the restored copy. |
| `started V` | The 0.1.0 launcher owns it. Never roll back. `healthy` is V: clear the state. Else: leave it, and apply no update. |

After its own swap, the new code writes `swapped V` for every install kind. Then it starts the new copy.

### Compatibility with 0.1.0

A 0.1.0 install has the launcher. On Windows, the Start-menu shortcut keeps pointing at `mqtt-navigator-launcher.exe`. So the 0.1.0 `prepare` can run at every start, maybe forever. Therefore:

- The update folder format does not change: the file names, the `healthy` content and the meaning of the old state words. The artifacts keep their layout. The Windows zip has one folder with `mqtt-navigator.exe` in it. The `.app.zip` has `Contents/MacOS/mqtt-navigator`.
- The new code never deletes an existing `mqtt-navigator-launcher` file. It starts the sibling `mqtt-navigator.exe`, so the shortcut keeps working. A reinstall with the installer removes it.
- The 0.1.0 launcher reads an unknown state word as "no state". It ignores `running`. It writes `started V` for the Exe and App kinds right before it starts the app, and `swapped V` for an AppImage. The new code never rolls back `started`. The 0.1.0 launcher ends that trial when it finds `healthy`.
- `swapped V` that the 0.1.0 launcher wrote for an AppImage is the first start of V for the new code.
- The 0.1.0 launcher may apply a staged update before the new code runs. It may also turn `swapped V` into `started V`. Both cases work.
- A test (`swap_compat.rs`) runs the real 0.1.0 `swap.rs` (copied from the tag `v0.1.0` into `tests/fixtures/`) against the new code.

### Rollback ceiling

The startup step is part of the app. A new version that cannot reach `main()` (it does not load, or it crashes before the first line) is not rolled back by itself. Only the 0.1.0 launcher can still do this, and only for the trials that it started. A version that reaches `main()` and then fails before the first window is rolled back at the next start. Accepted: the launcher was the only other way, and it needed a second signed program.
