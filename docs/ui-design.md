# UI and UX design

This page is the design reference for the user interface. Read [ARCHITECTURE.md](../ARCHITECTURE.md) first.

## 1. Principles

- The app has two major features: **Subscribe** and **Publish**. The layout shows these two features first.
- Keep the UI simple and professional, in a dense developer-tool style.
- Use **gpui-kit components only**. Do not use a webview or web UI. Do not depend on `gpui-webview` or `gpui-shell`.
- Use only theme tokens from `cx.theme()`. Do not put raw colour values in components.
- Each area has one primary action: Connect or Disconnect in the title bar, Subscribe in the left pane, Send in the composer.
- All actions work from the keyboard. The one exception is pane resizing, which uses the mouse ("Reset layout" is in the command palette).

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
┌──────────────────────────────────────────────────────────────────────────────────┐
│ [prod-broker ▾] ● Connected      🔍 Filter topics and payloads   Ctrl+K  [Disconnect] │ TitleBar
├───────────────────┬─────────────────────────────────────┬────────────────────────┤
│ SUBSCRIPTIONS     │ [Messages│Tree]  [All│Received│Sent]  │ factory/line1/temp   R │
│ ■ #           Q0  │                   ⏸ Pause  ✓ Follow │ [Value] [History]      │
│ ■ factory/#   Q1  │ 12:04:01.120 ■ factory/line1/temp  │ { "v": 21.4,           │
│ ■ secret/#    ✗   │              QoS1 R  {"v":21.4,…}   │   "u": "°C" }          │
│   Not authorized  │ 12:04:01.087 ■ home/garage/door    │ QoS 1 · Retained · 20 B│
│                   │              QoS0    "closed"       │ content-type  app/json │
│                   │ 12:04:00.990 ↑ factory/cmd   Sent  │ site          nz       │
│                   ├─────────────────────────────────────┤ [Edit and publish]     │
│ [topic/filter    ]│ PUBLISH  [factory/line1/cmd     ]  │ [Reply]                │
│ [Q0│Q1] [Subscribe]│ [{"cmd":"stop"}       ] Q[0│1] ☐R │ ───────                │
│                   │ ▸ Properties          [Send ⌃⏎]    │ [Clear retained]       │
├───────────────────┴─────────────────────────────────────┴────────────────────────┤
│ mqtts://broker:8883 · 1 240 msg/s · 105 topics · 34 / 128 MiB · 0 dropped         │ StatusBar
└──────────────────────────────────────────────────────────────────────────────────┘
```

The window has one place for each job:

- **Title bar:**
  - The profile `Select`. Its last item, "Manage connections…", opens the connection sheet (section 3.1).
  - The connection state, as a dot **and** text: "Connecting…", "Connected", "Reconnecting (3)…", "Disconnected: not authorized".
  - The filter, and the command palette.
  - The one primary action. It is "Connect" when disconnected, "Cancel" while connecting, and "Disconnect" when connected.
  - If you select a different profile while connected, the app asks "Disconnect from prod and connect to test? Messages from prod are removed.". It does not change the active broker silently.
  - **Data belongs to one profile.** Switching to another profile clears the received messages, the selection, the composer and History ▾. A disconnect or a reconnect with the **same** profile keeps them, and the list shows a marker row "Reconnected 12:05:10".
- **Left pane, Subscribe (major feature 1):**
  - One input: type a topic filter and press Enter. QoS 0 is the default, and a small toggle sets QoS 1.
  - An invalid filter (for example `a/#/b` or an empty filter) shows an error under the input. The app does not send it.
  - A new row shows "Subscribing…" until the SUBACK arrives. Then it shows the granted QoS and a message count.
  - A rejected row shows the reason in text (for example "Not authorized (0x87)"), with **Retry** and **Remove**.
  - Each subscription gets a colour automatically. A message takes the colour of the **first** subscription in the list that matches its topic. Click the colour chip to change the colour.
  - **Show only**: click a subscription to show only its messages, and click again to show all. This changes the view only. It does not unsubscribe. Remove (×) unsubscribes.
  - A new profile starts with the subscription `#`, so messages appear at once.
    - When you add a narrower filter while `#` exists, a hint asks "Remove # to receive only your filters?" with a **Remove #** button.
    - If the broker rejects `#`, the empty state says "The broker does not allow #. Subscribe to a narrower filter."
