# UI simplification: verification

Branch `feat/ui-simplify` (renamed from `docs/ui-simplify`). Base `e28a26f`. Plan: [2026-11-16-ui-simplify.md](2026-11-16-ui-simplify.md). One local commit for each step. Nothing is pushed.

Windows were checked with Xvfb screenshots (1600 x 1000, and about 1000 px wide for the narrow window). The screenshots are in the scratchpad folder `.../scratchpad/shots/` and are listed at the end. Only the checks in this page were run.

## Gate results

Each step ran `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo test --workspace`. All three passed after every step. The logs are `step<N>-{fmt,clippy,test}.log` in `.../scratchpad/ui/`. Broker tests (Docker) were not run: no core behaviour changed except one counter in the store.

| Step | Commit | fmt | clippy | tests |
|---|---|---|---|---|
| 1 Docs | 5755a46 | n/a (no Rust) | n/a | n/a |
| 2 Title bar, settings, status bar | d8888c8 | pass | pass | pass |
| 3 Subscriptions | e4b6d35 | pass | pass | pass |
| 4 Header, rows, Follow | 26a480c | pass | pass | pass |
| 5 Composer | 1330e5b | pass | pass | pass |
| 6 Details pane | 1b2b2d3 | pass | pass | pass |
| 7 Connection sheet, empty states | 94c3734 | pass | pass | pass |

Release build: passed (see "Release build").

## Per step

### 1 Docs
- `docs/ui-design.md`: sections 3, 3.1, 3.3, 4, 5 (settings menu text), 7, 8, 9 and 11 follow the plan. `docs/user-guide.md`: Connections, Subscribe, Connection details, View messages, Details pane, Publish, Memory limit, Themes, Keyboard. `AGENTS.md`: mqtt-viewer (GPL-3.0) is in the clean-room list.
- The mockups in `mockups/` show the old layout. I left them and added a note in the Mockups section of the design page. See deviations.

### 2 Title bar, settings menu, status bar (4.1, 4.6)
- Title bar: profile select, state, update badge, a gear menu (Commands…, Appearance, Theme, Row height, Memory budget, Reset the layout) and the primary button. The filter moved to the centre header (step 4 sets its hint).
- Status bar: URL, msg/s and a **Details** button always. A tag for "Not encrypted" or "Insecure TLS", the drops, memory above 80%, "N rows removed (memory limit)" and the Reconnect prompt show only when they apply. The Details popover (`Anchor::BottomRight`) shows Connection details (connected only), topics, memory with the budget menu, and drops.
- Tests added: `the_status_bar_shows_an_optional_part_only_when_it_applies`.

### 3 Subscriptions (4.2)
- Add input with a QoS menu as suffix. Row: dot (red cross when rejected), filter, `Q1` tag only for QoS 1, always-visible `⋯` button (tab stop, tooltip "Subscription actions") with Show only, QoS 0/1, Options… (MQTT 5), Retry or Retry remove, Remove. A click on `⋯` does not toggle Show only.
- A status line shows only for "Subscribing…", "Waiting for connection", "Removing…", an error, or "Granted QoS n" when the grant is below the request.
- QoS change: `Subs::check_qos_change` and `commit_qos` with a new request ID, same as options. A failed send keeps the old QoS and shows the failure.
- Options… opens a dialog with the same form as the old popover (`options_form`).
- Palette: Subscription: QoS 0, QoS 1, Options… (MQTT 5 only), Retry, Remove. They act on the subscription in Show only. A row shows its reason ("Select a subscription first.", "The subscription has this QoS already.", "The subscription did not fail.") and choosing it shows a notification instead of running.
- Tests added: `a_qos_change_is_tracked_by_its_request_id`, `a_row_shows_a_status_line_only_for_a_problem_or_a_change`, `subscription_actions_need_a_subscription_in_show_only`.

