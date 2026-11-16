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

## Codex round 1 fixes

One commit. fmt, clippy (-D warnings) and `cargo test --workspace` passed (logs `stepr1-*.log`).

| # | Fix | Test | Screenshot |
|---|---|---|---|
| 1 | The History dialog lists Drafts and Sent as buttons. They are tab stops: Tab moves, Enter or Space recalls. | `a_history_entry_is_recalled_by_its_sequence_number` (model). The key path itself is gpui and has no automated test. | none |
| 2 | The Value tab shows "Result: <status>" for the selected sent message (`sent_status`, read from the store row, not the composer). A failed result is red. | the status texts are covered by `a_row_shows_a_result_only_when_it_is_not_routine` and `status_text_follows_the_design` | `r1-details-result.png` |
| 3 | A profile change sets `follow` to its default together with the new store. The next selection then starts the count again. | `a_profile_change_restarts_follow_and_its_counter_together` | none |
| 4 | "Properties (N)" comes from the small property fields (`PropPresence`), not from `read_draft`. On MQTT 3.1.1 the count is not computed. | `the_flag_count_equals_the_draft_count` | none |
| 5 | "Jump to newest" has its own strip above the list. No overlay. | none (layout) | `r1-jump-strip.png` |
| 6 | After Jump the focus goes to the list. After Show latest it goes to the Value area. | none (focus is gpui). Checked in a window: after clicking Jump, the Down key selected the top row (`r1-after-jump-down.png`). | `r1-after-jump-down.png` |
| 7 | "Show or hide the details" uses one close (`close_details_area`): focus returns to the list, and in a narrow window `sheet_open` is cleared, so later opens work. | none (window state) | none |
| 8 | Follow turns off only when the scroll offset has left the top (`left_the_top`). | `only_a_scroll_that_leaves_the_top_turns_follow_off`. Checked in a window: wheel up at the top shows no strip, wheel down does (`r1-scroll-up-at-top.png`, `r1-jump-strip.png`). | `r1-scroll-up-at-top.png` |
| 9 | The security tag has the warning fill and the theme text colour that reads best on it (`readable_on`). Measured contrast from the screenshots: dark 9.5:1, light 7.9:1 (was about 1.8:1). | `a_tag_takes_the_text_colour_that_reads_on_its_fill` | `r1-tag-dark.png`, `r1-tag-light.png`, `r1-tag-dark-zoom.png`, `r1-tag-light-zoom.png` |

Deviation 8 (the palette entry "Show or hide the details") stays, now with the shared close path.

### Window checks of the Codex round 1 gaps

Done with `xdotool` on Xvfb (keys; a mouse click only to put the focus in a pane or to open a tab). The first run found two defects. Both are fixed in the commit of this round.

| Check | Result | Screenshots |
|---|---|---|
| (a) History recall by keyboard | **Defect:** Tab reached an entry, but Enter closed the dialog without recalling, because the dialog takes Enter as "confirm". Fix: a `HistoryList` key context binds Enter to `NoAction`, so the entry gets it. After the fix: Tab, Enter put the sent entry "first" in the composer (Space worked before the fix). | `r2-a-tab.png`, `r2-a-recalled.png` (before), `r2-a-enter.png` (after) |
| (b) Jump to newest by keyboard | **Defect:** Tab reached the strip button, but Enter did nothing, because the list context binds Enter to "open details". Fix: a `JumpStrip` context binds Enter to `NoAction`. After the fix: Enter jumped to the top, and Down then selected the next row (focus in the list). | `r2-b-focus.png`, `r2-b-jumped.png`, `r2-b-after.png` |
| (c) Show latest by keyboard | Passed. Tab reached **Show latest**, Enter showed the latest value (`"v": 2`), and `Esc` then closed the pane, so the focus was in the Value area. | `r2-c-tab.png`, `r2-c-latest.png`, `r2-c-esc.png` |
| (d) Show or hide the details, wide | Passed. The palette entry closed the pane. Down then selected the next row and the pane opened again, so the focus was in the list. | `r2-d-wide-closed.png`, `r2-d-wide-down.png` |
| (d) narrow (1000 px) | Enter opened the sheet. Closing the sheet (click outside) and then Down and Enter opened it again, so `sheet_open` was cleared. The palette entry opens the sheet in a narrow window. **Not reachable:** the palette entry cannot close an open sheet, because `Ctrl+K` does nothing while the sheet has the focus. That branch has no screenshot. | `r2-d-n1-open.png`, `r2-d-n4-reopen-after-close.png`, `r2-d-n5-palette-opens.png` |
| (e) Result in the details | Acknowledged: a QoS 1 message shows "Result: Acknowledged". Rejected: **not captured**; the test Mosquitto accepts every publish. | `r2-e-ack.png` |

## Codex round 2 fixes

Commits 6478d9e (items 1 and 2) and the commit after it (items 3 and 4). fmt, clippy (-D warnings) and `cargo test --workspace` passed (`stepr2-*.log`, `stepr3-*.log`).

1. **History dialog Enter.** A `HistoryList` key context binds Enter to `NoAction`, so the dialog does not confirm before the focused entry gets the key. The app has no `gpui_kit::test` UI tests, so the key path is checked in a window: Tab, Enter put the entry in the composer (`r3-a-recalled.png`; the failing run before the fix is `r2-a-recalled.png`).
2. **Jump to newest Enter.** A `JumpStrip` context binds Enter to `NoAction`, so the list's "open details" binding does not take it. In a window: Tab, Enter jumped, and Down then moved the selection, so the focus is in the list (`r3-b-after.png`).
3. **Send result by row id.** `TopicStore::row_send_status(id)` reads the status of a row regardless of the list filter or a pause. The details use it. Test: `a_sent_row_keeps_its_status_when_a_filter_hides_it` (the result is `Sending`, stays while a filter hides the row, then becomes `Acknowledged`). In a window: with the filter "Received" the sent row is hidden and the details still show "Result: Acknowledged" (`r3-e-filtered.png`).
4. **Property removal focus.** Removing a property (single field or user row) focuses the payload editor, which stays. In a window: after removing Content type, typed text went to the payload (`r3-after-remove.png`). The user-property row uses the same call; it was not exercised in a window.

Checks (c) and (d) of the first list (Show latest, details toggle, narrow sheet) were not repeated after round 2, because round 2 did not change those paths. Rejected results are still not captured (the test broker accepts every publish).

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
