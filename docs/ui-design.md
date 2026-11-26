# UI and UX design

This page is the design reference for the user interface. Read [ARCHITECTURE.md](../ARCHITECTURE.md) first.

## 1. Principles

- The app has two major features: **Subscribe** and **Publish**. The layout shows these two features first.
- Keep the UI simple and professional, in a dense developer-tool style.
- Use **gpui-kit components only**. Do not use a webview or web UI. Do not depend on `gpui-webview` or `gpui-shell`.
- Use only theme tokens from `cx.theme()`. Do not put raw colour values in components.
- Each area has one primary action: Connect or Disconnect in the title bar, Publish in the composer, Save and connect in the connection sheet. The subscribe input has no button: Enter adds the filter.
- A choice shows its selected state (a toggle group, a toggle, a selected list row). Do not swap primary and ghost buttons to show a choice.
- Follow the [gpui-kit design guides](https://gpui-kit.com/docs/design-guides.md) for sizes, copy and states.
- A shortcut has one source: the key binding. Bind `secondary-<key>` (Cmd on macOS, Ctrl on Windows and Linux). Do not bind both. A menu item takes its action (`PopupMenuItem::action`), a button tooltip uses `tooltip_with_action`, and the palette and hints use `shortcut_text`. Do not type a shortcut into a label.
- All actions work from the keyboard. The one exception is pane resizing, which uses the mouse ("Reset the layout" is in the command palette).

Sources:
- the UI/UX rule packs [jakubkrehel/skills](https://github.com/jakubkrehel/skills) (MIT) and [ui-ux-pro-max](https://github.com/nextlevelbuilder/ui-ux-pro-max-skill) (MIT), without their mobile-only rules;
- the [gpui-kit documentation](https://gpui-kit.com/docs).

## 2. What we learn from other clients

| Client | Take | Avoid |
|---|---|---|
| MQTT Explorer | Connect and the tree fills at once (default `#`). Counts and an activity marker on each topic. Per-topic history. Publish starts from the selected topic. | Subscriptions are hidden in advanced settings. A tree only, with no stream of messages, so it is hard to see "what just happened". |
| MQTTX | Add and remove subscriptions live. A colour for each subscription. Click a subscription to filter the messages. A publish composer that is always visible below the messages. All / Received / Sent filter. Payload recall with arrow keys. | The chat-bubble view: tall rows, low density, poor for many topics. The topic tree is in a separate "Beta" area. Feature sprawl (scripts, agent, viewer). |
| mqtt-spy | "Show latest on new" (follow newest). Fast keyboard stepping through messages. A bounded message store. | Tabs inside tabs, many panes, separate windows for core tasks. |

None of the three has a direct "edit and publish" or "reply" action for a received message. We add both.

## 3. Layout

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ ● prod-broker ▾   Connected                              ⚙   [Disconnect]   │ TitleBar
├──────────────────┬───────────────────────────────────────────────────────────┤
│ Subscriptions    │ [Messages│Tree]  🔍 Filter by topic or payload  [All ▾] ⏸ │
│ [topic/#   ][Q0▾]│ factory/line1/temp                            12:04:01.120 │
│ ● #           ⋯  │ {"v":21.4,"u":"°C","ts":1788503522623}                     │
│ ● factory/# Q1 ⋯ │ home/garage/door                       R      12:04:01.087 │
│ ✗ secret/#    ⋯  │ "closed"                                                   │
│   Not authorized │ ↑ factory/cmd                    Rejected: NotAuthorized   │
│                  │ {"cmd":"stop"}                                             │
│                  ├────────────────────────────────────────────────────────────┤
│                  │ [New message]                                              │
├──────────────────┴────────────────────────────────────────────────────────────┤
│ mqtts://broker:8883 · 1 240 msg/s                                   Details │ StatusBar
└───────────────────────────────────────────────────────────────────────────────┘
```

In a wide window (1 100 px or more), the details pane opens on the right when you select a message or a topic.

The window has one place for each job. A control shows when it can act. Rare actions are in a `⋯` menu and in the command palette.

- **Title bar:**
  - The connection menu: a button with the state dot, the name and the host and port. It truncates the address first, then the name. Below 900 px it shows the dot and the name only. A click opens a menu with the saved connections (checked = active; a "Command line" connection shows first), then "Edit "name"…", "New connection… (Ctrl/Cmd+N)" and "Manage connections…" (the sheet, section 3.1). For a "Command line" connection, the menu shows "Command line: not saved" in place of Edit. With no active connection, the button says "Choose a connection". With no connection at all, it says "Add a connection" and opens the sheet.
  - The connection state, as a dot **and** text: "Connecting…", "Connected", "Reconnecting (3)…", "Disconnected: The broker rejected the connection: this client is not authorized.".
  - The one primary action. It is "Connect" when disconnected, "Cancel" while connecting, and "Disconnect" when connected. Its tooltip names the target. During Disconnecting it says "Disconnecting…" and is off. With no active connection, it does not show.
  - A settings menu (gear icon, tooltip "Settings"). It holds: Commands… (`Ctrl/Cmd+K`), the submenus Appearance, Theme, Row height and Memory budget, Reset the layout, a renderer line (GPU or software, and the API) and a version line ("Version: 0.2.7"). These two lines are not actions. The checked items are in the submenus. The top level has no checked item and does not scroll, so it fits a small window. (A scrollable gpui-kit menu scrolls to its first checked item one frame after it opens. In a small window that looked like a flicker.)
  - The filter is in the centre header. The command palette has no button of its own.
  - If you choose a different profile while disconnected or failed, the app only selects it. You then select **Connect**. If you choose a different profile while connected, connecting or reconnecting, the app asks "Disconnect from prod and connect to test? Messages from prod are removed.". It does not change the active broker silently.
  - **A new connection that you save** becomes active when the state is Disconnected or Failed, no keyring read is pending and no Connect is outstanding. Else it is only saved.
  - **Data belongs to one profile.** Switching to another profile clears the received messages, the selection, the composer and History. A disconnect or a reconnect with the **same** profile keeps them, and the list shows a marker row "Reconnected 12:05:10".
- **Left pane, Subscribe (major feature 1):**
  - One input: type a topic filter and press Enter. A small QoS select (`Q0 ▾`) is the suffix of the input. QoS 0 is the default. The filter is added with the shown QoS.
  - An invalid filter (for example `a/#/b` or an empty filter) shows an error under the input. The app does not send it.
  - A row has a status dot (the subscription colour; a red ✗ when the broker rejected it), the filter text, a `Q1` tag only for QoS 1, and an always-visible ghost `⋯` button. A row has no message count.
  - A status line shows under the row only for a problem or a change: "Subscribing…", the rejection reason (for example "Not authorized to subscribe."), or "Granted QoS 0" when the broker grants less than you asked for.
  - The `⋯` button is a `DropdownMenu` (tooltip "Subscription actions"). It is in the Tab order. It holds: Show only (checked item), QoS 0 and QoS 1 (checked items), Options… (MQTT 5 only), Retry (when rejected) and Remove. A click on the button does not toggle Show only.
  - A QoS change sends a new SUBSCRIBE. It uses the same request tracking and send-failure handling as an options change.
  - The rows are `ListItem`s on a virtual list. Each subscription gets a colour from its row position. The colours repeat after 10 rows. A message takes the colour of the **first** subscription in the list that matches its topic.
  - **Options** (MQTT 5 only; hidden for MQTT 3.1.1): the item opens a `Popover` with two `Switch`es (No Local, Retain As Published) and a `ToggleGroup` for Retain Handling (Always, If new, Never). A change sends a new SUBSCRIBE and the row shows "Subscribing…" again. The app refuses No Local on a shared filter (`$share/...`) with a text under the input. The options are saved with the subscription. The add input has no options: add the row, then set its options.
  - **Show only**: click a subscription, or choose Show only in its menu, to show only its messages. Do it again to show all. This changes the view only. It does not unsubscribe. Remove unsubscribes.
  - A new profile starts with the subscription `#`, so messages appear at once.
    - When you add a narrower filter while `#` exists, a hint asks "Remove # to receive only your filters?" with a **Remove #** button.
    - If the broker rejects `#`, the empty state says "The broker does not allow #. Subscribe to a narrower filter."
- **Centre pane, received and sent messages:**
  - One header row: the switch "Messages | Tree", the filter input, the direction menu (Messages only) and **Pause**.
  - **Messages is the default**, because a newest-first stream answers "what is happening now". The app remembers the last choice.
  - **Messages**:
    - Each message has two lines. Line 1: the topic, `R` only if retained, and the time (right-aligned, muted). Line 2: the payload preview in the monospace font (muted).
    - QoS and size show in the details pane. The subscription colour is a 2 px left edge, and only when there is more than one subscription.
    - The order is always chronological, newest first. There is no sorting.
    - A sent message has the arrow-up marker. It shows its result only when the result is not routine: "Sending…", "Rejected: <reason>", "Timed out" or "Connection closed". "Sent" and "Acknowledged" show in the details and in the composer result line.
    - The **direction menu** is a small `All ▾` button after the filter input. It has the checked items All, Received and Sent. A choice other than All also shows as a filter chip with a clear button.
  - **Tree**: the live topic tree, sorted a–z. A leaf shows its last value inline. A branch shows muted counts. It has no activity marker.
  - **Selection** keeps a message ID, not only a topic:
    - The selected message stays visible in the details even if the store evicts it, until you select another one. Then the details show "Removed from the message list (memory limit)."
    - When you change views, the tree selects that message's topic. The list selects that message if its row still exists. If not, the list selects nothing.
  - **Follow and Pause:**
    - Follow has no toggle. It is on when the list opens. Scrolling away from the top, or selecting a row, turns it off. Then a "Jump to newest (N)" button shows over the top of the list. N counts the messages that arrived after Follow went off.
    - "Jump to newest" scrolls to the top, keeps the selection and turns Follow on again.
    - Pause freezes the view. Ingest continues inside the memory budget. The header shows "Paused · 1 240 new". "Jump to newest" does not show while paused. Resume returns to the Follow state from before the pause.
    - If the budget removed messages while paused, a marker row says "Older messages were removed (memory limit)".
  - **Filters** (section 3.2) show as `Tag`s only when active (Show only, direction), each with a clear button. The command palette and the empty state have **Clear filters**. The text filter shows in its own input.
- **Bottom of the centre pane, Publish (major feature 2):**
  - The composer is **closed** by default. Then the bottom of the centre pane shows one text button, **New message** (`Ctrl/Cmd+Shift+N`). This is the only New message button.
  - Closing keeps the editors and the draft. Opening restores them. Closing does not move the draft into History.
  - Edit and publish, Reply, Publish to this topic and a History recall open the composer too. The palette entry "Publish" opens a closed composer first. It does not send a draft that you cannot see.
  - The open composer has the topic, the message, and one row: message mode `Text | Hex | JSON` (always visible, because it changes the bytes that are sent), QoS `0 | 1`, Retain, a `⋯` menu and **Publish** (`Ctrl/Cmd+Enter`). JSON sends the text as is. It checks the text in the background: valid JSON gets tree-sitter highlighting ([ADR 0007](adr/0007-json-highlighting.md)), invalid JSON a warning line in the warning colour. The warning does not block Publish.
  - The `⋯` menu holds: Beautify JSON, Minify JSON, Convert to Base64, Convert to Base64URL, History… and Properties (MQTT 5). A visible ✕ button at the end of the topic row closes the composer (also `Esc`). A menu item that is a state uses a checked item.
  - **One close operation** serves the Close item, `Esc` and the palette. It moves the focus to the message list, so the focus never stays on a removed control.
  - The composer stays open after Publish. The last result shows under the message ("Acknowledged", "Rejected: …"). Show long results on a maximum of three lines.
  - A "Properties (N)" tag shows next to the topic when any property is present. The count includes present-but-empty properties ("Send empty"). A click on the tag opens the section.
  - **Properties** (MQTT 5) shows only the fields that are present, and an "Add property ▾" menu for the others: payload format (UTF-8), content type, response topic, correlation data, message expiry and user property. A field that you add stays visible while it is empty, because "present" is a state separate from the text. A History recall or a Reply opens Properties when it fills a field.
  - The maximum height of the composer is 55% of the window height. If that is less than 17.5 rem, the maximum is 17.5 rem, so that the fixed rows keep their room at a large zoom. At the end, the composer takes at most 80% of the window. The Properties fields scroll as one block inside it. The topic, the message and the action row (mode, QoS, Retain, Publish) always stay in view, and the message list keeps the rest of the height. A centre pane narrower than 560 design pixels puts the label of a property above its field.
  - Message mode is **Text**, **Hex** or **JSON**. A Hex message is sent as raw bytes. JSON sends the text as is, with a validity check and highlighting. Beautify and Minify work in Text and JSON mode.
  - The **History** menu has two groups: **Drafts** (the last 5 replaced drafts) and **Sent** (the last 20 publishes of this session). Choose one to fill the composer. Nothing is saved to disk.
  - **Every** replacement of the composer content first saves the current draft to Drafts, if it has content and was not sent. This includes History recall, Edit and publish, Reply, and Publish to this topic.
  - The topic and the property texts are single-line inputs. A text with a line break shows escaped (`\n`) with the note "This topic has line breaks; they are kept.". The composer keeps the exact text until you edit that field.
  - Content type, correlation data and each user property have **Send empty**: a property that is present but empty stays present in Edit and publish and in the sent message.
  - **Beautify JSON** and **Minify JSON** work up to 1 MiB of message and 2 MiB of result, and it saves the old draft first. The Text/Hex switch saves the old draft, too.
  - **Convert to Base64** and **Convert to Base64URL** (in the `⋯` menu and the command palette) encode the bytes that Publish would send now. They put the text in the message box, switch to Text mode and move the focus to the message box. They are off when the message has no bytes or the Hex text is not valid (the Hex error shows). Base64 has padding. Base64URL has none. A Hex message must be valid hex. The old draft is saved first.
  - While disconnected, Publish is disabled and the composer shows "Connect to a broker first." when it has no result to show. The result of the last publish stays on screen.
- **Right pane, details of the selected message or topic:**
  - **Wide window (1 100 px or more):** the pane opens when you select a message or a topic, with the mouse or the keyboard. Its close button closes it. It stays closed until the next selection, or until you open it from the command palette ("Details: Open").
  - **Narrow window:** a click opens the details as a right-side sheet. Keyboard selection keeps the focus in the list, and `Enter` opens the sheet for the selected row. Closing the sheet returns the focus to the list.
  - Header: the full topic as selectable text, and a `⋯` menu. There is no breadcrumb.
  - The header shows a muted "Topic" caption before the full topic.
  - Tabs: **Message** and **History**.
  - The Message tab shows the payload (pretty JSON, wrap on) and one muted line under it: QoS · retained · size · arrival time. MQTT 5 properties show in a "Properties" section under that line. It is collapsed by default.
  - **History:** choosing a row shows that message in the Message tab and moves the focus to the Message tab. The Message tab then shows "Earlier message from 12:03:58" and a **Show latest** button. The button returns to the selected message or to the latest value of the topic.
  - **Edit and publish** is the one visible action button. It fills the composer. **No action sends.** The old composer content goes to Drafts, so you can get it back. After the action, focus moves to the composer message. In a narrow window, the details sheet closes first.
    - **Edit and publish** fills topic, payload bytes, QoS, retain and properties from the message.
      - A payload that is not valid UTF-8 opens in Hex mode with the exact bytes.
      - The action is disabled when the full payload is not stored (truncated or evicted). The reason shows next to it: "The full payload is not stored."
      - The composer never takes a formatted view (pretty JSON, Hex, Base64) as the payload text.
  - The `⋯` menu holds: Copy topic, Copy payload, Publish to this topic, Reply (only with a Response Topic), Decoder, Wrap, Expand and Clear retained… (last, after a separator, with confirmation).
    - **Publish to this topic** sets the topic only and keeps the payload. It is available for a topic and for a message. It does not need a stored payload: a branch of the tree and a topic with an evicted value work too.
    - **Reply** sets the topic to the response topic and copies the correlation data. The payload is empty, Retain is off, and the other properties are empty.
  - **Clear retained** shows only for a topic that has a retained message. The store tracks the broker state for each topic: a retained message with a payload sets it, a retained message with an empty payload or our own acknowledged Clear ends it, and the Message Expiry Interval of the retained message ends it. A message without the Retain flag changes nothing.
- **Status bar:**
  - Always: the broker URL, msg/s and the **Details** button. The Details button also shows when you are not connected.
  - Only when it applies: a `Tag::warning()` "Not encrypted" (TCP and WS) or "Insecure TLS" (TLS verification off); drops above 0 (red, with text); memory above 80% of the budget; "Reconnect to apply your changes" with its **Reconnect** button.
  - The **Details** `Popover` shows: Connection details (only while connected, see the user guide), topics, memory used against the budget with the budget control, and drops.
  - While the view is paused, marker rows show the removed rows. When it is not paused, the status bar shows "N rows removed (memory limit)" while that count is above 0. This count is separate from the dropped incoming messages.
- **Panes:** the three panes (subscriptions, centre, details) are resizable. Each side pane has a least and a most width in rem, and the centre keeps at least 22.5 rem. The app saves the two side widths (`ui.pane_widths`) and clamps them when it loads them. "Expand" widens the details pane. The expanded width is not saved. "Reset the layout" restores the default widths (16.25 rem and 22.5 rem).

### 3.1 Connection sheet

- **Basic:** name, host, port, transport (TLS, WSS, TCP, WS), **Protocol**, and username / password.
- **TLS is on by default.** A new profile uses transport TLS on port 8883. The default ports are TLS 8883, WSS 443, TCP 1883 and WS 80. When you change the transport, the port changes too, but only if it is still the default of the old transport.
- To turn TLS off, select TCP or WS. The sheet then shows a warning: "Not encrypted. Passwords and messages are sent in clear text."
- **TLS** (TLS and WSS only): verify the server certificate (on), CA file, and a collapsed "Client certificate" section (certificate, key, key passphrase).
- **Advanced** (collapsed): client ID (generated; edit it here), WebSocket path, clean start (MQTT 3.1.1: clean session), session expiry (MQTT 5), keepalive, Will message (will delay: MQTT 5).
- The saved connections are `ListItem` rows. The row of the draft is selected. "New connection" is the last row.
- The sheet title names the action: "New connection" or "Edit lab". It follows the draft.
- Buttons: **Save and connect** (primary), **Save**, **Cancel**. A button that opens a dialog ends with "…" (for example "Delete connection…"). A dialog has **Cancel** and a verb ("Discard changes", "Switch connection"). Validation runs when you press a button. A validation error in a collapsed section opens that section first, then moves the focus to the field.
- There is no separate "Test connection" button. "Save and connect" is the test, a connection failure shows in the title bar, and a validation error shows in the sheet.

### 3.2 Filter scope

| Filter | Messages view | Tree view |
|---|---|---|
| Text filter (`Ctrl/Cmd+F`) | Topic, or text in the first 1 KiB of the payload. The placeholder says so. | Topic path only. Matches keep their parents visible. |
| Show only (a subscription) | Messages that match the subscription | Topics that match the subscription |
| All / Received / Sent | Yes | Not shown |

### 3.3 Long and short payloads

Payloads are often long (400 characters or more, often JSON). Some are a single number.

- **Rows have a fixed height.** The virtual list renders only visible rows, and it needs one row height. Rows do not expand in place. The details pane shows the full payload. Heights are in rem and the view reads them from `window.rem_size()` in each frame, so they follow the zoom: 30 px (1 line) and 44 px (2 lines) at the default 16 px rem.
- **Messages view, two lines (default):**
  - Line 1: topic, `R` only if retained, and the time (right-aligned, muted).
  - Line 2: the payload preview, in the monospace font, across the **full** row width. A short payload shows completely. A long payload shows its start and ends with "…".
- **Row height option:** "1 line" or "2 lines". It is in the settings menu and in the command palette. The app remembers it. With 1 line, the preview follows the topic.
- **Preview text:**
  - Line breaks and indentation collapse to one space, so JSON reads as `{"v":21.4,"u":"°C","ts":…}`.
  - Control characters show as `·`. A payload that is not valid UTF-8 shows as `bin 7b 22 76 …`.
  - Truncation never cuts a character in half (grapheme-safe).
  - The app builds the preview only for visible rows, from the first 512 bytes. There is no work for each arriving message.
- **Tree view:** one line for each row. A leaf shows a short preview of the last value. A branch shows muted counts.
- **Details pane, Message tab:**
  - The full payload. JSON is pretty-printed. Lines wrap by default, and the **Wrap** toggle turns wrapping off. Long topics and tokens wrap and do not overflow.
  - **Expand** widens the details pane over the message list. **Restore** returns it to its size.
  - A payload larger than 256 KiB shows its first 256 KiB and **Show all (1.2 MiB)**. Formatting runs in the background (see ARCHITECTURE.md, "Responsive UI").
  - The editor takes a bounded chunk of the formatted text in one step, because setting and laying out text runs on the UI thread. Longer text shows its first chunk and **Show more (+N KiB)**. Each click adds the next chunk. **Copy** copies all of the formatted text, but only when the finished render belongs to the selection. While the render runs, Copy is off.
  - The details `⋯` menu and the command palette have these actions: Decoder (also Number), Number type, Number byte order, Wrap, Show all, Show more. Each one shows only when it can work.
- **Tooltip:** hovering a row shows the first 20 lines of the payload. This is only a shortcut. Selecting the row gives the same content in the details pane.

## 4. Component map

This table lists what the app uses now.

| UI part | gpui-kit component |
|---|---|
| Window and overlays | `gpui_kit::open_window` with `TitleBar::window_options()`. `Root` hosts dialogs, sheets, toasts, menus. |
| Title bar | `TitleBar` with `Button` + `dropdown_menu` and `dropdown_caret` (connection menu; rows are `PopupMenuItem::element` with `checked`), `Button` (settings menu, connect) |
| Settings menu | `Button::dropdown_menu` with a `PopupMenu`: Commands…, submenus (`PopupMenu::submenu`) Appearance, Theme, Row height and Memory budget with checked items, Reset the layout, the renderer and version labels |
| Three panes | `h_resizable` with `resizable_panel().size(saved).size_range(..)`. The side panels are `.fixed()`. We save the widths in `on_resize`. |
| Subscriptions list | `ListItem` rows (selected state = "Show only") in a `uniform_list`; a colour dot (`div` with a theme `chart_1`…`chart_5` colour); `Tag` (QoS 1); ghost `Button` with `dropdown_menu` and a `PopupMenu` (row actions); `Input` with `InputEvent::PressEnter` and a QoS `Select` as suffix; `Popover` with `Switch` and `ToggleGroup` for the options (MQTT 5) |
| Messages / Tree switch | `TabBar::segmented()` |
| Direction menu | `Button::dropdown_menu` with checked `PopupMenuItem`s |
| Jump to newest (N) | `Button` over the top of the list |
| Filter chips, status warnings | `Tag`, `Tag::warning()` |
| Topic tree | Our own flat rows on `uniform_list` (chevron icons from `IconName`). gpui-kit `Tree` rebuilds all entries on each change, which is too slow for 100k topics. |
| Message list | Our own rows on `uniform_list`: one fixed row height for 1-line or 2-line rows, so the scroll anchor stays exact. |
| Detail tabs (Message, History) | `TabBar::underline()`. We switch the content ourselves. History rows are `ListItem`s. The header has a `⋯` `dropdown_menu`. |
| Publish composer | `v_flex` at the bottom of the centre pane. closed: one text `Button` (New message). Open: `Input` (topic), `Editor` (message), `TabBar::segmented()` (Text / Hex), `ToggleGroup` (QoS), `Switch` (Retain), `Button` with `dropdown_menu` (the `⋯` menu), `Tag` (Properties (N)), `Button::primary()` (Send). |
| Narrow window details | `open_sheet_at(Placement::Right, ..)` below 1 100 px |
| Decoder choice | `Select` |
| Payload view and Publish editor | `Editor`. Without the `tree-sitter` feature, the editor shows plain text. |
| MQTT 5 properties (view) | `DescriptionList` in a `Collapsible` |
| MQTT 5 properties (edit) | Only the present fields, and an "Add property" `dropdown_menu`. User properties are rows of two `Input`s and a remove `Button`. gpui-kit has no key-value editor. |
| Retain, verify TLS | `Switch` |
| Connection editor | `open_sheet_at(Placement::Right, ..)` with `v_form` and `field`; `ListItem` rows for the saved connections |
| Certificate files | GPUI `cx.prompt_for_paths(..)` (native file dialog). The path is read off the UI thread. |
| Confirm (clear retained, delete profile, switch, discard) | `open_alert_dialog` and `open_dialog` |
| Messages to the user | `push_notification`. Errors stay until the user closes them. |
| Command palette | `Command` in `open_dialog` |
| Empty states | `Empty` with `EmptyHeader`, `EmptyTitle`, `EmptyDescription` and `EmptyContent` |
| Status bar | `StatusBar` with `.left()` and `.right()`; `Popover` with a ghost `Button` trigger (Details) |
| Icons | Lucide icons from `gpui_kit::assets::Assets`. Use one icon set only. Use icons, not text glyphs (chevrons, arrows, check marks). |

Not used yet: `Spinner`, `Button::loading`, `Popover` with `TextView` (update notes), `Kbd`.

## 5. Theme

- Light and dark come from gpui-kit theme files. The app bundles seven theme families (JSON files in `assets/themes/`, notices in [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md)) and the gpui-kit default.
- A **preset** pairs one light and one dark theme:

  | Preset | Light | Dark |
  |---|---|---|
  | Default (default) | Default Light | Default Dark |
  | Catppuccin | Catppuccin Latte | Catppuccin Mocha |
  | Solarized | Solarized Light | Solarized Dark |
  | Everforest | Everforest Light | Everforest Dark |
  | Flexoki | Flexoki Light | Flexoki Dark |
  | Mellifluous | Mellifluous Light | Mellifluous Dark |
  | macOS Classic | macOS Classic Light | macOS Classic Dark |
  | Tokyo Night | Default Light | Tokyo Night |

- Setting "Appearance": **System** (default), Light, Dark. The settings menu of the title bar has two submenus, "Appearance" and "Theme". The command palette has "Appearance: …" and "Theme: …" entries. The app saves `ui.theme_mode` and `ui.theme_preset`. An unknown preset id means "default".
- The app applies the saved preset before the first window opens, so the first frame has the right colours. After each change it sets the font size, the font families and the caret blink again, because a theme change resets them.
- System mode follows the OS. Use `cx.observe_window_appearance` and `Theme::sync_system_appearance`. This is an event; there is no polling.
- Status colours use the semantic tokens `success`, `warning`, `danger`, `info`. Each status also has an icon or text.
- Test contrast in both themes on the real surface. Text needs 4.5:1. Icons, borders and focus rings need 3:1.
- Do not reuse a light-theme value in the dark theme.

## 6. Typography and size

- UI text: the system UI font (gpui-kit default). Payloads, topics, timestamps, counts and sizes: the monospace theme font. No fonts are bundled.
- Right-align numbers. Use the monospace font for numbers, so the columns do not move.
- Use the gpui type scale: `text_sm` for body text and `text_xs` for secondary text. Do not set font sizes in pixels. Use weight 400 and 500 only.
- Layout uses rem, not pixels. The theme `font_size` is the base rem and the app zoom. The helper `ui(px) -> Rems` (`rems(px / 16.0)`) turns a design pixel at the default 16 px rem into rems. Pixels are only for hairlines (1 px borders) and the minimum window size.
- List row heights come from `window.rem_size()` in each frame, so the virtual lists follow the zoom.
- Payloads and topics are selectable and can be copied.
- Copy: sentence case for labels and section titles. A button or menu item that opens a dialog or sheet ends with "…". A confirm dialog has "Cancel" and a verb. Keep tooltips for icon-only buttons, and for shortcuts and disabled reasons. Rows use the default arrow cursor.

## 7. States

Every view designs these states: empty, loading, error with a recovery action, disconnected, disabled with a reason, success.

| Where | Empty-state text | Action |
|---|---|---|
| No connection exists | "Add a broker to start" | "New connection…" (primary), "Connect to localhost:1883 (not encrypted)" |
| Connections exist, none active | "Choose a connection" and "Your connections are in this menu." | "Choose a connection ▾" (a menu button with the rows of the title bar menu), "New connection…" |
| Connected, no subscriptions | "Subscribe to a topic filter to see messages." | Focus the subscribe input |
| Connecting, reconnecting | "Connecting to mqtts://broker:8883…" | "Cancel" |
| Broker rejected `#` | "The broker does not allow #. Subscribe to a narrower filter." | Focus the subscribe input |
| Subscribed, no messages yet | "No messages yet" and "Messages show here when the broker sends them." | — |
| Active, disconnected | The connection card: the name, then "mqtt://host:port · MQTT 3.1.1 · Not encrypted" | "Connect" (primary), "Edit connection…" |
| Failed | The card, then the reason | "Try again" (primary), "Edit connection…" |
| Disconnecting | "Disconnecting from lab…" | — |
| Failed, messages on screen | A banner above the list: "lab: reason" | "Try again", "Edit connection…", close (×). A new connect shows it again. |
| Filters have no match | "No messages match the filters" (Tree: "No topics match the filters") | "Clear filters" |
| No selection | "Select a topic with a message." | — |

- An error states the cause and the next step. Example: "The server certificate expired on 2026-03-01. Renew it, or turn off verification."
- A publish result is short: "Sent" (QoS 0), "Acknowledged" (QoS 1), "Rejected: NotAuthorized" (the reason name of the broker, and its reason string if it sent one).

## 8. Keyboard

Rules, in this order:

1. An open layer (menu, popover, dialog, sheet, palette) gets `Esc` first, and `Esc` closes it.
2. The focused control keeps `Esc` when it has a use: the filter clears its text, and the payload editor keeps its keys.
3. Else `Esc` closes the area that has the focus: the composer (the draft stays) or the details pane. The focus moves to the message list.
4. App shortcuts always use a modifier key, so they do not take text input.

Key bindings are scoped to each area (`KeyBinding` with a context). There is no global close handler.

Other rules:

- `Tab` always moves focus, also in the message editor. To indent JSON, use **Format** in the composer.
- `Ctrl/Cmd+F` always focuses the app filter. The payload editor has no separate find.

| Keys | Action |
|---|---|
| `Ctrl/Cmd+K` | Command palette (most actions are also here) |
| `Ctrl/Cmd+F` | Focus the filter |
| `Esc` (in the filter) | Clear the filter |
| `Esc` (elsewhere) | Close the top layer (menu, popover, dialog, sheet), else the composer or the details pane |
| `Ctrl/Cmd+Shift+N` | New message (opens the composer) |
| `Ctrl/Cmd+1` / `Ctrl/Cmd+2` | Messages view / Tree view |
| `Ctrl/Cmd+Enter` (in the composer) | Publish |
| `Ctrl/Cmd+N` | New connection |
| `Ctrl/Cmd+,` | Settings (planned; not built in v1) |
| `F6` / `Shift+F6` | Next / previous pane (planned; not built in v1) |
| `Tab` / `Shift+Tab` | Next / previous control |
| `Up`, `Down` | Move the selection in the tree and the list |
| `Left`, `Right` (tree) | Collapse, expand the selected topic |
| `Enter` (in the list, narrow window) | Open the details sheet for the selected row |
| `Up`, `Down`, `Enter` (in History) | Move the cursor in the rows of the History tab, show the row in the Message tab |

- Pause, Edit and publish, Reply, Publish to this topic and the details pane have buttons, menu items and command palette entries. They have no shortcut, so that they cannot conflict with system or editor keys.
- Pane widths change with the mouse. The command palette has "Reset the layout".
- Tooltips and menus show the shortcuts.

## 9. Motion and live data

- Hover, selection and tab changes are instant.
- Planned, not built in v1: an activity marker on a topic row (a dot that fades out over 1 s, without a fade when the OS asks for reduced motion). Today the changing count is the only signal.
- Follow is on by default in the Messages view and has no toggle. Selecting a row or scrolling away from the top turns it off, and "Jump to newest (N)" shows. N is a new counter of the messages that arrived while Follow was off. All of this reacts to events. Pause stops view updates (WCAG 2.2.2).

## 10. Accessibility

Out of scope (owner decision, 2026-11-10). The app does not target screen readers, and we do no accessibility testing. Keyboard use (section 7) and readable contrast (section 4) stay, because they are part of a good professional UI.

## 11. Review checklist (each UI change)

1. All controls work from the keyboard, and the Tab order matches the visual order.
2. Focus is visible in both themes.
3. Every icon button has a tooltip. Every `⋯` button is visible and in the Tab order.
4. Contrast passes in light and dark.
5. Components use only theme tokens.
6. No status shows by colour alone.
7. Each area has one primary action, and no entry point shows twice. A control shows only when it can act. Each moved action has a menu item and a palette entry. Destructive actions ask for confirmation and are kept apart from routine actions.
8. The empty, loading, error, disabled and success states exist.
9. A truncated topic or payload has a keyboard path to the full value.
10. Numbers use the monospace font, and no text is smaller than 12 px.
11. Lists are virtualized. Live data can be paused.
12. Nothing clips at the minimum window size (960 × 600) or at 150% zoom. Below 1 100 px the details pane is a sheet.

## Mockups

[`mockups/`](../mockups/) has static HTML pictures of the layout. They are design references only. They show the layout from before [the simplification](plans/2026-11-16-ui-simplify.md). Section 3 is the current layout. The app does not use HTML.
