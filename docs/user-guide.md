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
3. Start the app. macOS blocks it. Open **System Settings**, then **Privacy & Security**. Scroll down to the message about MQTT Navigator. Select **Open Anyway**, then confirm.

The app has an ad-hoc signature. It has no Developer ID and is not notarized. Gatekeeper therefore blocks the first start. You must select **Open Anyway** in **Privacy & Security** one time, after the blocked start.

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

1. Start the app. It opens with the message "Add a broker to start". The screen also has **Connect to localhost:1883 (not encrypted)** for a broker on your own computer.
2. Select **New connection…** (`Ctrl/Cmd+N`). Fill in the host. Select **Save and connect**. If you select **Save**, the connection becomes the active one and the screen shows its name and address. Then select **Connect**.
3. A new connection subscribes to `#`. Messages appear at once.
4. Select a row. In a wide window, the details pane opens and shows the payload.

To test without a broker of your own, use a public test broker. Use TLS, and do not send private data.

## Connections

The connection menu is the button at the left of the title bar. It shows the active connection: its name and its address. Select it to see all connections. A check mark shows the active one. The menu also has **Edit "name"…**, **New connection…** and **Manage connections…**. **Manage connections…** opens the sheet. The sheet lists the saved connections. **New connection** is the last row. The sheet title says "New connection" or "Edit name".

When no connection is active, the button says **Choose a connection**. When no connection exists, it says **Add a connection**.

The main screen also shows the active connection when it is not connected. It has the name, the address, **Connect** and **Edit connection…**. If a connection fails, the screen shows the reason and **Try again**. If messages are on screen, a banner above the list shows the same. A connection that you start with command line options is not saved. The menu shows "Command line: not saved" for it.

### Basic fields

