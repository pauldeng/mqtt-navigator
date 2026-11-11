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

- A small dependency-free **launcher** is the program the user starts. It applies a staged update, keeps one previous copy, writes `pending = started`, and starts the app (the payload).
- The app writes `healthy` after its first window opens. When the launcher finds `started` without `healthy`, it restores the previous copy.
- The check runs once at start, never on a timer. A setting turns it off.
- Self-update: the AppImage, the Windows payload, and the macOS `.app`. A deb install, or a folder that the user cannot write, gets a notice with a link to the release instead.
- From pdfcraft (`storytold/craft-libs`, MIT/Apache, patterns): a swap for each install type, and the notice for package-manager-owned or read-only copies.

### Manifest and signing (D3)

- `latest.json`: `version`, and for each Rust target triple: `url`, `sha256`, `size`.
- CI signs the exact bytes of `latest.json` with minisign: `latest.json.minisig` lies beside it. The secret key and its password are GitHub secrets; the public key is in the repository and in the app.
- The app rejects a bad signature, a hash or size mismatch, an unknown target, and any version not newer than the running one.
- `SHA256SUMS` is for people; the app trusts only the signed manifest.

### Windows Authenticode (D4)

On `windows-2022`, sign the launcher, the payload and the NSIS installer with the owner's PFX (`signtool`, SHA-256, timestamp), then `signtool verify /pa`. Move to Azure Trusted Signing when the certificate renews.

### macOS without a Developer ID (D5)

Ad-hoc sign the `.app` from the inside out (`codesign -s -`). Updates swap the bundle by rename and keep one previous bundle. Remove `com.apple.quarantine` from the staged app. A bundle on a disk image (`/Volumes`) or in a folder that the user cannot write gets "Copy this app to Applications". The user guide documents the first launch: right-click, then Open.

### HTTPS client (D7)

`ureq` with native-tls (SChannel, Security.framework, and the vendored OpenSSL that the binary already has). No rustls: a second TLS stack would use most of the size margin (ADR 0004).

### Release workflow (D8)

A `v*` tag workflow beside `ci.yml`: a matrix of `ubuntu-24.04`, `windows-2022` and `macos-15` (arm64, plus a cross-built `x86_64-apple-darwin`), `fail-fast: false`, a draft release first, then checksums, minisign, a CycloneDX SBOM, provenance attestations, and `cargo-about` licence text appended to `THIRD-PARTY-NOTICES.md`. Tools pinned with `--locked`; actions pinned by commit.

Without a push we test: Linux packaging, a minisign round trip, the update contract tests, `cargo-about` and the SBOM. `signtool` and the Gatekeeper check run on their own machines.

## Not in scope (D9)

MSI, rpm, Flatpak, winget, scoop, Homebrew, delta updates, notarization, periodic checks, download resume, an in-app changelog, and self-update of the Windows launcher.