- **Centre pane, received and sent messages:**
  - The switch "Messages | Tree" selects the view. **Messages is the default**, because a newest-first stream answers "what is happening now". The app remembers the last choice.
  - **Messages**:
    - One dense row for each message: time, subscription colour, topic, QoS, retained, payload preview.
    - The order is always chronological, newest first. There is no sorting.
    - Sent messages show in the same list with an "↑ Sent" mark.
    - The "All | Received | Sent" filter shows in this view only.
  - **Tree**: the live topic tree, sorted a–z, with message counts, subtopic counts and an activity marker.
  - **Selection** keeps a message ID, not only a topic:
    - The selected message stays visible in the details even if the store evicts it, until you select another one. Then the details show "Removed from the message list (memory limit)."
    - When you change views, the tree selects that message's topic. The list selects that message if its row still exists. If not, the list selects nothing.
  - **Pause and Follow:**
    - Pause freezes the view. Ingest continues inside the memory budget. The header shows "Paused · 1 240 new".
    - If the budget removed messages while paused, a marker row says "Older messages were removed (memory limit)".
    - Follow keeps the newest row in view. Selecting a row or scrolling turns Follow off.
  - **Filters** (section 3.2) show as chips under the header, with one **Clear filters** action.
- **Bottom of the centre pane, Publish (major feature 2):**
  - The composer is always visible. It has topic, payload, QoS 0/1, Retain and **Send** (`Ctrl/Cmd+Enter`). It shows the target broker next to Send, for example "to prod".
  - Payload mode is **Text** or **Hex**. A Hex payload is sent as raw bytes.
  - "Properties" opens the MQTT 5 fields: content type, response topic, correlation data, message expiry, user properties.
  - The **History ▾** button has two groups: **Drafts** (the last 5 replaced drafts) and **Sent** (the last 20 publishes of this session). Choose one to fill the composer. Nothing is saved to disk.
  - **Every** replacement of the composer content first saves the current draft to Drafts, if it has content and was not sent. This includes History recall, Edit and publish, Reply, and Publish to this topic.
  - The composer can collapse to one line. Then it shows the topic, the payload and Send.
  - While disconnected, Send is disabled and the composer shows "Connect to a broker first."
- **Right pane, details of the selected message or topic:**
  - Tabs: Value and History.
  - Actions fill the composer. **They never send.** The old composer content goes to Drafts, so you can get it back. After the action, focus moves to the composer payload. In a narrow window, the details sheet closes first.
    - **Publish to this topic** sets the topic only and keeps the payload. It is available for a topic and for a message.
    - **Edit and publish** fills topic, payload bytes, QoS, retain and properties from the message.
      - A payload that is not valid UTF-8 opens in Hex mode with the exact bytes.
      - The action is disabled when the full payload is not stored (truncated or evicted). The reason shows next to it: "The full payload is not stored."
      - The composer never takes a formatted view (pretty JSON, Hex dump, Base64) as the payload text.
    - **Reply** shows only when the message has an MQTT 5 Response Topic. It sets the topic to the response topic and copies the correlation data. The payload is empty, Retain is off, and the other properties are empty.
  - **Clear retained** shows only for a topic that has a retained message. It sits apart from the other actions and asks for confirmation.
  - The pane header has a close button. The pane stays closed until you open it again (button in the centre header, or the command palette).
  - When the window is narrower than 1 100 px, the details open as a right-side sheet.