### 4 Centre header, rows, Follow (4.3)
- Header: Messages | Tree, the filter (hint "Filter by topic or payload" or "Filter by topic"), the `All ▾` direction menu (Messages only), Pause. The Details, Follow and row height buttons are gone.
- Chips: only Show only and a direction other than All, each with a clear button. No text chip.
- Rows: line 1 topic, `R`, time; line 2 preview. Sent rows have the arrow and a result only when not routine. A 2 px colour edge shows only with more than one subscription.
- Follow: `Follow { on }` (on at start; a scroll or a selection turns it off; a pause keeps its state). New store counter `watch_new` / `watched_new` in `navigator-core` counts rows that match the list filter, also when not paused, and resets on a filter change. "Jump to newest (N)" shows when Follow is off and the view is not paused. It scrolls to the top with `scroll_to_item_strict`, keeps the selection and turns Follow on. Palette: "Follow on/off" became "Jump to newest".
- Tests added: store `watched_new_counts_while_not_paused_and_only_matching_rows`; app `follow_goes_off_by_the_user_and_comes_back_by_the_jump`, `a_pause_keeps_the_follow_state_from_before_the_pause`, `the_jump_button_names_the_new_messages`, `the_filter_hint_and_the_chips_follow_the_view`, `a_row_shows_a_result_only_when_it_is_not_routine`, `jump_to_newest_shows_only_when_follow_is_off_and_the_view_runs`.

### 5 Publish composer (4.4)
- `Visibility { open, props }` replaces `collapsed`. Closed by default: one **New message** button. `Ctrl/Cmd+Shift+N` and the palette open it. Edit and publish, Reply, Publish to this topic and History recall open it too (they use `replace_composer`).
- Open: topic, payload, one result or error line, controls row (Text | Hex, QoS 0 | 1, Retain, `⋯`, Send). `⋯`: Format, History…, Properties (MQTT 5, checked), Close. History… opens a dialog list of Drafts and Sent.
- "Properties (N)" button next to the topic when N > 0. The section shows only present fields, each with a remove button, and an "Add property" menu (Payload format (UTF-8), Content type, Response topic, Correlation data, Message expiry, User property). `PropShown` is separate from the text. "Send empty" is unchanged. Recall and Reply open the section when they fill a field.
- One close operation (`close_composer`) for the Close item, `Esc` (binding `escape` in the `Composer` context) and the palette. It moves the focus to the list and keeps the draft (no History entry).
- The palette Send opens a closed composer first.
- Tests added: `the_composer_is_closed_by_default_and_one_close_serves_all`, `a_property_shows_by_presence_not_by_text`, `a_field_that_the_user_adds_stays_visible_while_it_is_empty`, `the_composer_entries_open_it_and_close_shows_only_when_it_is_open`.

### 6 Details pane (4.5)
- Wide window: the pane is closed at start and opens on a selection (mouse or keyboard). Its close button and `Esc` (context `DetailsPane`) close it and move the focus to the list. Narrow window: a click opens the sheet, Up/Down keep the focus in the list, `Enter` opens the sheet (binding in the list context), closing the sheet returns the focus to the list. Palette: "Details: Open".
- Header: the full topic in a read-only input (selectable) and a `⋯` menu: Copy topic, Copy payload, Publish to this topic, Reply (only with a Response Topic), Decoder (submenu), Wrap, Expand (not in the sheet), Clear retained… (last, after a separator).
- **Edit and publish** is the one action button, with its reason when off. Properties is a collapsed section ("Properties (N)").
- History: choosing a row moves the focus to the Value tab. The tab shows "Earlier message from hh:mm:ss" (or "Later message from…" when the row is newer than the selection) and **Show latest**.
- Tests added: `a_history_row_is_earlier_later_or_the_selection_itself`, `details_open_is_listed_while_the_details_are_closed`.

