# Connection UX: proposal (2026-11-18)

Status: approved by the owner on 2026-11-18, with the two recommendations of section 7 (select only; a "Choose a connection ▾" button in state B). Codex reviewed rounds 1 and 2. This text includes their corrections.

## 1. Problem

The owner reported that the main screen is confusing. It shows "Not connected" and a **Connect** button. The owner could not find answers to four questions:

1. Where does **Connect** connect to?
2. How do I change the connection settings?
3. How do I add a connection?
4. Where is the list of saved connections?

We reproduced this on v0.2.1 under Xvfb. We found these causes:

| # | What the user sees | Cause |
|---|---|---|
| P1 | The empty state says "Not connected. Connect to a broker to see messages." and a **Connect** button. | The text does not name the broker. The address shows only in small text in the status bar (bottom left). |
| P2 | Two **Connect** buttons (title bar and empty state). Neither names the target. | Both call `toggle_connection`. Neither has a tooltip or a target in its text. |
| P3 | The profile select says "No connections" when connections are saved but none is active. | `Select::placeholder("No connections")` shows whenever no item is selected (`ui.rs` `render_title`). |
| P4 | Edit and New are hard to find. | The visible path is the last item of the profile select, "Manage connections…". The sheet then shows **New connection** at the top. Other paths exist but are not visible: `Ctrl/Cmd+N`, the palette ("New connection…", "Manage connections…"), and the **New connection…** button of the first-start screen (only when nothing is saved). |
| P5 | The profile select looks like a form field, not like the place to switch brokers. | It is a plain `Select`, 180 px wide. |
| P6 | After **Save** of a new connection, the main screen still says "No connection selected". | `save_profile` (`ui/profiles.rs`) saves the profile. It makes it active only for **Save and connect**. |
| P7 | The title bar **Connect** button is disabled with no reason when no connection is active. | The button has no tooltip, and a disabled button is not in the Tab order. |
| P8 | The sheet title is "Connections" for a new connection and for an edit. | `connections.rs` uses one title. |

## 2. Goals

- The screen always shows the active broker, by name **and** address.
- All connection actions are in one visible connection menu in the title bar: switch, edit, add and manage. The empty state shows the next action for its state.
- The empty state answers "what next" for each case: nothing saved, saved but none active, active and disconnected, failed.
- Keep the simple UI of v0.2.1. Add no new pane. Use gpui-kit components only. Add no settings field.

## 3. Proposal

### 3.1 Title bar: the connection menu

Replace the profile `Select` with a connection menu: a `Button` with `.dropdown_menu(...)` and `.dropdown_caret(true)`. The button shows the state dot, the name and the host and port. The state text stays next to it.

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ [● lab-broker · localhost:18830 ▾]  Disconnected             ⚙  [Connect]   │
└──────────────────────────────────────────────────────────────────────────────┘
```

The menu (`PopupMenu`, `.scrollable(true)`, bounded height):

```
┌───────────────────────────────────────────┐
│ ✓ lab-broker        localhost:18830       │  saved connections, in saved order
│   prod              broker.example:8883   │  checked = active
│   Command line      10.0.0.5:1883         │  only when started with arguments
├───────────────────────────────────────────┤
│   Edit "lab-broker"…                      │  only for a saved active connection
│   New connection…               Ctrl+N    │
│   Manage connections…                     │  the sheet: list, export, import
└───────────────────────────────────────────┘
```

- Each row is `PopupMenuItem::element(...)` with `.checked(...)`: the name, and the host and port in muted text.
- The button text when no connection is active:
  - "Add a connection" when no connection exists: nothing is saved and there is no "Command line" connection. A click opens the sheet on a new connection.
  - "Choose a connection" when a saved or a "Command line" connection exists. A click opens the menu. (Example: you start with arguments, switch to the one saved connection, and delete it. The "Command line" connection is still in the menu.)
- A choice of a connection while **disconnected or failed** makes it active. It does not connect.
- A choice while **connected, connecting or reconnecting** keeps the current rule: the app asks "Disconnect from prod and connect to test? Messages from prod are removed." If you accept, it disconnects, clears the data of the old connection and connects to the new one (`request_switch`).
- **Command line connection.** The sheet edits only saved connections. For the "Command line" connection, the menu shows "Command line: not saved" in place of **Edit**. Editing it is not in this change.

**Long text and narrow windows:**
- The menu button has a maximum width of 320 px. It truncates the address first, then the name, with an ellipsis. The menu rows show the full text, and the button tooltip shows the full name and URL.
- The state text truncates before the menu button. The primary button (**Connect**, **Cancel**, **Disconnect**) never truncates or hides when a connection is active.
- IPv6 hosts show in brackets: `[fd00::1]:1883`.
- Below 900 px, the button shows only the dot and the name.

### 3.2 Title bar: the primary button

- The label stays short: **Connect**, **Cancel** or **Disconnect**.
- Its tooltip names the target: "Connect to lab-broker (mqtt://localhost:18830)".
- With no active connection, the primary button does not show. The menu button ("Add a connection" or "Choose a connection") is the action.
- During **Disconnecting**, the button shows "Disconnecting…" as its label and is disabled. The state text also says "Disconnecting…". So the reason is visible, not only in a tooltip.

### 3.3 Empty states of the centre pane

Each empty state names the broker and gives the next action. One primary button shows in each state.

**A. No connection exists** (no change): nothing is saved and there is no "Command line" connection.

```
              Add a broker to start
        Topics and messages show here.
             [New connection…]
   [Connect to localhost:1883 (not encrypted)]
