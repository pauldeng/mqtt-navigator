# User guide

This guide tells you how to install and use MQTT Navigator. For a short overview, read the [README](../README.md). For the design, read [ARCHITECTURE.md](../ARCHITECTURE.md).

Contents:
[Install](#install) · [First run](#first-run) · [Connections](#connections) · [Subscribe](#subscribe) · [View messages](#view-messages) · [Publish](#publish) · [Memory limit](#memory-limit) · [Themes](#themes-and-appearance) · [Updates](#updates) · [Keyboard](#keyboard-shortcuts) · [Command line](#command-line-options) · [Logs](#logs-and-diagnostics) · [Limits](#limits) · [Troubleshooting](#troubleshooting)

## Install

Download the file for your system from [GitHub Releases](https://github.com/pauldeng/mqtt-navigator/releases). Each release has a `SHA256SUMS` file. Use it to check your download.

### Windows 10 and later (x64)

1. Download `-setup.exe` (installer) or `.zip` (portable).
2. For the installer: run it. It installs for your user only. It does not need administrator rights.
3. For the portable file: unpack the zip to a folder you can write to. Start `mqtt-navigator.exe`.

The files have no Authenticode signature yet, so SmartScreen warns about an unknown publisher. Select **More info**, then **Run anyway**. Check the file against `SHA256SUMS` on the release page first.

### macOS 15 and later (Apple silicon and Intel)

1. Download the `.dmg`. One app runs on Apple silicon and on Intel.
2. Open the DMG. Drag **MQTT Navigator** to **Applications**.
3. Right-click the app and select **Open**. Select **Open** again in the dialog.

The app has an ad-hoc signature. It has no Developer ID and is not notarized. Gatekeeper therefore blocks a normal first start. You must use right-click, then **Open**, one time.

If macOS says the app is damaged, run this command in Terminal:

```bash
xattr -dr com.apple.quarantine "/Applications/MQTT Navigator.app"
```

Copy the app to `/Applications` before you start it. An app that runs from the disk image cannot update itself.

### Linux x64 (Ubuntu 24.04 or similar)

Your system needs Vulkan (`libvulkan1`) and a Wayland or X11 session.

- **AppImage:** run `chmod +x MQTT-Navigator-*.AppImage`. Then start the file. The AppImage updates itself.
- **deb:** run `sudo apt install ./mqtt-navigator_*.deb`. The deb does not update itself. See [Updates](#updates).

**App menu and dock icon.** On first start, the AppImage and the bare program add themselves to your app menu. This lets the dock and the task bar show the icon. The app writes `~/.local/share/applications/io.github.pauldeng.mqtt-navigator.desktop` and the icons `~/.local/share/icons/hicolor/<size>/apps/io.github.pauldeng.mqtt-navigator.png` (48, 128, 256 and 512 px). The entry starts the AppImage file, or else the program itself. The app does this once, in the background. If the `.deb` is installed, the app does not add an entry. It removes the entry and the icons that it added before, because the `.deb` has its own. If you move or rename the AppImage, the next start of the moved file repairs the entry. The app never changes an entry that starts another program. The app adds no entry if the path has a `%` character, because GNOME cannot start such a path from an entry.

To remove the app from your app menu, delete the AppImage or the program. Then delete these files (use `$XDG_DATA_HOME` instead of `~/.local/share` if you set it):

```bash
rm ~/.local/share/applications/io.github.pauldeng.mqtt-navigator.desktop
rm ~/.local/share/icons/hicolor/*/apps/io.github.pauldeng.mqtt-navigator.png
```

If you start the app again, it adds the entry again. On X11 the window icon comes from the app itself. On macOS, a program that does not run from the `.app` shows a generic Dock icon.

## First run

1. Start the app. It opens with the message "Add a broker to start." The screen also has **Connect to localhost:1883 (not encrypted)** for a broker on your own computer.
2. Select **New connection…** (`Ctrl/Cmd+N`). Fill in the host. Select **Save and connect**.
3. A new connection subscribes to `#`. Messages appear at once.
4. Select a row. The details pane shows the payload.

To test without a broker of your own, use a public test broker. Use TLS, and do not send private data.

## Connections

Open the sheet with **Manage connections…** in the profile menu of the title bar. The sheet lists the saved connections. **New connection** is the last row.

### Basic fields

| Field | Notes |
|---|---|
| Name, Host | Free text and the broker address. |
| Transport | TLS (default), WSS, TCP, WS. |
| Port | Follows the transport: TLS 8883, WSS 443, TCP 1883, WS 80. The port changes only if it is still the default of the old transport. |
| Username, Password | Optional. |
| Client ID | The app makes one. You can edit it. |

**Save and connect** is also the connection test. If it fails, the title bar shows the cause. **Save** keeps the connection and does not connect.

### Encryption

- TLS is on by default. It checks the server certificate.
- If you select TCP or WS, the sheet shows "Not encrypted. Passwords and messages are sent in clear text." The status bar shows a "Not encrypted" tag for this connection.
- If you turn off **Verify the server certificate**, the sheet shows "Certificate verification is off." The status bar shows an "Insecure TLS" tag. Use this only for tests.

### Certificates

Under **TLS** (TLS and WSS only):

- **CA file:** a PEM file of the CA that signed the broker certificate. The app also trusts the system roots.
- **Client certificate and key:** PEM files for mutual TLS. Set both or neither. For an encrypted key file, type its passphrase in **Key passphrase**. The app keeps it in the system keyring.

Select a file with the system file dialog.

### Passwords

- The app saves the password and the key passphrase in the system keyring: Windows Credential Manager, macOS Keychain, or the Linux Secret Service.
- The settings file never has a secret.
- If the system has no keyring, the app says "Password not saved: no system keyring. It is kept for this session only." (for a key passphrase: "Key passphrase not saved: …"). The app keeps the secret in memory until you quit.
- A password from the command line is also session-only.

### Advanced (collapsed)

Select **Advanced (show)** to open this part.

| Field | Default | Notes |
|---|---|---|
| WebSocket path | `/mqtt` | For WS and WSS. It must start with `/`. |
| Clean start | on | The broker keeps no session for you. |
| Session expiry (seconds) | 0 | Seconds the broker keeps the session after a disconnect. |
| Keep alive (seconds) | 60 | 0 to 65535. |
| Will topic, Will payload, Will QoS, Retain the will message, Will delay (seconds) | no will | The broker sends the will message when the connection ends without a DISCONNECT. An empty Will topic means no will message. |

### Lose no messages during an outage

By default, a connection uses clean start and QoS 0 subscriptions. In this setup the broker keeps nothing for you while you are offline. To keep messages during a short outage:

1. Turn off **Clean start**.
2. Set **Session expiry** to the time the broker must keep the session (for example 3600).
3. Subscribe with **QoS 1**.
4. Keep the same **Client ID**.

The broker then stores QoS 1 messages while the app is offline and sends them after the reconnect. The defaults stay clean start and QoS 0, because a persistent session can fill the broker.

QoS 1 does not guarantee that the app shows each message. If the app queue or the memory limit drops a message, the app still acknowledges it. The status bar counts these drops.

### Other actions

- **Export…** saves the connection profiles as JSON. The file has no secrets.
- **Import…** reads such a file. The app never overwrites a connection. It adds a suffix such as " (2)" to a name that exists.
- **Delete connection…** also removes the saved password.
- If you save a connection that is active, the status bar shows "Reconnect to apply your changes." Select **Reconnect**.
- If you select another connection while connected, the app asks first. Messages of the old connection are removed.
- The app reconnects by itself after a lost connection ("Reconnecting (3)…"). The wait grows from 1 s to 30 s. It also retries when the broker is not reachable or answers "server unavailable", "server busy", "quota exceeded" or "connection rate exceeded". It does not retry for a wrong user name or password, "not authorized", a ban, a host name that is not found, or a certificate error.

## Subscribe

The left pane lists the subscriptions.

1. Type a topic filter in the input. Press **Enter**.
2. Set the QoS with the **Q0 | Q1** switch before you press Enter. The default is QoS 0.

Rules:

- A new connection starts with `#`. When you add a narrower filter, a hint asks "Remove # to receive only your filters?".
- An invalid filter (for example `a/#/b`) shows an error under the input. The app does not send it. A filter that is in the list shows "You are already subscribed to …".
- A row has a colour dot, the filter, a **Q0** or **Q1** tag and a status line. The tag is the QoS that you asked for. The row has no message count.
- The status line shows "Subscribing…" (or "Waiting for connection" while you are offline). Then it shows "granted 0" or "granted 1". This is the QoS that the broker granted. It can be lower than the tag.
- If the broker rejects a filter, the status line shows the reason in red. Select **Retry** or **Remove**. The SUBACK reasons show as:
  - "Not authorized to subscribe."
  - "The broker says the topic filter is not valid."
  - "The subscription quota is exceeded."
  - "The broker does not support shared subscriptions."
  - "The broker does not support wildcard subscriptions."
  - "The broker refused the subscription: <reason name>." (any other reason)
  - "The broker sent an empty SUBACK."
- If the broker does not answer in 10 s, see [Troubleshooting](#other-connection-problems).
- **Remove** (×) unsubscribes. The row stays until the broker confirms. If the removal fails, the row shows the cause and **Retry remove**.
- **Show only:** select a row to show only the messages whose topic matches its filter. Select it again to show all. This does not unsubscribe.
- The colour dot is not a control. The colour comes from the position of the row and repeats after 10 rows. If you remove a row, the rows below it move up and change colour. A message takes the colour of the first subscription (from the top) that matches its topic. A message that matches no subscription has an empty dot.
- Shared subscriptions (`$share/group/filter`) work.

The app saves the subscriptions in the connection.

## View messages

The centre pane has two views. Switch with **Messages | Tree**, `Ctrl/Cmd+1` or `Ctrl/Cmd+2`. The app remembers your choice.

### Messages

- The newest message is at the top. The order is always by time. There is no sorting.
- Each row shows time, subscription colour, topic, QoS, retained mark (R), size and a preview of the payload.
- Sent messages show in the same list, with an arrow, "Sent" and the result of the send.
- Marker rows show "Reconnected" with the time, and "Removed (memory limit)".
- **All | Received | Sent** filters the list.
- **Row height:** the header button shows "1 line" or "2 lines". Select it to switch. The default is 2 lines. With 1 line, the preview follows the topic. With 2 lines, the preview is on the second line. A long payload ends with "…". A payload that is not valid UTF-8 shows as `bin` and at most its first 16 bytes in hex, for example `bin ff 00 10`.

### Tree

- The tree is sorted a–z. Each row shows the topic name, "N topics" when topics with messages are below it (not on a leaf), a preview of the latest payload and the number of messages of the topic and all its subtopics. The tree has no activity dot and no retained marker. The Messages list shows the retained mark (R).
- Click a row. Then use Up and Down to move. Use Left and Right to collapse and expand a topic that has subtopics.

### Filters

The filter field is in the title bar (`Ctrl/Cmd+F`).

| Filter | Messages view | Tree view |
|---|---|---|
| Text | Topic, or text in the first 1 KiB of the payload. The match ignores case. | Topic path. The match ignores case. Matches keep their parents. |
| Show only | Messages whose topic matches that subscription | Topics that match that subscription |
| All, Received, Sent | Yes | Not shown |

Each active filter is a tag under the header. **Clear filters** removes all of them. `Esc` in the filter field clears the text.

### Pause and Follow

- **Pause** freezes the view. The button then reads **Resume**. The app still receives messages inside the memory limit. The header shows how many are new, for example "Paused · 1 240 new".
- **Follow** (Messages view only, on by default) keeps the newest row in view. Selecting a row or scrolling turns Follow off. The reading position then stays still.
- If the memory limit removed messages while you paused, a marker row says "Older messages were removed (memory limit)".

### Details pane

The right pane has two tabs: **Value** and **History**. The **Details** button in the header shows or hides it. It opens as a sheet when the window is narrower than 1 100 px. Use the expand button (tooltip "Expand the details") to widen it.

- **Value:** the full payload. Choose a **decoder**: Auto, Text, JSON, Hex, Base64 or Number. Auto decides in this order:
  1. JSON, if the content type is `application/json` or ends with `+json`, or if the payload starts and ends like `{…}` or `[…]` (spaces around it do not count).
  2. Text, if the payload format flag says UTF-8, or if the payload is valid UTF-8.
  3. Hex, for all other payloads.

  Auto never picks Number. For Number, set the type (int8, uint8, int16, uint16, int32, uint32, int64, uint64, float32 or float64) and the byte order (LE or BE).
- If a decoder cannot show the payload, the pane shows the cause and **Use Hex**.
- JSON shows pretty-printed. **Wrap** turns line wrapping on or off. **Copy** copies the whole formatted text. **Copy topic** copies the topic.
- **Long payloads:** the pane shows the first 256 KiB and a **Show all** button. It formats in the background. For very long text, it shows the first part and **Show more (+N KiB)**. Each press adds more.
- MQTT 5 properties show under the value: content type, response topic, correlation data (hex), message expiry, payload format and user properties.
- **History:** the last 100 messages of the topic (each stored up to 64 KiB). Select a row to show it in the Value tab. You can also click in the list, use Up and Down to move the cursor, and press Enter to show the row in the Value tab.
- If the memory limit removed the selected message, the pane says "Removed from the message list (memory limit)."

## Publish

The composer is at the bottom of the centre pane. It is always visible.

1. Type the topic.
2. Type the payload.
3. Set QoS 0 or 1, and **Retain** if you need it.
4. Press **Send** or `Ctrl/Cmd+Enter`.

The result shows below the payload and in the list row. It is short: "Sending…", "Sent" (QoS 0), "Acknowledged" (QoS 1), or "Rejected: <ReasonName>", for example "Rejected: NotAuthorized". If the broker sent a reason string, the text is "Rejected: <ReasonName>: <reason string>". The app shows no hex code. A message can also end with "No acknowledgement after 10 s." or "Not sent: the connection closed." The app checks the message before it sends it. An error then shows under the composer, for example "Connect to a broker first." When you are not connected, Send is off. The 10 s timeout also ends the session (see [Troubleshooting](#other-connection-problems)).

- **Text or Hex:** a Hex payload sends exact bytes. Write the bytes as hex digits. Spaces are ignored.
- **Format** pretty-prints JSON. It works up to 1 MiB.
- **Properties** (MQTT 5): content type, response topic, correlation data (hex), message expiry, **Payload format: UTF-8** and user properties. **Send empty** keeps an empty property in the message.
- **History** menu: **Drafts** (the last 5 drafts that you replaced) and **Sent** (the last 20 publishes of this session). Choose one to fill the composer. A draft leaves **Drafts** when you choose it. The app does not save History to disk. It clears History when you change the connection.
- The composer saves the old content as a draft before something replaces it. These actions replace it: a choice in History, Edit and publish, Reply, Publish to this topic, Format and the Text/Hex switch. The app saves the old content only if it is unsent, it is not empty (a topic alone is empty) and it is at most 1 MiB. A larger draft is dropped, and the app says "The old draft is larger than 1 MiB. It is not kept."
- Drafts and Sent share a budget of 1 MiB. The app removes the oldest entries first.
- The composer can collapse to one line.

**Publish to this topic**, **Edit and publish** and **Reply** fill the composer. They never send. **Clear retained…** is different: after you confirm, it publishes an empty retained message at QoS 1.

| Action | Effect |
|---|---|
| Publish to this topic | Sets the topic. Keeps the payload. |
| Edit and publish | Copies topic, exact payload bytes, QoS, retain and properties. A payload that is not valid UTF-8 opens in Hex mode. Off when the full payload is not stored ("The full payload is not stored."). |
| Reply | Shows only when the message has a Response Topic. Sets that topic and copies the correlation data. The payload is empty. |
| Clear retained… | Shows only while you are connected and the topic has a retained message. After you confirm, it sends an empty retained message at QoS 1. |

## Memory limit

The app keeps all messages inside one memory limit. The default is 128 MiB. The settings file accepts 32 MiB to 2 GiB. The palette and the status-bar memory menu offer 64 MiB, 128 MiB, 256 MiB, 512 MiB, 1 GiB and 2 GiB. A change applies at once and the app saves it.

When the store is full:

- The app removes the oldest data in batches, down to 90% of the limit. It removes the oldest history first, then the oldest list rows, then the oldest latest values.
- Rarely updated topics leave the tree when the topic count is near its cap.
- Saving a message never blocks. Above the limit plus 25%, the app drops new messages and counts them.
- The status bar shows the use against the limit, for example "34 MiB / 128 MiB". A limit of 1024 or 2048 MiB shows as "1 GiB" or "2 GiB", for example "340 MiB / 1 GiB".
- The Messages list keeps 10 000 rows at most. The status bar shows "N rows evicted" when rows leave.

## Themes and appearance

- **Appearance:** System (default), Light or Dark. System follows the operating system.
- **Theme:** Default, Catppuccin, Solarized, Everforest, Flexoki, Mellifluous, macOS Classic, Tokyo Night. Each theme has a light and a dark variant. Tokyo Night uses the Default light variant.
- Use the theme button in the title bar, or the command palette ("Appearance: …", "Theme: …").
- The app saves the choice and applies it before the first window.
- The layout follows the font size of the theme.

The theme files keep the licences of their projects. See [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md).

## Updates

- The app checks for a new version once at start. It has no timer. Use **Check for updates…** in the command palette to check at any time.
- To turn off the check at start, use "Turn off the update check at start" in the command palette.
- The check reads `latest.json` from the latest published GitHub release. The file has a minisign signature, and the app verifies it. The app also checks the SHA-256 hash and the size of the download. It rejects an older or equal version.
- When an update is ready, the title bar shows "Update 0.2.0 ready". Select it, then **Restart to update** (or **Later**). The app never restarts by itself.
- The app keeps one previous copy. If the new version never opens a window, the next start restores the old one.
- **Windows (installer), AppImage, macOS app:** the app updates itself.
- **Version 0.1.0 on Windows:** the shortcut starts `mqtt-navigator-launcher.exe`. It still works: it starts `mqtt-navigator.exe`. A reinstall with the installer removes it.
- **A development build** (started from `target/`) never updates itself.
- **deb install, or a folder you cannot write to:** the app cannot update itself. The title bar shows "Update <version> available", for example "Update 0.2.0 available". Point at it to see why. Select it to open the release page. Install the new deb yourself.
- The update check is the only network traffic other than your broker. The app sends no telemetry.

## Keyboard shortcuts

`Ctrl` is `Cmd` on macOS.

| Keys | Action |
|---|---|
| `Ctrl+K` | Command palette. Most actions are also here. Not in the palette: subscribe, remove and retry a subscription, the publish QoS and Retain, Reconnect, and the connection sheet actions (Save, Export, Import, Delete). |
| `Ctrl+F` | Focus the filter |
| `Esc` | In the filter: clear it. Elsewhere: close the top layer (menu, dialog, sheet). |
| `Ctrl+1` / `Ctrl+2` | Messages view / Tree view |
| `Ctrl+Enter` | Send (in the composer) |
| `Ctrl+N` | New connection |
| `Ctrl+Q` | Quit |
| `Tab` / `Shift+Tab` | Next / previous control. In the composer, it moves focus and does not type a tab. |
| `Up` / `Down` | Select the previous / next row in the Messages list or the tree. Click a row first. The first key press selects the top row. |
| `Left` / `Right` | In the tree only: collapse / expand the selected topic |
| `Up`, `Down`, `Enter` (History tab) | Move the cursor, and show the row in the Value tab. Click the History list first. |

`Enter` does nothing in the Messages list and in the tree. Pause, Follow, Edit and publish, Reply, Publish to this topic and the details pane have no shortcut. Use the buttons or the command palette. Pane widths change with the mouse. "Reset the layout" is in the command palette.

## Command line options

```
mqtt-navigator [--host HOST] [--port PORT] [--tcp | --ws | --wss] [--insecure]
               [--ca FILE] [--user NAME] [--password-stdin] [--sub FILTER[:QOS]]...
```

| Option | Meaning |
|---|---|
| `--host HOST` | Broker host. |
| `--port PORT` | Port, 1 to 65535. The default follows the transport. |
| `--tcp`, `--ws`, `--wss` | Transport. The default is TLS on port 8883. `--tcp` is not encrypted. |
| `--insecure` | Do not verify the server certificate. |
| `--ca FILE` | CA file (PEM). |
| `--user NAME` | User name. |
| `--password-stdin` | Read the password from the first line of the standard input, before the window opens. The app removes the line ending. An empty line means no password. |
| `--sub FILTER[:QOS]` | A subscription. QoS is 0 or 1. Repeat the option for more. |
| `-h`, `--help` | Show the usage and exit with code 2. |

With options, the app makes a session connection called "Command line". The app never saves it or its password. A wrong option prints the usage and exits with code 2.

**Windows.** `mqtt-navigator.exe` is a windowed program. In a terminal, `--help` and error messages print in that terminal. The terminal does not wait for the app: the prompt comes back at once. Use `start /wait mqtt-navigator.exe ...` in `cmd` to wait. `--password-stdin` reads a piped standard input as on other systems.

There is no `--password PASS` option. A password in an argument shows in the process list (`/proc/<pid>/cmdline`) and in the shell history. The app prints a message and exits with code 2 if you use it.

Give the password in one of two ways:

```
printf '%s\n' "$PASSWORD" | mqtt-navigator --host b.example --user u --password-stdin
```

- `--password-stdin` is the safer way. The password does not show in the process list or in the shell history.
- The environment variable `MQTT_NAVIGATOR_PASSWORD` also sets the password. It is less safe: other programs of the same user can read the environment of a process, and a shell can keep it. `--password-stdin` wins if you use both. The variable has an effect only when you give other options.

Prefer a saved connection.

The environment variable `MQTT_NAVIGATOR_LOG` sets the log level (for example `MQTT_NAVIGATOR_LOG=debug`).

## Logs and diagnostics

The log folder is:

| OS | Folder |
|---|---|
| Windows | `%LOCALAPPDATA%\mqtt-navigator\logs` |
| macOS | `~/Library/Application Support/mqtt-navigator/logs` |
| Linux | `~/.local/share/mqtt-navigator/logs` |

- The log file is `mqtt-navigator.log`. It has a size cap of 5 MiB, plus one older file (`.1`). A crash writes a crash file in the same folder.
- Logs never contain payloads, passwords or keys.
- Use **Open the log folder** in the command palette to open it.
- Use **Copy diagnostics** in the command palette to copy a short report: version, operating system, CPU architecture, graphics adapter, connection state and counters. It has no connection, host, path, topic, payload or secret. Paste it in a bug report.

The settings file is `settings.json` in the `mqtt-navigator` folder of your system configuration folder (Linux: `~/.config`, macOS: `~/Library/Application Support`, Windows: `%APPDATA%`). The app keeps a damaged file as `.bad-<time>` and starts with defaults. A file from a newer version is not loaded and stays untouched. The app uses the default settings and does not save. It shows: "The settings file is from a newer version of MQTT Navigator (schema N). It was not changed. Default settings are in use, and changes are not saved." The app also does not save if it cannot read the file.

## Limits

- MQTT 5.0 only. A broker that supports only MQTT 3.1.1 cannot connect.
- QoS 0 and QoS 1 only. The MQTT library has no QoS 2.
- The largest incoming packet is 1 MiB. The app closes the connection if a packet is larger.
- No proxy support.
- No message export. Only connection profiles export.
- No accessibility support for screen readers.
- One connection at a time.
- English only.

## Troubleshooting

### TLS errors

| Message | What to do |
|---|---|
| The server certificate is not trusted. Add its CA file, or turn off verification. | Set **CA file** to the CA that signed the broker certificate. For a test only, turn off verification. |
| The server certificate is for another host name. | Use the host name that is in the certificate. |
| The server certificate expired on …. Renew it, or turn off verification. | Renew the certificate on the broker. |
| The server certificate is not valid yet. Check the clock of this computer and of the broker. | Correct the clock. |
| The broker needs a client certificate. Add your certificate and key. | Set the client certificate and key. |
| The client certificate and key do not match. | Choose the key that belongs to the certificate. |
| The client key is encrypted. Enter the key passphrase, or use a key without a passphrase. | Type the passphrase in **Key passphrase**. |
| The client key passphrase is wrong, or the key is not valid. | Correct the passphrase, or choose another key. |
| The client certificate file has no certificate. / The CA file … has no certificate. | Choose a PEM file that has a certificate. |

The app does not retry after a certificate error. Fix the cause and connect again.

### Other connection problems

- **The broker refused the connection. Check the host and the port.** The broker is off, or the host or port is wrong. The app retries with growing waits (1 s to 30 s).
- **The broker rejected the connection: …** The broker gave a reason. For a temporary reason (server unavailable, server busy, quota exceeded, connection rate exceeded), the app retries with growing waits. For any other reason, for example a wrong user name or password, "not authorized" or a ban, the app does not retry.
- **The broker closed the connection.** The connection dropped (for example a broker restart or a network cut). The app reconnects with growing waits. Select **Cancel** to stop.
- **The broker closed the connection: …** The broker sent a DISCONNECT with a reason. For a temporary reason, the app reconnects. For any other reason, the session ends as Failed and the app does not retry. Connect again after you fix the cause.
- **The broker sent a packet that is larger than the allowed size.** The broker sent a packet above 1 MiB. The app does not retry.
- **The broker sent data that is not valid MQTT 5.** The broker broke the MQTT 5 protocol. The app does not retry. A broker that supports only MQTT 3.1.1 shows a different message: **The broker rejected the connection: the broker does not support MQTT 5**.
- **The broker did not answer the connection request.** The broker sent no CONNACK in 10 s. The app retries with growing waits.
- **No answer from the broker after 10 s.** A subscribe or an unsubscribe got no answer. The row shows this text (for an unsubscribe: "Cannot unsubscribe from <filter>: No answer from the broker after 10 s."). Then the session fails with "The broker did not answer for 10 s. Connect again." Connect again.
- **No acknowledgement after 10 s.** A publish got no acknowledgement. The app ends the session in the same way: "The broker did not answer for 10 s. Connect again."
- **The broker did not answer the keep-alive ping.** The app reconnects with growing waits.

### The broker does not allow `#`

Some brokers reject the `#` filter. The row shows a reason, for example "Not authorized to subscribe." Remove `#`. Subscribe to a narrower filter.

### The "dropped" counter is above 0

The app received more messages than it could keep. The reasons are a very high message rate, or a full queue or store. The counter is red when it is not 0.

- Subscribe to a narrower filter.
- Raise the [memory limit](#memory-limit).
- Do not pause during a long flood.

### The window does not open

The app draws with the GPU (Vulkan on Linux). On a virtual machine or a remote desktop, the GPU can be missing. Install a software renderer (for example Mesa lavapipe on Linux). Check the log folder for an error. The software-renderer case is not tested yet.

### More help

Open an issue on GitHub. Add the output of **Copy diagnostics**. Do not add payloads or passwords. For a security problem, read [SECURITY.md](../SECURITY.md).