### 7 Connection sheet and empty states (4.7, 4.8)
- Client ID (with New ID) is the first field of Advanced. Certificate, key and passphrase are in a collapsed "Client certificate" section ("In use" shows when it is closed and a file is set). A validation error opens the section (`section_of`) before the focus moves.
- Empty states: "No subscriptions" with "Subscribe to a topic filter to see messages." and "The broker does not allow #." with "Subscribe to a narrower filter.". Both have the action "Add a topic filter", which focuses the add input.
- Tests added: `a_wrong_field_opens_the_collapsed_section_that_holds_it`, `the_two_empty_states_of_the_subscriptions`.

## Expectations changed

No existing assertion was removed or weakened. The new behaviour changed these things that no test covered:
- Palette: "Follow on", "Follow off" (replaced by "Jump to newest") and "Collapse or expand the composer" (replaced by New message, Composer: Properties, Composer: History, Close the composer) are gone.
- The old "Connection details" status button is now **Details**.
- The row height button, the Details button and the Follow toggle left the centre header.
- Reset the layout no longer sets the details pane open or the composer expanded.

## Deviations from the plan, with reasons

1. **QoS select on the add input** is a ghost `Button::dropdown_menu` with two checked items, not a `Select`. A `Select` needs its own state entity and events; the menu gives the same result and the same checked state.
2. **Options…** opens a dialog, not a popover, because a menu item cannot anchor a popover.
3. **History…** opens a dialog list for the same reason, and it lets the palette open it.
4. **"Jump to newest"** has no "(0)": the count shows only when N > 0.
5. **"Properties (N)"** is a small button, not a `Tag` (a `Tag` has no click handler).
6. **"Later message from…"** shows when the History row is newer than the selected message. The plan names only "Earlier message from…".
7. **Subscription row height** stays fixed (the list is virtual). A row without a status line keeps its second line empty.
8. **Palette "Show or hide the details"** stays next to the new "Details: Open", because the plan says that the existing entries stay.
9. **Mockups** were not removed. They show the old layout, so the design page now says so. Update or remove them in a later change.
10. **Esc order** is by scoped bindings (`Composer`, `DetailsPane`). The deepest control that handles `Esc` (menu, dialog, the filter, the editor) goes first and the binding runs only when it passes the key on. This is gpui behaviour; there is no automated test of the key path (the app has no `gpui_kit::test` UI tests). The state change has tests.
11. **No `#[gpui_kit::test]`**: the app has no such tests today, so the new tests are plain unit tests of the logic.
12. The status bar no longer shows "Reconnected". The list still has the marker row.

## Release build

`cargo build --release -p navigator-app` finished in 4 min 14 s (log: `release-build.log`). The binary `target/release/mqtt-navigator` is 35 683 224 B. The CI gate `MAX_BYTES` is 37 317 165 B, so the build is 1 633 941 B (4.4%) under the limit. The size on 2026-11-11 was 33 924 696 B; the previous local release binary (before this work) was 35 724 888 B, so this work did not add size. The size is from a Linux build on this machine, as `stat -c %s` in the CI step.

## Manual checks for the owner

Use a broker that you trust. A narrow window is below 1 100 px.