```

**B. Connections exist, none active** (changed text). A saved or a "Command line" connection exists. This state is rare: at start the app selects the last or the first saved connection.

```
              Choose a connection
     Your connections are in this menu.
     [Choose a connection ▾]   New connection…
```

**Choose a connection ▾** is a second `DropdownMenu` button with the same rows as the title bar menu. One function builds the rows for both menus. (gpui-kit 0.9.1 has no public way to open the title bar menu from another button.) B has no second visible list.

**C. Active, disconnected** (changed): a connection card.

```
                 lab-broker
     mqtt://localhost:18830 · MQTT 3.1.1 · Not encrypted
           [Connect]     Edit connection…
```

- **Connect** is the primary button. **Edit connection…** opens the sheet on this connection. For the "Command line" connection, the card shows "Command line: not saved" in place of **Edit connection…**.

**D. Failed** (changed): the card of C, the reason, and the same two buttons in the same positions.

```
                 lab-broker
     mqtts://broker.example:8883 · MQTT 5
   The server certificate is not trusted. Add its CA file or turn off verification.
           [Try again]     Edit connection…
```

- The app does not sort the buttons by the error text. `ConnState::Failed` holds a string, not a category.
- **Reconnecting** is not a failure. It keeps state E with the attempt number and **Cancel**.

**E. Connecting or reconnecting** (no change): "Connecting to mqtt://localhost:18830…" and **Cancel**.

**F. Disconnecting** (changed): "Disconnecting from lab-broker…" with no button. Today this state shows an enabled **Connect** button that does nothing (`toggle_connection` ignores Disconnecting).

**Failure with messages on screen.** The empty states show only when the list is empty, and an active filter has its own empty state. So a failed connection can have messages on screen. For this case, a banner shows above the list:

```
 ⚠ lab-broker: The server certificate is not trusted. …   [Try again] [Edit connection…] [×]
