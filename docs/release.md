# Release

## Cut a release

1. Set `version` in the workspace `Cargo.toml`. Run `cargo check` to update `Cargo.lock`.
2. Commit and merge to `main`. CI must pass.
3. Tag the commit: `git tag v0.2.0 && git push origin v0.2.0`. The tag must equal `v` plus the crate version. The workflow stops when they differ.
4. Wait for the `Release` workflow. It makes a **draft** release and attaches all files.
5. Open the draft. Read the notes. Press **Publish release**.

The app reads `latest.json` from the latest published release. A draft is not visible to the app. Users get the update only after you publish.

## What the workflow makes

| Target | Files |
|---|---|
| Linux x64 | `.AppImage`, `.deb`, `.tar.gz` |
| Windows x64 | `-setup.exe` (per-user), `.zip` (portable) |
| macOS arm64 and x64 | `.dmg`, `.app.zip` |
| All | `SHA256SUMS`, `latest.json`, `latest.json.minisig`, SBOM (`*.cdx.json`), `THIRD-PARTY-NOTICES.md`, build attestations |

The AppImage, the Windows `.zip` and the macOS `.app.zip` are the update files. `latest.json` lists them.

## Secrets

Add these in **Settings > Secrets and variables > Actions**.

| Secret | Content |
|---|---|
| `MINISIGN_SECRET_KEY` | The text of the minisign secret key file. |
| `MINISIGN_PASSWORD` | The password of that key. |
| `WINDOWS_PFX_BASE64` | The Authenticode certificate (PFX), base64: `base64 -w0 cert.pfx`. |
| `WINDOWS_PFX_PASSWORD` | The password of the PFX. |

The workflow uses `GITHUB_TOKEN` for everything else. Never put a key in the repository. The repository holds only the public key, `assets/update/minisign.pub`.

## Rotate the minisign key

The public key is compiled into the app. An old app trusts only the old key.

1. Make a new pair: `minisign -G -p assets/update/minisign.pub -s new.key`.
2. Replace the two minisign secrets.
3. Release one version signed with the **old** key that contains the **new** public key. Wait until users have it.
4. Release the next version signed with the new key.

If the old key is lost or stolen, step 3 is not possible. Tell users to install the new version by hand.

## First start on a user machine

- **Windows.** The installer and the programs have an Authenticode signature. SmartScreen can still warn about a new publisher until the file gains reputation. Choose **More info**, then **Run anyway**.
- **macOS.** The app has an ad-hoc signature, not a Developer ID. It is not notarized. Gatekeeper blocks the first start. Right-click the app, choose **Open**, then choose **Open** again. If macOS says the app is damaged, run `xattr -dr com.apple.quarantine "/Applications/MQTT Navigator.app"`. Copy the app to `/Applications` before the first start.
- **Linux.** Run `chmod +x` on the AppImage. The `.deb` installs the app but does not update it. The app shows a link to the new release.

## Build the packages on your machine

```bash
cargo install cargo-packager --locked --version 0.11.8
cargo build --release -p navigator-app -p navigator-launcher
cargo packager --release -p navigator-app --formats appimage,deb   # Linux
```

The files appear in `dist/`. The packager does not build the programs. Build both first. `cargo packager` starts the launcher (`mqtt-navigator-launcher`) from the shortcut, the desktop entry and the `.app`.

Other tools: `cargo about generate about.hbs` (licence text, see `about.toml`) and `cargo cyclonedx --format json --describe binaries --target all` (SBOM). The `sign-manifest.sh` script needs `minisign`.

## Not covered

- Notarization and a Developer ID for macOS.
- MSI, rpm, Flatpak, winget, scoop, Homebrew.
- Delta updates, periodic update checks, update of the Windows launcher.
- The Windows and macOS steps run only in GitHub Actions. Nobody tested them without a push.