- **Status bar:** URL, msg/s, topics, memory used against the budget, and drops (red with text when above 0). A warning tag shows "Not encrypted" for TCP and WS, and "Insecure TLS" when TLS verification is off.

### 3.1 Connection sheet

- **Basic:** name, host, port, transport (TLS, WSS, TCP, WS) and username / password. The client ID is generated, and you can edit it.
- **TLS is on by default.** A new profile uses transport TLS on port 8883. The default ports are TLS 8883, WSS 443, TCP 1883 and WS 80. When you change the transport, the port changes too, but only if it is still the default of the old transport.
- To turn TLS off, select TCP or WS. The sheet then shows a warning: "Not encrypted. Passwords and messages are sent in clear text."
- **TLS** (TLS and WSS only): verify the server certificate (on), CA file, client certificate and key.
- **Advanced** (collapsed): WebSocket path, clean start, session expiry, keepalive, Will message.
- Buttons: **Save and connect** (primary), **Save**, **Cancel**. Validation runs when you press a button, and focus moves to the first wrong field.
- There is no separate "Test connection" button. "Save and connect" is the test, and a failure shows its cause in the sheet.

### 3.2 Filter scope

| Filter | Messages view | Tree view |
|---|---|---|
| Text filter (`Ctrl/Cmd+F`) | Topic, or text in the first 1 KiB of the payload. The placeholder says so. | Topic path only. Matches keep their parents visible. |
| Show only (a subscription) | Messages that match the subscription | Topics that match the subscription |
| All / Received / Sent | Yes | Not shown |

### 3.3 Long and short payloads

Payloads are often long (400 characters or more, often JSON). Some are a single number.

- **Rows have a fixed height.** The virtual list renders only visible rows, and it needs one row height. Rows do not expand in place. The details pane shows the full payload.
- **Messages view, two lines (default):**
  - Line 1: time, subscription colour, topic, QoS, retained, size (right-aligned).
  - Line 2: the payload preview, in the monospace font, across the **full** row width. A short payload shows completely. A long payload shows its start and ends with "…".
- **Row height option:** "1 line" or "2 lines". It is in the Messages header menu and in the command palette, and the app remembers it. With 1 line, the preview follows the topic.
- **Preview text:**
  - Line breaks and indentation collapse to one space, so JSON reads as `{"v":21.4,"u":"°C","ts":…}`.
  - Control characters show as `·`. A payload that is not valid UTF-8 shows as `bin 7b 22 76 …`.
  - Truncation never cuts a character in half (grapheme-safe).
  - The app builds the preview only for visible rows, from the first 512 bytes. There is no work for each arriving message.
- **Tree view:** one line for each row, with a short preview of the last value.
- **Details pane, Value tab:**
  - The full payload. JSON is pretty-printed. Lines wrap by default, and the **Wrap** toggle turns wrapping off. Long topics and tokens wrap and do not overflow.
  - **Expand** widens the details pane over the message list. **Restore** returns it to its size.
  - A payload larger than 256 KiB shows its first 256 KiB and **Show all (1.2 MiB)**. Formatting runs in the background (see ARCHITECTURE.md, "Responsive UI").
- **Tooltip:** hovering a row shows the first 20 lines of the payload. This is only a shortcut. Selecting the row gives the same content in the details pane.

## 4. Component map