1. **Start.** Connect. Expect: title bar with profile, state, gear and Disconnect. Centre header with Messages | Tree, a filter, `All ▾`, Pause. Details pane closed. Bottom of the centre pane: **New message** and `Ctrl+Shift+N`.
2. **Gear menu.** Click the gear. Expect Commands…, Appearance, Theme, Row height, Memory budget, Reset the layout. Pick 1 line: rows become one line.
3. **Status bar.** Expect URL, msg/s, **Details**. Click Details: Connection details (connected), Topics, Memory with a budget menu, Dropped.
4. **Subscriptions.** Click `⋯` on a row: Show only, QoS 0/1, Options… (MQTT 5), Remove. Choose QoS 1: the row shows `Q1` and "Subscribing…". Use the `Q0 ▾` menu on the input, type `demo/#`, press Enter. Press `Tab` to reach `⋯`.
5. **Show only** from the menu: the list shows only that filter, and a chip with a **×** shows.
6. **Palette.** `Ctrl+K`, type "sub": the subscription entries show "Select a subscription first." until a subscription is in Show only.
7. **Follow.** Select a row, then publish two messages from another client. Expect "Jump to newest (2)". Click it: the list goes to the top and the selection stays. Press Pause: the button hides and the header shows "Paused · N new".
8. **Direction.** Open `All ▾`, choose Sent: chip "Sent" with **×**. Clear it with the **×**.
9. **Composer.** `Ctrl+Shift+N`: the composer opens with the focus in Topic. Type a topic and a payload. Press `Esc`: it closes, the focus is in the list. Press `Ctrl+Shift+N` again: the draft is back. Press `Ctrl+Enter` to send. Expect a result line under the payload.
10. **Composer menu.** `⋯`: Format, History…, Properties, Close. Properties, Add property, Content type: the field shows and has the focus. A "Properties (1)" button shows next to the topic after you type a value (or turn on Send empty). Close and reopen: it is still there.
11. **Palette Send.** Close the composer, run "Send" from the palette: the composer opens and nothing is sent.
12. **Details (wide).** Click a row: the pane opens with the topic as selectable text. `⋯` has Copy topic, Copy payload, Publish to this topic, Decoder, Wrap, Expand, Clear retained… (only for a retained topic). **Edit and publish** opens the composer. Click **×**: the pane closes and the next selection opens it. `Esc` in the pane also closes it.
13. **History.** Publish two values to one topic, open the History tab, choose the older row: the Value tab shows "Earlier message from …" and **Show latest**.
14. **Details (narrow).** Make the window narrower than 1 100 px. Click a row: the sheet opens. Close it: the focus is in the list. Press Down: no sheet. Press `Enter`: the sheet opens.
15. **Connection sheet.** `Ctrl+N`. Expect no Client ID in the basic part. Open "Client certificate" and "Advanced" (Client ID is the first field). With a certificate and no key, press Save: the Client certificate section is open and shows the message.
16. **Empty states.** Remove the last subscription on an empty list: "No subscriptions" with "Add a topic filter" (focuses the input). Subscribe to `#` on a broker that rejects it: "The broker does not allow #."
17. **Themes.** Check the new parts in a light and a dark theme (contrast of the muted text, the red cross, the jump button).
18. **Keyboard only.** Tab through the window. Every `⋯` button must be reachable and show a focus ring.

## Screenshots

Folder: `/tmp/claude-1000/-home-pdeng-Workspace-mqtt-navigator/ab4fac5c-4e83-4bee-944a-0cfdac32c453/scratchpad/shots/`. The set is for the owner and the reviewer. A final screenshot of each step was taken with the step binary.

- Step 2: `step2-settings.png`, `step2-details-popover.png`
- Step 3: `step3-main.png`, `step3-rowmenu.png`, `step3-palette.png`, `step3-options-dialog.png`
- Step 4: `step4-follow-off.png`, `step4-after-jump.png`, `step4-direction-menu.png`, `step4-direction-chip.png`
- Step 5: `step5-closed.png`, `step5-open.png`, `step5-more-menu.png`, `step5-add-menu.png`, `step5-property-typed.png`, `step5-esc-closed.png`, `step5-reopen-draft.png`
- Step 6: `step6-selected.png`, `step6-menu.png`, `step6-history.png`, `step6-earlier.png`, `step6-narrow-keyboard.png`, `step6-narrow-enter.png`
- Step 7: `step7-empty-nosubs.png`, `step7-sheet.png`, `step7-sheet-open.png`, `step7-advanced-clientid.png`

Not screenshotted: the "broker rejects #" state (needs a broker that rejects it; covered by a unit test), the light theme, and a failed send row.
