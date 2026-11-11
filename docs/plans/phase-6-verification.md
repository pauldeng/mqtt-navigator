# Phase 6 verification (release and auto-update)

Date: 2026-11-11. Decisions: ADR 0005 (advisor: Cursor agent, grok-4.7-high).

## Done

- Packaging (`cargo-packager` 0.11.8): NSIS per-user installer and portable zip (Windows), ad-hoc-signed DMG and `.app.zip` (macOS), AppImage and deb (Linux). The installers start the launcher `mqtt-navigator-launcher`; the app binary `mqtt-navigator` is the payload. Icons made from `assets/icon/icon-1024.png`.
- Updater: `crates/navigator-update` (signed manifest check, download with size and SHA-256 checks, staging, install-type detection) and `crates/navigator-launcher` (std only: applies a staged update, keeps one previous copy, rolls back when the new version never reports healthy). App: one check at start (setting, palette entries), "Update ready" badge, restart only on user request.
- Release workflow `.github/workflows/release.yml`: draft release, 4-target build matrix, Authenticode signing with `signtool verify /pa`, `SHA256SUMS`, signed `latest.json`, CycloneDX SBOM, `cargo-about` notices, build provenance attestations. `scripts/make-manifest.py`, `scripts/sign-manifest.sh`, `docs/release.md`.
- Release key: the minisign public key is `assets/update/minisign.pub`; the secret key and its password are outside the repository (owner's machine, then GitHub secrets).

## Checks run

| Check | Result |
|---|---|
| fmt, clippy (`-D warnings`), cargo-deny | pass |
| `cargo test --workspace` | pass: 243 tests, including 20 update-contract tests, 7 swap tests, 1 launcher process test, and a test that the committed key is a real public key |
| Local packaging (Linux) | deb 14.4 MB, AppImage 14.4 MB; contents inspected |
| Minisign round trip with the real key | a signature verifies; a tampered file fails |
| `cargo about`, `cargo cyclonedx` | pass |
| Release binary | 34 798 520 B (limit 37 317 165 B) |

## Incident

The updater agent replaced the real public key with a placeholder comment while the packaging agent worked in parallel. Found before the commit. The public key was recreated from the secret key (`minisign -R`) and checked with a test signature. The new `release_key` test now fails if the committed key is not a real key.

## Not checked (needs a pushed tag)

- Windows: signtool, NSIS, zip extraction by the updater.
- macOS: DMG, `.app.zip`, the x86_64 cross build, bundle swap with `ditto`, Gatekeeper first run.
- Attestations, the draft release upload, and `minisign` 0.11 on the Ubuntu runner.
- `actionlint` was not run.
- An AppImage that cannot start at all after an update is not rolled back (the launcher is inside the image).

## Review 1 (Cursor agent, grok-4.7-high): findings and fixes

Confirmed by the reviewer: the signature is checked before any manifest field; downgrade and same-version rejection; 256 MB cap; hash before staging; quarantine removal only on the staged app; arguments passed with no shell; manifest fields match the parser; the workflow (pinned actions, permissions per job, tag equals version, no publish after a failed matrix leg, signature over the uploaded `latest.json`).

| Finding | Fix |
|---|---|
| Rollback deleted the live copy before restoring `.prev` (a crash left nothing to start) | `restore_prev`: rename the bad copy aside, rename `.prev` into place, then delete the aside copy; the state changes only after that. Crash tests at each step for an exe, an `.app` and an AppImage. |
| `staged/` could stay after the swap and be applied again; `failed` was not read | `staged/` is removed and checked before the trial state; a cross-volume copy finishes from `<path>.new` after a crash; a staged update with the failed version is never applied. Test on a real second volume (`/dev/shm`). |
| Archive extraction did not check members (`..`, absolute paths, links outside) | Members are listed and checked before extraction; extraction goes into an empty folder; the extracted tree is checked without following links; links are allowed only inside an `.app` bundle. Tests for each case. |