| UI part | gpui-kit component |
|---|---|
| Window and overlays | `gpui_kit::open_window` with `TitleBar::window_options()`. `Root` hosts dialogs, sheets, toasts, menus. |
| Title bar | `TitleBar` with `Select` (profiles), `Input` with a search icon prefix, `Kbd`, `Button` |
| Three panes | `h_resizable` with `resizable_panel().size_range(..)`. We save `sizes()` in `on_resize`. |
| Subscriptions list | `List` / `ListItem`; a small colour chip (`div` with a theme `chart_1`…`chart_5` / palette colour); `Tag` (QoS); `Button::ghost().xsmall().icon(..)` (remove); `Input` with `InputEvent::PressEnter` and a `ToggleGroup` for QoS (add) |
| Messages / Tree switch, All / Received / Sent filter | `TabBar::segmented()` |
| Topic tree | `Tree` with `TreeState`, which renders through `uniform_list`. **Risk:** `set_items` and expand rebuild all entries. P1 measures 100k topics. The fallback is our own flat row list on `uniform_list`. |
| Message list | `DataTable` with a `TableDelegate` (virtualized, one fixed row height for 1-line or 2-line rows). The delegate reads our buffer newest-first. P1 checks that a 2-line row (metadata line + full-width preview line) works in `DataTable`. If it does not, use `uniform_list` with our own row layout. |
| Detail tabs (Value, History) | `TabBar::underline()`. We switch the content ourselves. |
| Publish composer | `v_flex` at the bottom of the centre pane, in a vertical `v_resizable` split with the message view. `Input` (topic), `Editor` (payload), `TabBar::segmented()` (Text / Hex), `ToggleGroup` (QoS), `Switch` (Retain), `Collapsible` (Properties), `DropdownButton` (History ▾), `Button::primary()` (Send). |
| Narrow window details | `open_sheet_at(Placement::Right, ..)` below 1 100 px |
| Decoder choice | `Select` |
| Payload view and Publish editor | `Editor` (`.language("json")`). P5 measures the `tree-sitter` feature (JSON only) before we enable it. Without it, the editor shows plain text. |
| MQTT 5 properties (view) | `DescriptionList` |
| User properties (edit) | Rows of two `Input`s and a remove `Button`, plus an "Add property" button. gpui-kit has no key-value editor. |
| QoS choice | `ToggleGroup` with two toggles, 0 and 1 |
| Retain, verify TLS | `Switch` |
| Connection editor | `open_sheet_at(Placement::Right, ..)` with `v_form` and `field` |
| Certificate files | GPUI `cx.prompt_for_paths(..)` (native file dialog). The path is read off the UI thread. |
| Confirm (clear retained, delete profile) | `open_alert_dialog` |
| Messages to the user | `push_notification`. Errors stay until the user closes them. |
| Update notes | `Popover` with `TextView::markdown` |
| Command palette | `Command` in `open_dialog` |
| Empty states | `Empty` |
| Loading | `Spinner`, `Button::loading(true)` |
| Status bar | `StatusBar` with `.left()` and `.right()` |
| Icons | Lucide icons from `gpui_kit::assets::Assets` (101 icons, 44 KiB). Use one icon set only. |

## 5. Theme

- Light and dark come from the gpui-kit built-in themes.
- Setting "Appearance": **System** (default), Light, Dark. You can change it in Settings and in the command palette. The app saves it.
- System mode follows the OS. Use `cx.observe_window_appearance` and `Theme::sync_system_appearance`. This is an event; there is no polling.
- Status colours use the semantic tokens `success`, `warning`, `danger`, `info`. Each status also has an icon or text.
- Test contrast in both themes on the real surface. Text needs 4.5:1. Icons, borders and focus rings need 3:1.
- Do not reuse a light-theme value in the dark theme.

## 6. Typography

- UI text: the system UI font (gpui-kit default). Payloads, topics, timestamps, counts and sizes: the monospace theme font. No fonts are bundled.
- Right-align numbers. Use the monospace font for numbers, so the columns do not move.
- Use a small scale: 13 px for body text and 12 px for secondary text. Use weight 400 and 500 only. The theme `font_size` setting is also the app zoom.
- Payloads and topics are selectable and can be copied.

## 7. States

Every view designs these states: empty, loading, error with a recovery action, disconnected, disabled with a reason, success.

