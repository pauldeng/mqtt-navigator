# Security

## Report a problem

Do not open a public issue for a security problem. Use GitHub private vulnerability reporting on this repository ("Security" tab → "Report a vulnerability").

Give the version, the operating system and the steps to reproduce the problem.

## Security design

- TLS is on by default for a new connection. If you select an unencrypted transport (TCP or WS), the app shows a "Not encrypted" warning.
- TLS certificate checks are on by default. If you turn them off, the app shows an "Insecure TLS" warning tag.
- Passwords and key passphrases go only into the system keyring. If there is no keyring, the app keeps them in memory for the session only.
- Logs never contain payloads or secrets.
- The app makes invisible and direction characters visible, for example `⟨U+202E⟩`, so that a broker cannot make a topic look like another one. It does this for topics, filters, property names and values, the reason text of the broker, and the composer fields (topic, content type, response topic and user properties) that come from a received message (Publish to this topic, Reply, Edit and publish). In a composer field, the original text stays until you edit that field, and Send uses the original. Payload previews in the lists escape direction controls only. The payload body view shows the decoded text as it is. Copy topic and Copy value copy the original text.
- The command line has no password option, because an argument shows in the process list and in the shell history. Give the password on the standard input (`--password-stdin`) or in the environment variable `MQTT_NAVIGATOR_PASSWORD`. The environment variable is less safe.
- On Linux and macOS, the app creates its settings, log and crash folders with mode 0700 and its files with mode 0600, and it sets mode 0700 on its own settings and log folders at start. On Windows, files stay in the user profile and use its default access list.
- A hostile broker cannot stop the client with a malformed PUBLISH. A QoS 2 PUBLISH, or a reused packet ID without the DUP flag, ends the session with a plain error text. The app does not connect again by itself.
- The app sends no telemetry. The only traffic other than the broker is the update check, and you can turn it off.
- Updates are verified with a minisign signature and a SHA-256 hash before the app installs them. The app rejects older versions.