```

- The banner shows for `Failed`. It hides on a new connect, on a switch, or on **×**.
- For the "Command line" connection, the banner shows "Command line: not saved" in place of **Edit connection…**, as in C and D.
- The title bar state text keeps the reason too, as today.

### 3.4 The connection sheet

- **Save** of a new connection makes it active only when the state is **Disconnected** or **Failed**, no keyring read is pending, and no Connect request is outstanding (`gate.outstanding(...)`: after `send_connect` the state can still read Disconnected or Failed). It uses the existing switch path (`switch_to` with no connect), so the data of the old connection does not stay under the new one. In every other state, **Save** only saves, as today.
- The sheet title names the action: "New connection", or "Edit lab-broker". It follows the draft when you pick another row in the sheet list.
- The list in the sheet stays. It is the place for delete, export and import.

### 3.5 Keyboard

- The menu button is in the Tab order. `Enter` or `Space` opens the menu. The arrow keys move. `Enter` chooses. `Esc` closes the menu and returns the focus to the button.
- After a choice, the focus moves to the primary button (**Connect**).
- After **Save** in the sheet, the focus moves to the primary button.
- If you cancel the "Disconnect from …?" question, the focus returns to the menu button.
- The card buttons in C and D are normal buttons in the Tab order.
- `Ctrl/Cmd+N` (new connection) stays.

### 3.6 Status bar

No change. The address stays at the bottom left.

## 4. Not in this change

- No sidebar list of connections.
- No "recent connections" order and no new settings field. The menu uses the saved order.
- No edit of the "Command line" connection.
- No "Duplicate" action.
- No connect-on-start setting.
- No change to the sheet fields.

## 5. Changes in code and docs

- `ui.rs` `render_title`: replace `Select` with `Button` + `DropdownMenu`. Add the tooltips, the truncation rules and the "Disconnecting…" label. Hide the primary button when no connection is active.
- `ui.rs` empty states: the card of C and D, the text of F, and the failure banner.
- `ui/profiles.rs`: `sync_select` builds the menu rows. `render_no_profile` gets the B text. `save_profile` makes a new profile active through `switch_to`, only when disconnected or failed and no keyring read is pending.
- `connections.rs`: the sheet title.
- Remove the `profile_select` entity if nothing else needs it.
- Docs: `docs/ui-design.md` sections 3 and 7, `docs/user-guide.md` "Connections" and "First start", and the UI verification list.

## 6. Verification

- Unit tests:
  - the menu button text for nothing saved, saved but none active, active, and a Command line connection;
  - **Save** of a new connection makes it active when disconnected or failed. It does not when connected, connecting, reconnecting or disconnecting, when a keyring read is pending, or when a Connect request is outstanding. The messages of the old connection do not stay.
- Screenshots under Xvfb of A to F and the banner, at 1600 × 1000 and 960 × 600, with a long name, an IPv6 host and a long failure text.
- Keyboard checks under Xvfb with xdotool: Tab to the menu button, open, arrows, choose, `Esc`, and the focus after a choice, after **Save** and after a cancelled question.
- 150 % zoom is a check for the owner on a real display.

## 7. Questions for the owner

1. **A choice while disconnected: select only, or select and connect?** Recommendation: select only. You see the name and the address, then you select **Connect**. While connected, the current question "Disconnect from … and connect to …?" stays in both cases.
2. **State B: a "Choose a connection ▾" menu button, or a second list of saved connections in the centre?** Recommendation: the menu button. It has the same rows as the title bar menu, and B is rare.

## 8. Verification (2026-11-18)

Screenshots: `/tmp/claude-1000/-home-pdeng-Workspace-mqtt-navigator/48e74d3d-e99e-4cde-8366-7382a812f445/scratchpad/conn/`. Xvfb :77, a temporary config folder, no real keyring, Mosquitto on 18830. The window was 960 × 600, and one shot is 1600 × 1000.

- A, no connection: `A.png`. B, none active: `B.png`, `B-menu.png` (menu open). C, disconnected: `C.png`, `L-1600.png` (1600 × 1000). D, failed, with a long name and a long address: `D-long.png`. E, connecting: `E.png`. Banner, failed with messages: `banner.png`, `banner-closed.png`.
- Title bar menu open: `menu.png`. IPv6 host `[fd00::1]:1883`: `k-open.png`. "Command line" connection and its menu: `N-menu.png`. Sheet titles: `sheet.png` ("Edit lab"), `sheet-new.png` ("New connection").
- Keyboard: `kt-all.png` (first Tab reaches the menu button), `k-all2.png` (Enter opens, arrows move, Enter chooses, the focus goes to **Connect**), `Q2-cancelled.png` (a cancelled "Disconnect from…?" question returns the focus to the menu button), `menu-esc.png` and `N-esc.png` (Esc closes the menu).
- Save of a new connection while failed: `save-new.png`. The new connection is active, the old messages are gone and the focus is on **Connect**.
- Unit tests: the menu button text, the Save rule for every state (with a pending keyring read or an outstanding Connect), and the IPv6 address text.

Not checked:

- State F (Disconnecting). It lasts less than a frame against a local broker, so no screenshot caught it. The code path is in `render_empty`.
- The button below 900 px. The window minimum is 960 px.
- A unit test of "the old messages do not stay". The rule is the use of `switch_to`, which calls `clear_data`. The screenshot `save-new.png` shows it.
- 150 % zoom. This is a check for the owner.
