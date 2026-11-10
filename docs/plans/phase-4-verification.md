# Phase 4 verification (profiles and persistence)

Date: 2026-11-11.

## Advisor decisions (Codex)

- D1: `settings.json` in `dirs::config_dir()/mqtt-navigator/`, `schema_version` 1, atomic write (temp file in the same folder, fsync, rename, directory fsync on Unix), a damaged file is kept as `.bad-<time>`, a newer schema makes the app read-only, saves after 500 ms of quiet on one background writer.
- D2: secrets in the OS store through `keyring-core` (Windows Credential Manager, macOS Keychain, Linux Secret Service through zbus), on one background thread; session-only with a notice when no store works.
- D3: one serializable core `Profile` without secrets; runtime `ConnectConfig` with the secrets.
- D4: right-side connection sheet; validation on button press; confirm before a profile switch while connected.
- D5: profile export and import (no secrets, fresh ids, names with suffixes, never overwrite).
- D6: window size, theme, view and row height are restored; quit asks the writer to flush (best effort).
- D7: crash-safety test by subprocess abort at checkpoints; keyring mock not needed.

## Done

- Core: `profile.rs` (model, defaults, port rule, validation, export, import), `settings.rs` (load, migrations table, atomic save with checkpoints), `net.rs` (`ConnectConfig`; clean start, session expiry, keepalive and Will reach CONNECT; key passphrase).
- App: `persist.rs` (writer, debounce, stale temp files), `secrets.rs` (keyring thread, session cache), `connections.rs` (sheet), `ui/profiles.rs` (settings, switching, import, export, empty state), title-bar profile Select with "Manage connections…", theme button, CLI session profile (never saved).

## Checks run

| Check | Result |
|---|---|
| fmt, clippy (`--all-features`, `-D warnings`), cargo-deny | pass |
| `cargo test --workspace` | pass: 124 tests (34 app, 76 core unit, 14 Mosquitto and fake-broker) |
| Broker matrix (Docker) | pass: 15 |
| Crash-safety subprocess test (abort at partial write, after temp sync, after rename) | original file intact before the rename; new file whole after it |
| Will test (TCP cut without DISCONNECT) | a second client receives the Will |
| Smoke: no settings file / existing file + stale temp / CLI args / schema 99 | starts; no file written without a change; temp file removed; CLI password not saved; schema-99 file untouched |
| Release binary | 46.7 MB (P3: 44.2 MB; the Linux secret-service chain is about 1.5 MB) |

## Not checked

- No UI interaction: sheet, Select, dialogs, file pickers, notifications, theme cycle, view and row-height persistence, window-size restore.
- No keyring write or read: this machine has no Secret Service. Windows and macOS keyring code was not compiled (CI only).
- A window-manager resize in the first second after the first frame is ignored (`RESIZE_SETTLE`); a later one is saved as the user's size.

## Known deviations

- A "Save and connect" failure shows in the title bar, not in the sheet (the sheet closes on save). The advisor deferred this.

## Codex review 1: findings and fixes

| Finding | Fix |
|---|---|
| High: saving the active profile changed the shown host while the socket stayed on the old broker | The runtime profile stays unchanged while live. A strip shows "Reconnect to apply your changes." with Reconnect (confirm). |
| High: a switch before the Connect status arrived did not cancel; the old generation passed `accept()` | Every switch and delete calls `net.disconnect()` first. A generation floor (`GenGate`) drops data at or below the highest generation seen before the switch. Test: A connect, switch to B, A rejected, B accepted. |
| A late keyring reply could connect a profile | Keyring reads carry a request token. Switch, delete, edit and Cancel make it stale; a stale reply is only cached. |
| A pending read could restore a cleared password | Edited secrets are flagged; a read never overwrites an edited slot. |
| Password-only profiles lost authentication after a restart | The keyring is always read on connect. |
| A full writer queue could drop the flush on quit | The snapshot and flush requests share one slot that the writer reads after each wake-up. Test with a full queue. |
| Fields over 65 535 bytes panicked the MQTT library | `Profile::validate` and `ConnectConfig::validate` check MQTT length limits (sheet, CLI, and before each connect). |
| Windows: unused `File` import | Fixed. The Windows target check of core needs Perl for vendored OpenSSL on Linux; it was not run. |
| Live subscription edits were not saved (fixed in P4 by advisor decision) | Each accepted add or remove updates the saved profile (not the command-line profile). |
| Import names were checked against an old list | Names are checked again against the current list when the import is applied. |

Tests: 133 (40 app, 79 core unit, 14 Mosquitto and fake-broker). No UI or keyring interaction was run.

## Codex review 2: findings and fixes

| Finding | Fix |
|---|---|
| High: a save before the first Connect status replaced the runtime profile | A Connect without a status counts as live: a save shows the Reconnect notice. The title bar shows "Connecting…" with Cancel. |
| The generation floor admitted a replaced connect (advisor: must change) | The generation is assigned when the UI sends Connect (`Net::connect` returns it). The net thread uses it and ignores older ones. The UI accepts only its current generation; switch, delete and cancel clear it. Tests: unit test and a Mosquitto test with a queued Subscribe. |
| Offline removal with a persistent session | Removals made while offline are kept and sent as UNSUBSCRIBE after a CONNACK with session present; dropped after a clean session. Mosquitto test with a resumed session. |
| Flaky `many_filters_and_a_slow_ui_lose_no_result` | The documented "Too many messages are waiting" rejection counts as the single result. 10 runs pass. |

Open: an UNSUBSCRIBE that the broker handled at the moment the connection died can stay in the retry list.

## Codex review 3: findings and fixes

| Finding | Fix |
|---|---|
| Removals after a manual disconnect were lost on a resumed session | The supervisor keeps `maybe_subscribed` for each broker session (host, port, client id; at most 8). It survives Disconnect and Connect. |
| A live UNSUBSCRIBE lost with the connection was never retried | A filter leaves the set only after a successful UNSUBACK. After a CONNACK with session present, each filter in the set that is not desired gets a quiet UNSUBSCRIBE. A clean session clears the set. |
| Open item (an UNSUBSCRIBE that the broker handled as the connection died) | Accepted: a retry is harmless (`NoSubscriptionExisted` counts as success). |

Tests: Mosquitto tests for a removal after a manual disconnect, a lost UNSUBSCRIBE, and a clean session; they fail without the fix.

## Codex review 4: finding and fix

| Finding | Fix |
|---|---|
| The persistent-session filter set was lost across an app restart | `Profile.session_filters` is saved. The net thread reports the set (`Event::SessionFilters`, at most every 500 ms and at the end of an attempt). `ConnectConfig.session_filters` seeds a fresh net thread. A host, port or client-id change clears it. Test: a fresh `Net` between the removal and the reconnect; it fails without the seed. |

## Codex review 5: finding and fix

| Finding | Fix |
|---|---|
| A new profile on a shared session (for example an imported copy with the same client id) never saved the cached set | Each connection reports its full session-filter set once after its first CONNACK (persistent sessions only). Test: `a_new_profile_on_a_shared_session_gets_the_cached_set`; it fails without the fix. |

## Codex review 6: finding and fix

| Finding | Fix |
|---|---|
| A confirmed reconnect of the same profile (Reconnect, or Save and connect) erased the messages, the selection and the composer | `switch_to` only cancels the old connection when the profile is the same. `clear_data` runs only for a different profile. Old-generation data is dropped by the gate. |
