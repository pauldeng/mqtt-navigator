# Security

## Report a problem

Do not open a public issue for a security problem. Use GitHub private vulnerability reporting on this repository ("Security" tab → "Report a vulnerability").

Give the version, the operating system and the steps to reproduce the problem.

## Security design

- TLS is on by default for a new connection. If you select an unencrypted transport (TCP or WS), the app shows a "Not encrypted" warning.
- TLS certificate checks are on by default. If you turn them off, the app shows a red badge.
- Passwords and key passphrases go only into the system keyring. If there is no keyring, the app keeps them in memory for the session only.
- Logs never contain payloads or secrets.
- The app sends no telemetry. The only traffic other than the broker is the update check, and you can turn it off.
- Updates are verified with a minisign signature and a SHA-256 hash before the app installs them. The app rejects older versions.