| Field | Notes |
|---|---|
| Name, Host | Free text and the broker address. |
| Transport | TLS (default), WSS, TCP, WS. |
| Protocol | MQTT 3.1.1 (default for a new connection) or MQTT 5. Set it to the version that the broker supports. A connection that you saved before this setting existed uses MQTT 5. See [MQTT 3.1.1 connections](#mqtt-311-connections). |
| Port | Follows the transport: TLS 8883, WSS 443, TCP 1883, WS 80. The port changes only if it is still the default of the old transport. |
| Username, Password | Optional. |

**Save and connect** is also the connection test. If it fails, the title bar shows the cause. **Save** keeps the connection and does not connect.

### Encryption

- TLS is on by default. It checks the server certificate.
- If you select TCP or WS, the sheet shows "Not encrypted. Passwords and messages are sent in clear text." The status bar shows a "Not encrypted" tag for this connection.
- If you turn off **Verify the server certificate**, the sheet shows "Certificate verification is off." The status bar shows an "Insecure TLS" tag. Use this only for tests.

### Certificates

Under **TLS** (TLS and WSS only):

- **CA file:** a PEM file of the CA that signed the broker certificate. Without a CA file, the app trusts the CA certificates of the operating system (the Windows certificate store, the macOS keychain, or the Linux CA files; `SSL_CERT_FILE` and `SSL_CERT_DIR` also work). With a CA file, the app trusts only that file. It does not trust the system CA certificates then.
- **Client certificate:** a collapsed section. Open it to set the certificate and key (PEM files for mutual TLS). Set both or neither. For an encrypted key file, type its passphrase in **Key passphrase**. The app keeps it in the system keyring.

Select a file with the system file dialog.

### Passwords

- The app saves the password and the key passphrase in the system keyring: Windows Credential Manager, macOS Keychain, or the Linux Secret Service.
- The settings file never has a secret.
- If the system has no keyring, the app says "Password not saved: no system keyring. It is kept for this session only." (for a key passphrase: "Key passphrase not saved: …"). The app keeps the secret in memory until you quit.
- A password from the command line is also session-only.

### Advanced (collapsed)

Select **Advanced (show)** to open this part. If a field in a collapsed section is not valid, the sheet opens that section and moves the focus to the field.

| Field | Default | Notes |
|---|---|---|
| Client ID | generated | The app makes one. Edit it here. Keep the same ID to keep a session. |
| WebSocket path | `/mqtt` | For WS and WSS. It must start with `/`. |
| Clean start | on | The broker keeps no session for you. |
| Session expiry (seconds) | 0 | Seconds the broker keeps the session after a disconnect. |
| Keep alive (seconds) | 60 | 0 to 65535. The app sends a ping each interval. It ends a connection that does not answer. This takes about 2 times the keep alive, plus timer and shutdown delays (at most 145 s at the default). With 0, the app sends no ping and cannot find a silent broker, unless an MQTT 5 broker sets its own keep alive. The sheet then shows a note under the field. See [Connection details](#connection-details). |
| Will topic, Will payload, Will QoS, Retain the will message, Will delay (seconds) | no will | The broker sends the will message when the connection ends without a DISCONNECT. An empty Will topic means no will message. |

### MQTT 3.1.1 connections

When **Protocol** is MQTT 3.1.1, the app hides the MQTT 5 features:

- The sheet shows **Clean session** instead of **Clean start**. It has no **Session expiry** and no **Will delay**. The broker keeps the session when **Clean session** is off.
- The composer has no **Properties** section. A message with MQTT 3.1.1 has no properties.
- **Edit and publish** copies the topic, the payload, the QoS and retain. **Reply** uses the Response Topic as the topic. On MQTT 3.1.1 both actions drop the properties. If the message had a property, they show "MQTT 3.1.1 does not send properties."
- The details pane shows "No properties (MQTT 3.1.1)." for a message without properties.
- A message can be at most 1 MiB. The composer shows "MQTT 3.1.1 messages can be at most 1 MiB in this app." before it sends a larger message.

If the broker does not support the version, the connection fails with a text that names the **Protocol** setting.

### Lose no messages during an outage

By default, a connection uses clean start and QoS 0 subscriptions. In this setup the broker keeps nothing for you while you are offline. To keep messages during a short outage:

1. Turn off **Clean start** (**Clean session** for MQTT 3.1.1).
2. MQTT 5 only: set **Session expiry** to the time the broker must keep the session (for example 3600). MQTT 3.1.1 has no session expiry: the broker decides.
3. Subscribe with **QoS 1**.
4. Keep the same **Client ID**.

The broker then stores QoS 1 messages while the app is offline and sends them after the reconnect. The defaults stay clean start and QoS 0, because a persistent session can fill the broker.

QoS 1 does not guarantee that the app shows each message. If the app queue or the memory limit drops a message, the app still acknowledges it. The status bar counts these drops.

### Other actions

- **Export…** saves the connection profiles as JSON. The file has no secrets.
- **Import…** reads such a file. The app never overwrites a connection. It adds a suffix such as " (2)" to a name that exists.
- **Delete connection…** also removes the saved password.
- If you save a connection that is active, the status bar shows "Reconnect to apply your changes." Select **Reconnect**.
- If you choose another connection while you are not connected, the app only selects it. Select **Connect** to connect. If you choose another connection while connected, the app asks first. Messages of the old connection are removed.
- The app reconnects by itself after a lost connection ("Reconnecting (3)…"). The wait grows: the base doubles from 1 s to 30 s, and the real wait is 0.8 s to 36 s. It also retries when the broker is not reachable or answers with one of these reasons: the server is not available, the server is busy, a quota is exceeded, or the connection rate is exceeded. It does not retry for a wrong user name or password, a client that is not authorized, a ban, a host name that is not found, or a certificate error.

## Subscribe

The left pane lists the subscriptions.

1. Type a topic filter in the input.
2. Set the QoS with the **Q0 ▾** select at the end of the input. The default is QoS 0.
3. Press **Enter**.

Rules:

- A new connection starts with `#`. When you add a narrower filter, a hint asks "Remove # to receive only your filters?".
- An invalid filter (for example `a/#/b`) shows an error under the input. The app does not send it. A filter that is in the list shows "You are already subscribed to …".
- A row has a status dot, the filter, a **Q1** tag (only for QoS 1) and a **⋯** button. The row has no message count. The dot has the colour of the subscription. A red ✗ replaces it when the broker rejected the filter.
- A status line shows under the row only when there is something to say: "Subscribing…" (or "Waiting for connection" while you are offline), the rejection reason, or "Granted QoS 0" when the broker grants less than you asked for. If the broker refuses the filter, the row shows the reason, for example "Refused (0x80)" for MQTT 3.1.1.
- The **⋯** button (tooltip "Subscription actions") opens a menu: **Show only**, **QoS 0**, **QoS 1**, **Options…** (MQTT 5 only), **Retry** (only when the broker rejected the filter) and **Remove**. The button is in the Tab order. The same actions are in the command palette.
- A QoS change sends the subscription again, and the row shows "Subscribing…".
- If the broker rejects a filter, the status line shows the reason in red. Choose **Retry** or **Remove** in the menu. The SUBACK reasons show as:
  - "Not authorized to subscribe."
  - "The broker says the topic filter is not valid."
  - "The subscription quota is exceeded."
  - "The broker does not support shared subscriptions."
  - "The broker does not support wildcard subscriptions."
  - "The broker refused the subscription: <reason name>." (any other reason)
  - "The broker sent an empty SUBACK."
- If the broker does not answer in 10 s, see [Troubleshooting](#other-connection-problems).
- **Remove** unsubscribes. The row stays until the broker confirms. If the removal fails, the row shows the cause and the menu has **Retry remove**.
- **Show only:** select a row, or choose **Show only** in its menu, to show only the messages whose topic matches its filter. Do it again to show all. This does not unsubscribe.
- The colour comes from the position of the row and repeats after 10 rows. If you remove a row, the rows below it move up and change colour. A message takes the colour of the first subscription (from the top) that matches its topic. With one subscription, the message rows show no colour edge.
- Shared subscriptions (`$share/group/filter`) work.

#### Subscription options (MQTT 5)

With MQTT 5, the **⋯** menu of a row has **Options…**. MQTT 3.1.1 has no such options, so the item is not there.

- **No Local:** the broker does not send you the messages that you publish yourself. MQTT 5 does not allow this option on a shared subscription. The app shows "No Local is not allowed on a shared subscription ($share/...)." and does not send the change.
- **Retain As Published:** the broker keeps the retain flag of a message when it forwards it. Without this option, the broker clears the flag on a live message.
- **Retained messages at subscribe:** "Always" sends the retained message each time you subscribe. "If new" sends it only if the subscription did not exist. "Never" does not send it.

Each change sends the subscription again, and the row shows "Subscribing…". Retain handling acts when the broker takes the SUBSCRIBE. To skip the retained message of a topic, set "Never" on its row. The app saves the options with the subscription. The app sends the saved options again after each reconnect. The add input has no options: add the row, then set the options.

The app saves the subscriptions in the connection.

## Connection details

The status bar has a **Details** button. While you are connected, its popover starts with **Connection details**, a list of values from the broker's CONNACK. Below that, it shows the topic count, the memory use with the budget control, and the dropped count.

- MQTT 3.1.1: "Session present" only.
- MQTT 5: "Session present", "Assigned client ID", "Keep alive", "Receive maximum", "Maximum packet size", "Maximum QoS", "Retained messages", "Wildcard subscriptions", "Shared subscriptions", "Subscription identifiers", "Topic alias maximum", "Server reference", "Response information" and "Reason string".
- These lines always show: "Session present", "Keep alive", "Receive maximum", "Maximum QoS", "Retained messages", "Wildcard subscriptions", "Shared subscriptions", "Subscription identifiers" and "Topic alias maximum". If the broker did not send one of the last seven values, its line shows the MQTT 5 default. The other lines show only when the broker sent the value. The list does not show Session Expiry Interval or user properties.
- "Keep alive" is the keep alive in use: the value of the broker (Server Keep Alive) if the broker sent one, else your value. The "Keep alive" value "off" means 0. With keep alive off, the app finds a lost connection only when an operation times out or the system reports an error.

If a broker refuses the connection or closes it with a server reference, the error text ends with "Server reference: …". The reference names another server. The app does not connect to it by itself.

## View messages

The centre pane has two views. Switch with **Messages | Tree**, `Ctrl/Cmd+1` or `Ctrl/Cmd+2`. The app remembers your choice.

### Messages

- The header has one row: **Messages | Tree**, the filter field, the direction menu and **Pause**.
- The newest message is at the top. The order is always by time. There is no sorting.
- Each message has two lines. Line 1 shows the topic, **R** if the message is retained, and the time. Line 2 shows a preview of the payload. QoS and size are in the details pane. If you have more than one subscription, a colour edge at the left shows the subscription.
- Sent messages show in the same list, with an arrow. A sent message shows its result only when the result is not routine: "Sending…", "Rejected: <reason>", "Timed out" or "Lost on disconnect". "Sent" and "Acknowledged" show in the details and in the composer.
- Marker rows show "Reconnected" with the time, and "Removed (memory limit)".
- The **All ▾** menu next to the filter selects All, Received or Sent. A choice other than All shows as a tag.
- **Row height:** "1 line" or "2 lines". Use the settings menu (the gear in the title bar) or the command palette. The default is 2 lines. With 1 line, the preview follows the topic. A long payload ends with "…". A payload that is not valid UTF-8 shows as `bin` and at most its first 16 bytes in hex, for example `bin ff 00 10`.

### Tree

- The tree is sorted a–z. Each row shows the topic name, "N topics" when topics with messages are below it (not on a leaf), a preview of the latest payload and the number of messages of the topic and all its subtopics. The tree has no activity dot and no retained marker. The Messages list shows the retained mark (R).
- The tree is the first view. It shows the first topic level. All topics below are collapsed.
- Click a row to select it. A click on a topic without subtopics also opens its details.
- Double-click a topic that has subtopics to expand or collapse it. To see its details, press Enter or click **Details**.
- Use Up and Down to move. Use Left and Right to collapse and expand.

### Filters

The filter field is in the centre header (`Ctrl/Cmd+F`). Its hint says what it searches: "Filter by topic or payload" in Messages, "Filter by topic" in Tree.

| Filter | Messages view | Tree view |
|---|---|---|
| Text | Topic, or text in the first 1 KiB of the payload. The match ignores case. | Topic path. The match ignores case. Matches keep their parents. |
| Show only | Messages whose topic matches that subscription | Topics that match that subscription |
| All, Received, Sent | Yes | Not shown |

Each active Show only or direction filter is a tag under the header. Its **×** button clears it. **Clear filters** in the command palette removes all filters, including the text. `Esc` in the filter field clears the text.

### Pause and Follow

- **Follow** has no switch. It is on when the list opens, and the newest row stays in view. If you scroll away from the top or select a row, Follow turns off. The reading position then stays still, and a **Jump to newest (N)** button shows over the list. N is the number of messages that arrived since Follow turned off. Select the button to go to the top and turn Follow on. The selection stays.
- **Pause** freezes the view. The button then reads **Resume**. The app still receives messages inside the memory limit. The header shows how many are new, for example "Paused · 1 240 new". **Jump to newest** does not show while you pause. **Resume** returns to the Follow state from before the pause.
- If the memory limit removed messages while you paused, a marker row says "Older messages were removed (memory limit)".

### Details pane

The right pane has two tabs: **Value** and **History**.

- In a window that is 1 100 px wide or more, the pane opens when you select a message or a topic. Its close button closes it. It stays closed until the next selection. To open it again, use "Details: Open" in the command palette.
- In a narrower window, a click opens the details as a sheet. If you select with the keyboard, the focus stays in the list. Press **Enter** to open the sheet for the selected row. When you close the sheet, the focus returns to the list.
- The header shows the full topic (you can select the text) and a **⋯** menu. **Edit and publish** is the one action button.
- The **⋯** menu has: **Copy topic**, **Copy payload**, **Publish to this topic**, **Reply** (only when the message has a Response Topic), **Decoder**, **Wrap**, **Expand** and **Clear retained…**. Use **Expand** to widen the pane.
- **Value:** the full payload. Choose a **decoder** in the **⋯** menu: Auto, Text, JSON, Hex, Base64 or Number. Auto decides in this order:
  1. JSON, if the content type is `application/json` or ends with `+json`, or if the payload starts and ends like `{…}` or `[…]` (spaces around it do not count).
  2. Text, if the payload format flag says UTF-8, or if the payload is valid UTF-8.
  3. Hex, for all other payloads.

  Auto never picks Number. For Number, set the type (int8, uint8, int16, uint16, int32, uint32, int64, uint64, float32 or float64) and the byte order (LE or BE).
- One muted line under the payload shows QoS, retained, size and arrival time.
- If a decoder cannot show the payload, the pane shows the cause and **Use Hex**.
- JSON shows pretty-printed. Wrap is on. **Copy payload** copies the whole formatted text.
- **Long payloads:** the pane shows the first 256 KiB and a **Show all** button. It formats in the background. For very long text, it shows the first part and **Show more (+N KiB)**. Each press adds more.
- MQTT 5 properties show in a **Properties** section under the payload (MQTT 3.1.1 messages have none). It is collapsed. It has: content type, response topic, correlation data (hex), message expiry, payload format and user properties.
- **History:** the last 100 messages of the topic (each stored up to 64 KiB). Select a row to show it in the Value tab. The focus moves to the Value tab. The tab then shows "Earlier message from 12:03:58" and a **Show latest** button. The button returns to the selected message or to the latest value of the topic. You can also use Up and Down to move the cursor, and press Enter to show the row.
- If the memory limit removed the selected message, the pane says "Removed from the message list (memory limit)."

## Publish

The composer is closed by default. The bottom of the centre pane then shows one button, **New message** (`Ctrl/Cmd+Shift+N`).

1. Select **New message**.
2. Type the topic.
3. Type the payload.
4. Set QoS 0 or 1, and **Retain** if you need it.
5. Press **Publish** or `Ctrl/Cmd+Enter`.

The composer stays open after Publish. To close it, select **✕** at the end of the topic row, or press `Esc`. The focus moves to the message list. The draft stays. When you open the composer again, the draft is there. Closing does not put the draft in History.

The result shows as one line below the payload and, if it is not routine, in the list row. It is short: "Sending…", "Sent" (QoS 0), "Acknowledged" (QoS 1), or "Rejected: <ReasonName>", for example "Rejected: NotAuthorized". If the broker sent a reason string, the text is "Rejected: <ReasonName>: <reason string>". The app shows no hex code. A message can also end with "No acknowledgement after 10 s." or "Not sent: the connection closed." The app checks the message before it sends it. An error then shows under the composer, for example "Connect to a broker first." When you are not connected, Publish is off. The 10 s timeout also ends the session (see [Troubleshooting](#other-connection-problems)).

- **Text, Hex or JSON:** the switch is always visible. A Hex payload sends exact bytes. Write the bytes as hex digits. Spaces are ignored.
- **JSON** sends the same bytes as Text. The composer checks the payload: valid JSON gets syntax colours. Invalid JSON shows a warning under the payload, for example "The payload is not valid JSON: … Publish still sends it as is." You can still publish it. Text and JSON keep the same text when you switch between them.
- The **⋯** menu of the composer has: **Beautify JSON**, **Minify JSON**, **History…** and **Properties** (MQTT 5).
- **Beautify JSON** indents the JSON payload. **Minify JSON** removes all spaces and line breaks outside strings. Both keep the key order. They work up to 1 MiB, in Text and JSON mode.
- **Properties** (MQTT 5): a **Properties (N)** tag shows next to the topic when a property is present. Select it to open the section. The section shows only the properties that are present. Use **Add property** to add: **Payload format: UTF-8**, content type, response topic, correlation data (hex), message expiry or user property. **Send empty** keeps an empty property in the message. A property that you add stays visible while it is empty.
- **History** menu: **Drafts** (the last 5 drafts that you replaced) and **Sent** (the last 20 publishes of this session). Choose one to fill the composer. A draft leaves **Drafts** when you choose it. The app does not save History to disk. It clears History when you change the connection.
- The composer saves the old content as a draft before something replaces it. These actions replace it: a choice in History, Edit and publish, Reply, Publish to this topic, Beautify JSON, Minify JSON and the Text/Hex switch. The app saves the old content only if it is unsent, it is not empty (a topic alone is empty) and it is at most 1 MiB. A larger draft is dropped, and the app says "The old draft is larger than 1 MiB. It is not kept."
- Drafts and Sent share a budget of 1 MiB. The app removes the oldest entries first.
- The command palette entry **Publish** opens the composer first when it is closed. It does not send a draft that you cannot see.

**Publish to this topic**, **Edit and publish** and **Reply** open the composer and fill it. They never send. **Clear retained…** is different: after you confirm, it publishes an empty retained message at QoS 1.

| Action | Effect |
|---|---|
| Publish to this topic | In the details **⋯** menu. Sets the topic. Keeps the payload. |
| Edit and publish | The button in the details pane. Copies topic, exact payload bytes, QoS, retain and properties (MQTT 3.1.1: no properties). A payload that is not valid UTF-8 opens in Hex mode. Off when the full payload is not stored ("The full payload is not stored."). |
| Reply | In the details **⋯** menu. Shows only when the message has a Response Topic. Sets that topic and copies the correlation data (MQTT 3.1.1: no correlation data). The payload is empty. |
| Clear retained… | In the details **⋯** menu. Shows only while you are connected and the topic has a retained message. After you confirm, it sends an empty retained message at QoS 1. |

## Memory limit

The app keeps all messages inside one memory limit. The default is 128 MiB. The settings file accepts 32 MiB to 2 GiB. The palette and the status-bar memory menu offer 64 MiB, 128 MiB, 256 MiB, 512 MiB, 1 GiB and 2 GiB. A change applies at once and the app saves it.

When the store is full:

- The app removes the oldest data in batches, down to 90% of the limit. It removes the oldest history first, then the oldest list rows, then the oldest latest values.
- Rarely updated topics leave the tree when the topic count is near its cap.
- Saving a message never blocks. Above the limit plus 25%, the app drops new messages and counts them.
- The **Details** popover of the status bar shows the use against the limit, for example "34 MiB / 128 MiB", and the limit control. A limit of 1024 or 2048 MiB shows as "1 GiB" or "2 GiB". The status bar itself shows the memory only when it is above 80% of the limit.
- The Messages list keeps 10 000 rows at most. The status bar shows "N rows removed (memory limit)" when rows leave.

## Themes and appearance

- **Appearance:** System (default), Light or Dark. System follows the operating system.
- **Theme:** Default, Catppuccin, Solarized, Everforest, Flexoki, Mellifluous, macOS Classic, Tokyo Night. Each theme has a light and a dark variant. Tokyo Night uses the Default light variant.
- Use the settings menu (the gear in the title bar), or the command palette ("Appearance: …", "Theme: …"). Appearance, Theme, **Row height** and **Memory budget** are submenus: point to one to see its choices. The settings menu also has **Commands…** and **Reset the layout**. The last two lines of the menu show the renderer and the app version (for example "Version: 0.2.4"). The renderer line shows "GPU" or "software", and the drawing API (DirectX 11 on Windows, Metal on macOS, Vulkan or OpenGL on Linux). "software" means that the CPU draws the window, for example in a virtual machine or over remote desktop. Then drawing uses CPU time and the window can be slower. On Windows and Linux, "Copy diagnostics" names the graphics adapter. On macOS, the app cannot read the adapter name, so the report shows "Unavailable (Metal)".
- The app saves the choice and applies it before the first window.
- The layout follows the font size of the theme.

The theme files keep the licences of their projects. See [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md).

## Updates

- The app checks for a new version once at start. It has no timer. Use **Check for updates…** in the command palette to check at any time.
- To turn off the check at start, use "Turn off the update check at start" in the command palette.
- The check resolves the address of the latest published GitHub release once. Then it reads `latest.json` from that release. The file has a minisign signature, and the app verifies it. The app also checks the SHA-256 hash and the size of the download. It rejects an older or equal version.
- When an update is ready, the title bar shows "Update 0.2.0 ready". Select it, then **Restart to update** (or **Later**). The app never restarts by itself.
- The app keeps one previous copy. If the new version never opens a window, the next start restores the old one.
- **Windows (installer), AppImage, macOS app:** the app updates itself.
- **Version 0.1.0 on Windows:** the shortcut starts `mqtt-navigator-launcher.exe`. It still works: it starts `mqtt-navigator.exe`. A reinstall with the installer removes it.
- **A development build** (started from `target/`) never updates itself.
- **deb install, or a folder you cannot write to:** the app cannot update itself. The title bar shows "Update <version> available", for example "Update 0.2.0 available". Point at it to see why. Select it to open the release page. Install the new deb yourself.
- The update check is the only network traffic other than your broker. The app sends no telemetry.

### Upgrade from 0.1.x

- Version 0.2.0 reads the connections that 0.1.x saved. A connection without a Protocol uses MQTT 5, as before. A new connection uses MQTT 3.1.1.
- The first save by 0.2.0 writes settings schema 2.
- The macOS app needs macOS 15 or later.

### Go back to version 0.1.x

Version 0.1.x does not load a schema 2 settings file. It leaves the file unchanged, uses default settings and does not save. To use 0.1.x again, move or rename `settings.json` (see [Logs and diagnostics](#logs-and-diagnostics)), or delete it after you make a backup. Version 0.1.x then starts with default settings. You lose the connections and settings that 0.2.0 saved.

## Keyboard shortcuts

`Ctrl` is `Cmd` on macOS.

| Keys | Action |
|---|---|
| `Ctrl+K` | Command palette. Most actions are also here. Subscription actions (QoS, Options…, Retry, Remove) work on the subscription in Show only. Not in the palette: add a subscription, the publish QoS and Retain, Reconnect, and the connection sheet actions (Save, Export, Import, Delete). |
| `Ctrl+F` | Focus the filter |
| `Esc` | Close the top layer (menu, popover, dialog, sheet). In the filter: clear it. Else close the area that has the focus: the composer (the draft stays) or the details pane. The focus moves to the list. |
| `Ctrl+Shift+N` | New message (opens the composer) |
| `Ctrl+1` / `Ctrl+2` | Messages view / Tree view |
| `Ctrl+Enter` | Publish (in the composer) |
| `Ctrl+N` | New connection |
| `Ctrl+Q` | Quit |
| `Tab` / `Shift+Tab` | Next / previous control. In the composer, it moves focus and does not type a tab. |
| `Up` / `Down` | Select the previous / next row in the Messages list or the tree. Click a row first. The first key press selects the top row. |
| `Left` / `Right` | In the tree only: collapse / expand the selected topic |
| `Up`, `Down`, `Enter` (History tab) | Move the cursor, and show the row in the Value tab. Click the History list first. |

In a narrow window, `Enter` in the Messages list or the tree opens the details sheet for the selected row. In a wide window, it does nothing. Pause, Edit and publish, Reply, Publish to this topic and the details pane have no shortcut. Use the buttons or the command palette. Pane widths change with the mouse. "Reset the layout" is in the command palette.

## Command line options

```
mqtt-navigator [--host HOST] [--port PORT] [--tcp | --ws | --wss] [--insecure]
               [--ca FILE] [--user NAME] [--password-stdin] [--mqtt 3|5]
               [--sub FILTER[:QOS]]...
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
| `--mqtt 3`, `--mqtt 5` | MQTT version. `--mqtt 3` is MQTT 3.1.1 and is the default. |
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

The settings file is `settings.json` in the `mqtt-navigator` folder of your system configuration folder (Linux: `~/.config`, macOS: `~/Library/Application Support`, Windows: `%APPDATA%`). The app keeps a damaged file as `.bad-<time>` and starts with defaults. A file from a newer version is not loaded and stays untouched. Version 0.2.0 writes settings schema 2. Version 0.1.x cannot read a schema 2 file: it does not change the file, uses default settings and does not save. A newer version shows this text when it meets a file it cannot read: "The settings file is from a newer version of MQTT Navigator (schema N). It was not changed. Default settings are in use, and changes are not saved." The app also does not save if it cannot read the file.

## Limits

- QoS 0 and QoS 1 only. The app does not support QoS 2. If a broker sends a QoS 2 message, the session ends with "The broker sent a QoS 2 message. This app supports QoS 0 and 1."
- MQTT 3.1.1 has no message properties, no reason codes, no subscription options and no session expiry. See [MQTT 3.1.1 connections](#mqtt-311-connections).
- The app finds a lost connection in about 2 times the keep alive, plus timer and shutdown delays (keep alive 60 s: at most 145 s).
- The largest incoming packet is 1 MiB. The app closes the connection if a packet is larger. One exception: on MQTT 3.1.1 the app checks the Remaining Length, so a packet with a non-minimal length encoding can be up to 3 bytes larger.
- After a very high message rate (far above 10 000 messages per second), the app can keep more memory than at a normal rate until it exits. In a Linux test it stayed the same over repeated overloads.
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
| The broker closed the connection during set-up. If the broker needs a client certificate, add one to the connection. | The app retries. If it keeps failing, set the client certificate and key. Some brokers (and Windows) hide the reason when they need a client certificate. Also check the address and the port. |
| The client certificate and key do not match. | Choose the key that belongs to the certificate. |
| The client key is encrypted. Enter the key passphrase, or use a key without a passphrase. | Type the passphrase in **Key passphrase**. |
| The client key passphrase is wrong, or the key is not valid. | Correct the passphrase, or choose another key. |
| The client certificate file has no certificate. / The CA file … has no certificate. | Choose a PEM file that has a certificate. |

The app does not retry after a certificate error. Fix the cause and connect again.

### Other connection problems

- **The broker refused the connection. Check the host and the port.** The broker is off, or the host or port is wrong. The app retries with growing waits (base 1 s to 30 s, real wait 0.8 s to 36 s).
- **The broker rejected the connection: …** The broker gave a reason. For a temporary reason (the server is not available, the server is busy, a quota is exceeded, the connection rate is exceeded), the app retries with growing waits. For any other reason, for example a wrong user name or password, a client that is not authorized or a ban, the app does not retry.
- **The broker closed the connection.** The connection dropped (for example a broker restart or a network cut). The app reconnects with growing waits. Select **Cancel** to stop.
- **The broker closed the connection: …** The broker sent a DISCONNECT with a reason. For a temporary reason, the app reconnects. For any other reason, the session ends as Failed and the app does not retry. Connect again after you fix the cause.
- **The broker sent a packet that is larger than the allowed size.** The broker sent a packet above 1 MiB. The app does not retry.
- **The broker sent data that is not valid MQTT 5.** The broker broke the MQTT 5 protocol. The app does not retry. (On MQTT 3.1.1 the text says "MQTT 3.1.1".)
- **This broker does not support MQTT 5. Set Protocol to MQTT 3.1.1.** Open the connection, set **Protocol** to MQTT 3.1.1, and connect again.
- **The broker sent a reply that is not MQTT 5. If the broker supports only MQTT 3.1.1, set Protocol to MQTT 3.1.1.** Do the same.
- **This broker does not support MQTT 3.1.1. Set Protocol to MQTT 5.** Set **Protocol** to MQTT 5.
- **The broker closed the connection before it accepted it. Check the address, the TLS settings and the Protocol.** The broker closed the connection before it sent a CONNACK.
- **The broker did not answer the connection request.** The broker sent no CONNACK in 10 s. The app retries with growing waits.
- **No answer from the broker after 10 s.** A subscribe or an unsubscribe got no answer. The row shows this text (for an unsubscribe: "Cannot unsubscribe from <filter>: No answer from the broker after 10 s."). Then the session fails with "The broker did not answer for 10 s. Connect again." Connect again.
- **No acknowledgement after 10 s.** A publish got no acknowledgement. The app ends the session in the same way: "The broker did not answer for 10 s. Connect again."
- **The connection closed (the broker did not answer, or the network failed).** The broker did not answer the keep-alive ping, or the network failed. The app reconnects with growing waits.
- **The broker did not read our data for 10 s.** The broker stopped reading while the app sent data. The app reconnects.
- **The broker stopped in the middle of a packet.** A packet started and did not finish. The app reconnects.

### The broker does not allow `#`

Some brokers reject the `#` filter. The row shows a reason, for example "Not authorized to subscribe." Remove `#`. Subscribe to a narrower filter.

### The "dropped" counter is above 0

The app received more messages than it could keep. The reasons are a very high message rate, or a full queue or store. The counter is red when it is not 0.

- Subscribe to a narrower filter.
- Raise the [memory limit](#memory-limit).
- Do not pause during a long flood.

### The window does not open

The app draws with the GPU (Vulkan on Linux). On a virtual machine or a remote desktop, the GPU can be missing. Install a software renderer (for example Mesa lavapipe on Linux). Check the log folder for an error. The app was checked on Linux with the Mesa lavapipe driver. Windows (WARP), virtual machines and remote desktops are not checked.

### More help

Open an issue on GitHub. Add the output of **Copy diagnostics**. Do not add payloads or passwords. For a security problem, read [SECURITY.md](../SECURITY.md).