| Where | Empty-state text | Action |
|---|---|---|
| No profiles | "Add a broker to start." | "New connection" (TLS), "Connect to localhost:1883 (not encrypted)" |
| Connected, no subscriptions | "Subscribe to a topic filter to see messages." | Focus the subscribe input |
| Connecting | "Connecting to broker:8883…" | "Cancel" |
| Broker rejected `#` | "The broker does not allow #. Subscribe to a narrower filter." | Focus the subscribe input |
| Subscribed, no messages yet | "No messages yet. Messages show here when the broker sends them." | — |
| Filters have no match | "No messages match the filters." (Tree: "No topics match the filters.") | "Clear filters" |
| No selection | "Select a topic with a message." | — |

- An error states the cause and the next step. Example: "Certificate expired on 2026-03-01. Renew the server certificate, or turn off verification."
- A publish result is short: "Sent" (QoS 0), "Acknowledged" (QoS 1), "Rejected: not authorized (0x87)".

## 8. Keyboard

Rules, in this order:

1. An open layer (menu, dialog, sheet, palette) gets `Esc` first, and `Esc` closes it.
2. The focused control gets its keys next. Text fields and the payload editor keep `/`, Space, arrows and text editing keys.
3. App shortcuts always use a modifier key, so they do not take text input.

Other rules:

- `Tab` always moves focus, also in the payload editor. To indent JSON, use **Format** in the composer.
- `Ctrl/Cmd+F` always focuses the app filter. The payload editor has no separate find.

| Keys | Action |
|---|---|
| `Ctrl/Cmd+K` | Command palette (every action is also here) |
| `Ctrl/Cmd+F` | Focus the filter |
| `Esc` (in the filter) | Clear the filter |
| `Esc` (elsewhere) | Close the top layer (menu, dialog, sheet) |
| `Ctrl/Cmd+1` / `Ctrl/Cmd+2` | Messages view / Tree view |
| `Ctrl/Cmd+Enter` (in the composer) | Send |
| `Ctrl/Cmd+N` | New connection |
| `Ctrl/Cmd+,` | Settings |
| `F6` / `Shift+F6` | Next / previous pane |
| `Tab` / `Shift+Tab` | Next / previous control |
| Arrows, `Left`, `Right`, `Enter` | Move, collapse, expand, select in the tree and the list |

- Pause, Edit and publish, Reply, Publish to this topic and the details pane have buttons and command palette entries. They have no shortcut, so that they cannot conflict with system or editor keys.
- Pane widths change with the mouse. The command palette has "Reset layout".
- Tooltips and menus show the shortcuts.

## 9. Motion and live data

- Hover, selection and tab changes are instant.
- The activity marker on a topic row is a dot that fades out over 1 s. It is not the only signal: the count changes too. If the OS asks for reduced motion, the dot shows without a fade.
- "Follow newest" in the Messages view is on by default. Selecting a row or scrolling turns it off. Pause stops view updates (WCAG 2.2.2).

## 10. Accessibility

Out of scope (owner decision, 2026-11-10). The app does not target screen readers, and we do no accessibility testing. Keyboard use (section 7) and readable contrast (section 4) stay, because they are part of a good professional UI.

## 11. Review checklist (each UI change)

1. All controls work from the keyboard, and the Tab order matches the visual order.
2. Focus is visible in both themes.
3. Every icon button has a tooltip.
4. Contrast passes in light and dark.
5. Components use only theme tokens.
6. No status shows by colour alone.
7. Each view has one primary action. Destructive actions ask for confirmation and are kept apart from routine actions.
8. The empty, loading, error, disabled and success states exist.
9. A truncated topic or payload has a keyboard path to the full value.
10. Numbers use the monospace font, and no text is smaller than 12 px.
11. Lists are virtualized. Live data can be paused.
12. Nothing clips at the minimum window size (960 × 600) or at 150% zoom. Below 1 100 px the details pane is a sheet.

## Mockups

[`mockups/`](../mockups/) has static HTML pictures of the layout. They are design references only. The app does not use HTML.
