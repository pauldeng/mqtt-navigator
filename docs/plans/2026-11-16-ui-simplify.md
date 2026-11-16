# Plan: a simpler UI

Status: proposal for owner review (2026-11-16). Nothing is built yet.

Owner request: "I want a simple and intuitive UI and UX design. The current design is still very busy. Try to simplify it." Review [mqtt-viewer](https://github.com/mqtt-viewer/mqtt-viewer) and take what fits.

Rules that do not change: the two major features are **Subscribe** and **Publish**; gpui-kit components only; theme tokens only; everything works from the keyboard; one connection at a time.

## 1. What we learn from mqtt-viewer

mqtt-viewer is GPL-3.0-or-later (Go, Wails, Svelte). We take ideas only. We copy no code, text, icons or images. Add it to the clean-room list in AGENTS.md.

| Take | Why it fits |
|---|---|
| The topic tree shows the last value inline on a leaf row, and the counts are small and muted. | One look answers "what does this topic say now". Muted counts do not compete with names. |
| One filter field above the list: "Filter by topic or payload". | One place to search. No extra filter chips in normal use. |
| The details pane starts with the topic, and one short line gives the arrival time. | The user sees what and when without a metadata table. |
| Small, quiet chrome: an icon toolbar with tooltips, a one-item status bar ("12 messages/s"). | The data gets the space. |
| Publish starts from one "New message" action, and the publish history is a short list. | Publish is one clear entry point, not a permanent form. |

| Do not take | Why |
|---|---|
| Many brokers in tabs at the same time | Our scope is one connection. Tabs add a second navigation level. |
| Charts, the topic graph, the `$SYS` dashboard, client logs, Sparkplug B, record to disk, a web UI | Feature sprawl. Not Subscribe or Publish. |
| Saved message collections (global and per connection) | Useful, but a second content type with its own management. The History menu covers the need for now. |
| A timeline chart in the details pane | Heavy for the value it adds. Our History list is enough. |
| Icon-only buttons for primary actions | Primary actions keep a text label. Icons only for secondary actions, always with a tooltip. |
| A notification bell and a feedback button | Noise. |

## 2. What makes the current design busy

From [docs/ui-design.md](../ui-design.md) section 3, a normal connected window shows about 30 controls at once:

- **Title bar (6):** profile select, theme menu, state, filter, command palette, Connect.
- **Subscriptions pane (5 + 4 per row):** input, QoS toggle group, and on each row a colour chip, QoS tag, Options button and Remove button.
- **Centre header (6):** Messages | Tree, All | Received | Sent, Pause, Follow, row height, filter chips.
- **Message row (7 fields):** time, colour, topic, QoS, retained, size, payload.
- **Composer (9, always open):** topic, payload, Text | Hex, QoS, Retain, Properties, History, Format, Send.
- **Details (8):** Value | History tabs, Publish to this topic, Edit and publish, Reply, Clear retained, Expand, Wrap, decoder.
- **Status bar (7):** URL, Connection details, msg/s, topics, memory, drops, security tag.

Each control is reasonable alone. Together they compete with the data.

## 3. Principles for the change

1. **Data first.** The message list or the tree gets the most space and the strongest text. Chrome is quiet.
2. **Show a control when it can act.** Hide what does not apply now (for example the Options of a subscription on MQTT 3.1.1, already done). Show warnings only when they are true.
3. **One place for each thing.** No duplicate entry points on the screen. The command palette is the second path, not a second button.
4. **Progressive disclosure.** Rare actions go in a row menu (`⋯`) or the command palette. Default values need no control.
5. **Keep every feature.** Nothing is removed from the app. Things move to a menu, a popover or the palette.

## 4. The proposed layout

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ ● prod-broker ▾   Connected                              ⚙   [Disconnect]   │ title bar
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
│ mqtts://broker:8883 · 1 240 msg/s                                   Details │ status bar
└───────────────────────────────────────────────────────────────────────────────┘
```

In a wide window, the details pane opens on the right when you select a message or a topic (section 4.5).

### 4.1 Title bar

- Keep: the profile select (with "Manage connections…" as its last item), the connection state as a dot and text, and the one primary action (Connect, Cancel, Disconnect).
- Add a settings menu (gear icon, tooltip "Settings"). It holds: Commands… (`Ctrl/Cmd+K`), Appearance, Theme, Row height, Memory budget, and Reset the layout.
- Move: the theme menu goes into the settings menu. The filter moves to the centre header, next to the list it filters. The command palette button goes into the settings menu as "Commands…".
- Result: 3 controls and the settings icon (was 6).

### 4.2 Subscriptions pane

- The add row: the filter input (placeholder `topic/#`) and a small QoS select, `Q0 ▾`, as the input suffix. QoS 0 is the default. Enter adds the filter with the shown QoS. You choose the QoS before the SUBSCRIBE, so no message arrives at the wrong QoS.
- Each row: a status dot (the colour of the subscription; a red ✗ when rejected), the filter text, a `Q1` tag only for QoS 1, and an always-visible ghost `⋯` button.
  - The `⋯` button is a `DropdownMenu` (`Button::dropdown_menu` with a `PopupMenu`). It is in the Tab order (`Button::tab_stop`, `track_focus`) and has the tooltip "Subscription actions". It holds: Show only (checked item), QoS 0 / QoS 1 (checked items), Options… (MQTT 5 only), Retry (when rejected), and Remove. The Show only item is the keyboard path to Show only.
  - A click on the `⋯` button does not toggle Show only.
- A status line shows under the row only for a problem or a change: "Subscribing…", the rejection reason, or "Granted QoS 0" when the broker grants less than you asked for.
- A QoS change sends a new SUBSCRIBE with the same request tracking and send-failure handling as an options change (`set_sub_options` in `ui.rs`).
- Click a row: Show only (unchanged).
- Result: 2 visible controls on a normal row (was 4).

### 4.3 Centre header and rows

- One header row: `Messages | Tree`, the filter input, a direction menu, and Pause.
- **Filter text stays literal.** The placeholder states the real scope: "Filter by topic or payload" in Messages (topic, or text in the first 1 KiB of the payload), and "Filter by topic" in Tree.
- **Direction menu** (Messages only): a small `All ▾` button after the filter input, with the checked items All, Received and Sent. It uses the existing `DirectionFilter`. A choice other than All shows as a filter chip with a clear button.
- Filter chips show only when a filter is active (Show only, direction). The text filter shows in its own input.
- Row height (1 or 2 lines) moves to the settings menu and the command palette.
- **Follow** keeps its state, but it has no toggle:
  - Follow is on when the list opens.
  - Scrolling away from the top, or selecting a row, turns Follow off. Then a "Jump to newest (N)" button shows over the top of the list. N counts the messages that arrived since Follow went off. A new counter is needed: `paused_new()` counts only during Pause.
  - "Jump to newest" scrolls to the top, keeps the selection, and turns Follow on. It uses `UniformListScrollHandle::scroll_to_item_strict`.
  - Pause is separate and unchanged: while paused, the view does not change and the header shows "Paused · N new". "Jump to newest" does not show while paused. Resume returns to the Follow state from before Pause.
  - All of this reacts to events. There is no idle timer.
- **Message row, 2 lines:**
  - Line 1: topic (normal text colour), `R` only if retained, then the time, right-aligned and muted.
  - Line 2: the payload preview, monospace, muted.
  - A sent row has the ↑ marker. It also shows its result when the result is not routine: "Sending…", "Rejected: <reason>", "Timed out" or "Lost on disconnect". "Sent" and "Acknowledged" show in the details and in the composer result line, not on the row.
  - QoS and size move to the details. The subscription colour stays as a 2 px left edge, and only when there is more than one subscription.
- **Tree row:** name, the last value inline on a leaf, muted counts on a branch. No change in function.

### 4.4 Publish

- The composer has a new visibility state: **closed** (new) or open. The existing collapsed state goes away. Closed keeps the editor entities and the draft. Opening restores them. Closing does not move the draft into History.
- When closed, the bottom of the centre pane shows one text button: **New message** (`Ctrl/Cmd+Shift+N`). This is the only New message button. Edit and publish, Reply, Publish to this topic, and a History recall open the composer too.
- The palette entry "Send" opens the composer first when it is closed. It does not send a draft that the user cannot see.
- **The open composer:**
  - Topic and payload.
  - One row: payload mode `Text | Hex` (always visible, because it changes the bytes that are sent), QoS `0 | 1`, Retain, a `⋯` menu, and **Send**.
  - The `⋯` menu holds: Format, History…, Properties (MQTT 5), and Close. Menu choices use `PopupMenuItem::checked` where they are a state.
  - One shared close operation serves the Close item, `Esc` and the palette. It moves the focus to the message list, so the focus never stays on a removed control. The details pane close button uses the same rule.
  - When properties are present, a "Properties (3)" tag shows next to the topic. The count includes present-but-empty properties ("Send empty"). A click opens them.
  - The Properties section shows only the fields that are present, and an "Add property ▾" menu for the others: payload format (UTF-8), content type, response topic, correlation data, message expiry, user property. Whether a field shows is a separate state from its text, so a field that you add stays visible while it is empty. "Send empty" stays as it is. A History recall or a Reply opens Properties when it fills a field, as `apply_draft` does now.
- The composer stays open after Send. The last result shows as one line under the payload ("Acknowledged", "Rejected: …").
- Result: no publish form until you need it. One button when closed (was 9 controls always).

### 4.5 Details pane

- **Wide window (1 100 px or more):** the pane opens when you select a message or a topic, with the mouse or the keyboard. Its close button closes it. It stays closed until the next selection, or until you open it from the command palette.
- **Narrow window:** unchanged. A click opens the details sheet. Keyboard selection keeps the focus in the list, and `Enter` opens the sheet for the selected row. Closing the sheet returns the focus to the list.
- Header: the full topic as selectable text, and a `⋯` menu. No breadcrumb.
- Tabs stay: **Value** and **History**.
- History: choosing a row shows that message in the Value tab and moves the focus to the Value tab. The Value tab then shows "Earlier message from 12:03:58" with a **Show latest** button, which returns to the selected message or the latest value of the topic. The Value tab shows the payload (pretty JSON, wrap on) and one muted line under it: QoS · retained · size · arrival time. MQTT 5 properties show in a "Properties" section under that line, collapsed by default.
- **Edit and publish** is the one visible action button. The `⋯` menu holds: Copy topic, Copy payload, Publish to this topic, Reply (only with a Response Topic), Decoder, Wrap, Expand, and Clear retained… (last, after a separator, with confirmation).

### 4.6 Status bar

- Always: the broker URL, msg/s, and the **Details** button. The Details button exists also when not connected.
- Only when it applies:
  - "Not encrypted" or "Insecure TLS" (warning tag);
  - drops above 0 (red, with text);
  - memory above 80% of the budget;
  - "Reconnect to apply your changes" with its **Reconnect** button.
- The **Details** popover (`Popover` with `Anchor::BottomRight`) shows: Connection details (only while connected), topics, memory used / budget with the budget control, and drops.
- Eviction feedback: while paused, the marker rows stay in the list ("Older messages were removed (memory limit)"). When not paused, the status bar shows "N rows removed (memory limit)" while that count is above 0, as today (`list_drops`). This count stays separate from the dropped incoming messages.
- Result: 3 items normally (was 7).

### 4.7 Connection sheet

- Basic: name, host, port, transport, Protocol, username and password.
- TLS (TLS and WSS): verify the server certificate, CA file, and a collapsed "Client certificate" section (certificate, key, key passphrase).
- Advanced (collapsed): client ID (generated; edit here), WebSocket path, clean start / clean session, session expiry (MQTT 5), keep alive, Will message.
- A validation error in a collapsed section opens that section first, then moves the focus to the field.

### 4.8 Empty states

Build the two missing states (design section 7):

- Connected, no subscriptions: "Subscribe to a topic filter to see messages." The action focuses the add input.
- The broker rejected `#`: "The broker does not allow #. Subscribe to a narrower filter." The action focuses the add input.

## 5. Keyboard

- `Esc` order:
  1. An open layer (menu, popover, dialog, sheet, palette) closes.
  2. The focused control keeps `Esc` when it has a use: the filter clears its text; the payload editor keeps its keys.
  3. Else, `Esc` closes the area that has the focus: the composer (the draft stays) or the details pane. The focus moves to the message list.
- Use key bindings scoped to each area (`KeyBinding` with a context), not one global close handler.
- New shortcut: `Ctrl/Cmd+Shift+N` opens the composer (it is free in the app and the bundled input keymaps).

## 6. Command palette

Existing entries stay. New entries:

| Entry | Target |
|---|---|
| New message | Opens the composer. |
| Composer: Properties, History | Opens the composer and the part. |
| Subscription: QoS 0, QoS 1, Options…, Retry, Remove | The subscription in Show only. Disabled with the reason "Select a subscription first." when no subscription is in Show only. |
| Settings: Appearance, Theme, Row height, Memory budget, Reset the layout | As in the settings menu. |
| Details: Open | Opens the details pane for the selection. |

## 7. What stays the same

Data model, store, network, the narrow-window rule, the connection sheet sections (with the moves in 4.7), themes and presets, Drafts and History, and all texts that do not move.

## 8. Risks and checks

| Risk | Check |
|---|---|
| A moved action is harder to find | Every moved action has a palette entry (section 6) and a menu with a tooltip. The user guide lists where each action is. |
| `⋯` menus are invisible to keyboard users | The `⋯` buttons are always visible and in the Tab order. |
| Follow without a toggle surprises the user | "Jump to newest (N)" shows when Follow is off. |
| A closed composer hides a problem | Send from the palette opens the composer first. The result line shows in the open composer. Failed sends also show on their rows. |
| Hidden properties change a publish | The "Properties (N)" tag shows when any property is set. |

## 9. Steps (after owner approval)

1. Update `docs/ui-design.md` (sections 3, 3.1, 4, 5, 7, 8, 9, 11) and the user guide from this plan. Add mqtt-viewer to the clean-room list in AGENTS.md. Codex review.
2. Title bar, settings menu and status bar (4.1, 4.6).
3. Subscriptions pane (4.2).
4. Centre header, rows, direction menu and Follow (4.3).
5. Publish composer (4.4).
6. Details pane (4.5).
7. Connection sheet and empty states (4.7, 4.8).
8. Each step: gpui-kit APIs from the pinned source, unit tests for the logic (Follow and its counter, the direction filter, visibility rules, `Esc` order, palette targets), fmt, clippy, tests, Codex review, and a check in a real window by the owner (we have no screenshot tool).

## 10. Questions for the owner

1. Composer closed by default (4.4): yes, or keep it open as today?
2. Direction filter as a small menu next to the filter (4.3): yes, or keep the All / Received / Sent switch?
3. Details pane opens on selection in a wide window (4.5): yes?
4. Saved message collections (not taken now): add later as a separate feature?
