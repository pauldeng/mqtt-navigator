//! The details pane (inspector): Value and History tabs, the decoder, the background render
//! job, and the actions that fill the composer. Below 1 100 px the pane opens as a sheet.

use gpui_kit::component::description_list::{DescriptionItem, DescriptionList};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::sheet::Sheet;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{IndexPath, Placement};
use navigator_core::decode::{self, DEFAULT_LIMIT, Endian, NumberKind, Rendered};

use super::*;
use crate::composer::{self, Draft};
use crate::inspector::{
    AUTO_BYTES, DecoderChoice, DecoderPick, FIRST_BYTES, Jobs, MORE_BYTES, NUMBER_KINDS, meta_line,
    next_cut, next_step, props_rows, size_text,
};

/// Below this window width the details open as a right sheet.
pub(super) const NARROW: f32 = 1100.;
const DETAIL_WIDTH: f32 = 360.;
/// Height of one History row: two lines of small text and the padding.
const HIST_ROW_H: f32 = 44.;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DetailTab {
    Value,
    History,
}

#[derive(Clone)]
pub(super) struct DecoderItem(DecoderPick);

impl SearchableListItem for DecoderItem {
    type Value = DecoderPick;

    fn title(&self) -> SharedString {
        self.0.label().into()
    }

    fn value(&self) -> &DecoderPick {
        &self.0
    }
}

#[derive(Clone)]
pub(super) struct KindItem(NumberKind, &'static str);

impl SearchableListItem for KindItem {
    type Value = NumberKind;

    fn title(&self) -> SharedString {
        self.1.into()
    }

    fn value(&self) -> &NumberKind {
        &self.0
    }
}

/// What the payload view was built for.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Want {
    key: DetailKey,
    choice: DecoderChoice,
    all: bool,
}

/// A hash of the rendered text and its state, to see that a new result changes nothing.
fn text_hash(r: &Rendered) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (&r.text, r.truncated_at, &r.error).hash(&mut h);
    h.finish()
}

/// Same subject: a new latest value of the same topic is not a new subject.
fn same_subject(a: DetailKey, b: DetailKey) -> bool {
    match (a, b) {
        (DetailKey::Node(x, ..), DetailKey::Node(y, ..)) => x == y,
        (a, b) => a == b,
    }
}

/// The state of the payload view.
enum PayloadView {
    /// Nothing was built yet.
    Idle,
    /// The job runs. `blank`: the editor shows another message, so it shows "Formatting…".
    Running { blank: bool },
    Done {
        truncated_at: Option<usize>,
        error: Option<String>,
    },
}

pub(super) struct Inspector {
    pub(super) tab: DetailTab,
    pub(super) choice: DecoderChoice,
    pub(super) wrap: bool,
    applied_wrap: bool,
    /// Hash of the text in the payload editor.
    text_hash: Option<u64>,
    pub(super) expanded: bool,
    show_all: bool,
    /// Metrics only (`MQTT_NAVIGATOR_AUTOSELECT`): a new subject starts with Show all.
    auto_all: bool,
    /// The whole rendered text. The editor shows its first `shown_len` bytes. The text grows in
    /// steps (see `crate::inspector::next_step`) up to `more_target`.
    full: String,
    shown_len: usize,
    more_target: usize,
    /// The size of the next step.
    step: usize,
    more_task: Option<Task<()>>,
    jobs: Jobs,
    want: Option<Want>,
    view: PayloadView,
    task: Option<Task<()>>,
    editor: Entity<EditorState>,
    path: Entity<InputState>,
    shown_path: SharedString,
    pub(super) decoder_select: Entity<SelectState<Vec<DecoderItem>>>,
    kind_select: Entity<SelectState<Vec<KindItem>>>,
    /// A History row that the user chose. The Value tab shows it instead of the selection.
    pub(super) hist_pick: Option<Msg>,
    /// The History list takes the keys. `hist_cursor` is the row that Up and Down moved to.
    hist_focus: FocusHandle,
    hist_cursor: Option<SystemTime>,
    hist_scroll: UniformListScrollHandle,
    /// The details sheet of a narrow window is open.
    pub(super) sheet_open: bool,
    /// Open the sheet at the next frame (a row was selected in a narrow window).
    pub(super) want_sheet: bool,
    _subs: Vec<Subscription>,
}

impl Inspector {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Navigator>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(false)
                .searchable(false)
                .soft_wrap(true)
        });
        editor.update(cx, |e, cx| e.set_readonly(true, cx));
        let path = cx.new(|cx| InputState::new(window, cx));
        path.update(cx, |p, cx| p.set_readonly(true, cx));
        let items = DecoderPick::ALL.iter().map(|p| DecoderItem(*p)).collect();
        let decoder_select =
            cx.new(|cx| SelectState::new(items, Some(IndexPath::new(0)), window, cx));
        let kinds: Vec<KindItem> = NUMBER_KINDS.iter().map(|(k, n)| KindItem(*k, n)).collect();
        let kind_select = cx.new(|cx| SelectState::new(kinds, Some(IndexPath::new(4)), window, cx));
        let subs = vec![
            cx.subscribe(
                &decoder_select,
                |v, _, ev: &SelectEvent<Vec<DecoderItem>>, cx| {
                    let SelectEvent::Confirm(pick) = ev;
                    if let Some(p) = pick {
                        v.insp.choice.pick = *p;
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(&kind_select, |v, _, ev: &SelectEvent<Vec<KindItem>>, cx| {
                let SelectEvent::Confirm(kind) = ev;
                if let Some(k) = kind {
                    v.insp.choice.kind = *k;
                    cx.notify();
                }
            }),
        ];
        Self {
            tab: DetailTab::Value,
            choice: DecoderChoice::default(),
            wrap: true,
            applied_wrap: true,
            text_hash: None,
            expanded: false,
            show_all: false,
            auto_all: false,
            full: String::new(),
            shown_len: 0,
            more_target: 0,
            step: FIRST_BYTES,
            more_task: None,
            jobs: Jobs::default(),
            want: None,
            view: PayloadView::Idle,
            task: None,
            editor,
            path,
            shown_path: SharedString::default(),
            decoder_select,
            kind_select,
            hist_pick: None,
            hist_focus: cx.focus_handle(),
            hist_cursor: None,
            hist_scroll: UniformListScrollHandle::new(),
            sheet_open: false,
            want_sheet: false,
            _subs: subs,
        }
    }

    /// The selection is gone (profile switch): forget the shown value.
    pub(super) fn reset(&mut self) {
        self.jobs.cancel();
        self.task = None;
        self.want = None;
        self.view = PayloadView::Idle;
        self.show_all = false;
        self.hist_pick = None;
        self.hist_cursor = None;
        self.text_hash = None;
        self.full = String::new();
        self.shown_len = 0;
        self.more_target = 0;
        self.more_task = None;
    }

    /// Metrics only: the next subject starts with Show all.
    pub(super) fn set_auto_all(&mut self) {
        self.auto_all = true;
    }
}

/// The message that the details show, copied out of the store.
pub(super) struct Shown {
    pub(super) topic: String,
    pub(super) node: Option<NodeId>,
    pub(super) msg: Msg,
    pub(super) direction: Direction,
    pub(super) key: DetailKey,
    /// The list row is gone (memory limit).
    pub(super) evicted: bool,
}

/// A view for the details sheet. It draws the details of the window.
pub(super) struct DetailsHost {
    nav: WeakEntity<Navigator>,
    _observe: Subscription,
}

impl DetailsHost {
    fn new(nav: &Entity<Navigator>, cx: &mut Context<Self>) -> Self {
        Self {
            nav: nav.downgrade(),
            _observe: cx.observe(nav, |_, _, cx| cx.notify()),
        }
    }
}

impl Render for DetailsHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.nav.upgrade() {
            Some(nav) => nav
                .update(cx, |v, cx| v.render_details(window, true, cx))
                .into_any_element(),
            None => div().into_any_element(),
        }
    }
}

impl Navigator {
    /// What the Value tab shows now.
    pub(super) fn shown(&self) -> Option<Shown> {
        let hist = |node: Option<NodeId>, topic: String, direction: Direction| {
            let m = self.insp.hist_pick.clone()?;
            let key = DetailKey::Hist(node?, m.received_at, m.payload.len(), m.truncated);
            Some(Shown {
                topic,
                node,
                msg: m,
                direction,
                key,
                evicted: false,
            })
        };
        match self.detail_of {
            DetailOf::Nothing => None,
            DetailOf::Message => {
                let s = self.sel_msg.as_ref()?;
                let node = self.live_node(s.node, &s.topic);
                // A History entry is a received message, also when the selected row is a sent one.
                if let Some(h) = hist(node, s.topic.to_string(), Direction::Received) {
                    return Some(h);
                }
                Some(Shown {
                    topic: s.topic.to_string(),
                    // A sent row has no node; the broker echo may have created one since.
                    node,
                    msg: s.msg.clone(),
                    direction: s.direction,
                    key: DetailKey::Row(s.id),
                    evicted: self.store.row_evicted(s.id),
                })
            }
            DetailOf::Topic => {
                let id = self.sel_topic?;
                let n = self.store.get(id)?;
                let topic = self.store.path(id);
                if let Some(h) = hist(Some(id), topic.clone(), Direction::Received) {
                    return Some(h);
                }
                let m = n.latest?.clone();
                Some(Shown {
                    topic,
                    node: Some(id),
                    key: DetailKey::Node(id, m.received_at, m.payload.len(), m.truncated),
                    msg: m,
                    direction: Direction::Received,
                    evicted: false,
                })
            }
        }
    }

    /// The topic of the selection and its node. It does not depend on a payload: a branch of the
    /// tree or a topic with an evicted value is a topic, too.
    pub(super) fn selected_topic(&self) -> Option<(String, Option<NodeId>)> {
        match self.detail_of {
            DetailOf::Nothing => None,
            DetailOf::Message => {
                let s = self.sel_msg.as_ref()?;
                Some((s.topic.to_string(), self.live_node(s.node, &s.topic)))
            }
            DetailOf::Topic => {
                let id = self.sel_topic?;
                self.store.get(id)?;
                Some((self.store.path(id), Some(id)))
            }
        }
    }

    /// True if the broker keeps a retained message for the selected topic, as far as the store
    /// knows (it does not depend on the retain flag of the shown message).
    fn has_retained(&self) -> bool {
        self.selected_topic()
            .is_some_and(|(topic, _)| self.store.has_retained_topic(&topic))
    }

    /// The finished render belongs to `s` (and to the decoder and limit now chosen).
    fn render_ready(&self, s: &Shown) -> bool {
        matches!(self.insp.view, PayloadView::Done { .. })
            && self.insp.want
                == Some(Want {
                    key: s.key,
                    choice: self.insp.choice,
                    all: self.insp.show_all,
                })
    }

    /// Copy needs a finished render of the shown value.
    fn can_copy(&self, shown: Option<&Shown>) -> bool {
        shown.is_some_and(|s| self.render_ready(s)) && !self.insp.full.is_empty()
    }

    /// "Show all": the payload is cut, or the decoder needs all of it, and a render is done.
    fn can_show_all(&self, shown: Option<&Shown>) -> bool {
        let PayloadView::Done {
            truncated_at,
            error,
        } = &self.insp.view
        else {
            return false;
        };
        shown.is_some_and(|s| {
            self.render_ready(s)
                && !self.insp.show_all
                && s.msg.payload.len() > DEFAULT_LIMIT
                && (truncated_at.is_some() || error.is_some())
        })
    }

    /// "Show more": the rendered text is longer than the part in the editor.
    fn can_show_more(&self, shown: Option<&Shown>) -> bool {
        shown.is_some_and(|s| self.render_ready(s))
            && self.insp.more_task.is_none()
            && self.insp.shown_len < self.insp.full.len()
    }

    /// The facts that the registry needs.
    pub(super) fn facts(&self) -> crate::palette::Facts {
        let shown = self.shown();
        let topic = self.selected_topic();
        let can_edit = shown
            .as_ref()
            .is_some_and(|s| composer::edit_unavailable(&s.msg, s.evicted).is_none());
        crate::palette::Facts {
            has_profile: self.profile.is_some(),
            connected: self.connected(),
            connecting: self.connecting()
                || matches!(
                    self.status.state,
                    ConnState::Connecting | ConnState::Reconnecting { .. }
                ),
            messages_view: self.view == View::Messages,
            filters_active: self.filters_active(),
            paused: self.store.is_paused(),
            follow: self.follow,
            two_lines: self.two_lines,
            details_open: self.details_open,
            has_value: shown.is_some(),
            can_publish_to_topic: topic.is_some(),
            can_edit,
            has_reply: shown.as_ref().is_some_and(|s| {
                s.msg
                    .props
                    .response_topic
                    .as_deref()
                    .is_some_and(|t| !t.is_empty())
            }),
            has_retained: self.has_retained(),
            can_copy: self.can_copy(shown.as_ref()),
            wrap: self.insp.wrap,
            number: (self.insp.choice.pick == DecoderPick::Number)
                .then_some((self.insp.choice.kind, self.insp.choice.endian)),
            can_show_all: self.can_show_all(shown.as_ref()),
            can_show_more: self.can_show_more(shown.as_ref()),
            payload_mode: self.comp.mode,
            theme: Some(self.settings.ui.theme_mode),
            memory_mib: self.memory_mib(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Background job
    // -----------------------------------------------------------------------------------------

    /// Starts a render job if the shown message, the decoder or the limit changed. The job runs
    /// on a background thread. A new job cancels the old one, and a result of an old job is
    /// dropped (see [`Jobs`]).
    fn ensure_render(
        &mut self,
        shown: Option<&Shown>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = shown else {
            if self.insp.want.take().is_some() {
                self.insp.jobs.cancel();
                self.insp.task = None;
                self.insp.view = PayloadView::Idle;
            }
            return;
        };
        let subject_changed = self.insp.want.is_none_or(|w| !same_subject(w.key, s.key));
        if subject_changed {
            self.insp.show_all = self.insp.auto_all;
        }
        let want = Want {
            key: s.key,
            choice: self.insp.choice,
            all: self.insp.show_all,
        };
        if self.insp.want == Some(want) {
            return;
        }
        self.insp.want = Some(want);
        let (generation, cancel) = self.insp.jobs.start();
        self.insp.view = PayloadView::Running {
            blank: subject_changed || matches!(self.insp.view, PayloadView::Idle),
        };
        let payload = s.msg.payload.clone();
        // Auto scans the payload (UTF-8 check): the job does it, not the UI thread. Only Auto
        // reads the properties.
        let choice = want.choice;
        let props = if choice.pick == DecoderPick::Auto {
            s.msg.props.clone()
        } else {
            Default::default()
        };
        let limit = if want.all {
            payload.len()
        } else {
            DEFAULT_LIMIT
        };
        self.insp.task = Some(cx.spawn_in(window, async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move {
                    let decoder = choice.resolve(&payload, &props);
                    let out = decode::render(decoder, &payload, limit, &cancel);
                    let hash = text_hash(&out);
                    (out, hash)
                })
                .await;
            this.update_in(cx, |v, window, cx| {
                v.render_done(generation, out.0, out.1, window, cx)
            })
            .ok();
        }));
    }

    fn render_done(
        &mut self,
        generation: u64,
        out: Rendered,
        hash: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.insp.jobs.is_current(generation) {
            return;
        }
        let Rendered {
            text,
            truncated_at,
            error,
        } = out;
        // A live topic sends the same text again and again: set_value would reset the scroll
        // position and the selection. The same text for the same subject changes nothing.
        let same = matches!(self.insp.view, PayloadView::Running { blank: false })
            && self.insp.text_hash == Some(hash);
        if !same {
            self.insp.text_hash = Some(hash);
            // The editor takes a small first window. The text then grows in timed steps.
            let cut = next_cut(&text, 0, FIRST_BYTES);
            let first = text[..cut].to_string();
            self.insp.full = text;
            self.insp.shown_len = cut;
            self.insp.step = FIRST_BYTES;
            self.insp
                .editor
                .update(cx, |e, cx| e.set_value(first, window, cx));
            self.insp.more_target = self.insp.full.len().min(AUTO_BYTES);
            self.insp.more_task = None;
            self.schedule_step(window, cx);
        }
        self.insp.view = PayloadView::Done {
            truncated_at,
            error,
        };
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // Actions of the pane
    // -----------------------------------------------------------------------------------------

    pub(super) fn set_decoder(
        &mut self,
        pick: DecoderPick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insp.choice.pick = pick;
        self.insp.decoder_select.update(cx, |s, cx| {
            s.set_selected_value(&pick, window, cx);
        });
        cx.notify();
    }

    /// Copies the whole rendered text, but only if the finished render is the one of the
    /// current selection (not of another message, and not while it runs).
    pub(super) fn copy_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.shown();
        if !self.can_copy(shown.as_ref()) {
            window.push_notification(
                Notification::warning("The value is not ready. Open the details and wait."),
                cx,
            );
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(self.insp.full.clone()));
        window.push_notification(Notification::success("Value copied."), cx);
    }

    /// The width and the byte order of the Number decoder. The decoder is Number from now.
    pub(super) fn set_number(
        &mut self,
        kind: Option<NumberKind>,
        endian: Option<Endian>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(k) = kind {
            self.insp.choice.kind = k;
            self.insp.kind_select.update(cx, |s, cx| {
                s.set_selected_value(&k, window, cx);
            });
        }
        if let Some(e) = endian {
            self.insp.choice.endian = e;
        }
        self.set_decoder(DecoderPick::Number, window, cx);
    }

    pub(super) fn set_wrap(&mut self, on: bool, cx: &mut Context<Self>) {
        self.insp.wrap = on;
        cx.notify();
    }

    /// Renders all of the payload (in the background). The editor still gets chunks.
    pub(super) fn show_all(&mut self, cx: &mut Context<Self>) {
        self.insp.show_all = true;
        cx.notify();
    }

    /// "Show more": the text grows by [`MORE_BYTES`], in steps over the next frames.
    pub(super) fn show_more(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = (self.insp.shown_len + MORE_BYTES).min(self.insp.full.len());
        if target > self.insp.more_target {
            self.insp.more_target = target;
        }
        self.schedule_step(window, cx);
    }

    /// Runs the next step in a later frame, if the text has not reached its target. The timer
    /// starts from an event (a new text, a click) and ends with the last step.
    fn schedule_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.insp.shown_len >= self.insp.more_target {
            self.insp.more_task = None;
            return;
        }
        self.insp.more_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            this.update_in(cx, |v, window, cx| v.grow_step(window, cx))
                .ok();
        }));
        cx.notify();
    }

    /// Appends one step of the rendered text to the editor. The step size comes from the time
    /// of the last step, so that no frame takes much longer than the budget.
    fn grow_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.insp.shown_len;
        let want = self.insp.more_target.min(self.insp.full.len());
        if from >= want {
            self.insp.more_task = None;
            return;
        }
        let to = next_cut(&self.insp.full, from, self.insp.step.min(want - from));
        if to <= from {
            // No progress is possible: stop, and never schedule again for this target.
            self.insp.more_target = self.insp.shown_len;
            self.insp.more_task = None;
            return;
        }
        let chunk = self.insp.full[from..to].to_string();
        let t = Instant::now();
        self.insp.editor.update(cx, |e, cx| {
            // Append at the end, and keep the reading position.
            let (end, scroll) = (e.text().len(), e.scroll_offset());
            e.set_selected_range(end..end, cx);
            e.insert(chunk, window, cx);
            e.set_selected_range(0..0, cx);
            e.set_scroll_offset(scroll, cx);
        });
        let took = t.elapsed();
        self.metrics.ui_step("grow", to - from, took);
        self.insp.step = next_step(to - from, took.as_secs_f32() * 1000.);
        self.insp.shown_len = to;
        self.schedule_step(window, cx);
        cx.notify();
    }

    pub(super) fn toggle_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.narrow {
            if self.insp.sheet_open {
                window.close_sheet(cx);
            } else {
                self.details_open = true;
                self.open_details_sheet(window, cx);
            }
        } else {
            self.details_open = !self.details_open;
        }
        cx.notify();
    }

    /// Opens the details as a right sheet. Only one sheet can be open.
    pub(super) fn open_details_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.insp.sheet_open || window.has_active_sheet(cx) {
            return;
        }
        self.insp.sheet_open = true;
        let nav = cx.entity();
        let host = cx.new(|cx| DetailsHost::new(&nav, cx));
        let weak = nav.downgrade();
        window.open_sheet_at(Placement::Right, cx, move |sheet: Sheet, _, _| {
            let weak = weak.clone();
            sheet
                .title("Details")
                .size(px(DETAIL_WIDTH + 60.))
                .on_close(move |_, _, cx| {
                    weak.update(cx, |v, _| v.insp.sheet_open = false).ok();
                })
                .child(host.clone())
        });
    }

    /// The sheet closes before an action fills the composer.
    fn close_details_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.insp.sheet_open {
            self.insp.sheet_open = false;
            window.close_sheet(cx);
        }
    }

    pub(super) fn publish_to_topic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((topic, _)) = self.selected_topic() else {
            return;
        };
        self.close_details_sheet(window, cx);
        let mut d = self.read_draft(cx);
        d.topic = topic;
        let unsent = d.has_content();
        self.replace_composer(d, unsent, window, cx);
    }

    pub(super) fn edit_and_publish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.shown() else {
            return;
        };
        if composer::edit_unavailable(&s.msg, s.evicted).is_some() {
            return;
        }
        self.close_details_sheet(window, cx);
        self.replace_composer(Draft::from_message(&s.topic, &s.msg), true, window, cx);
    }

    pub(super) fn reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.shown() else {
            return;
        };
        let Some(d) = Draft::reply(&s.msg, self.comp.qos) else {
            return;
        };
        self.close_details_sheet(window, cx);
        self.replace_composer(d, true, window, cx);
    }

    /// Asks before it clears the retained message of the shown topic. "Cancel" has the focus.
    pub(super) fn confirm_clear_retained(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((topic, _)) = self.selected_topic() else {
            return;
        };
        let weak = cx.entity().downgrade();
        let cancel_focus = cx.focus_handle();
        let first = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                cancel_focus.focus(window, cx);
            }
            let (weak, topic) = (weak.clone(), topic.clone());
            dialog
                .title(format!("Clear the retained message on {topic}?"))
                .child(
                    div()
                        .text_sm()
                        .child("The broker stops keeping a message for this topic."),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("clear-retained-cancel")
                                .label("Cancel")
                                .track_focus(&cancel_focus)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("clear-retained-ok")
                                .danger()
                                .label("Clear retained message")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let topic = topic.clone();
                                    weak.update(cx, |v, cx| v.clear_retained(topic, window, cx))
                                        .ok();
                                }),
                        ),
                )
        });
    }

    /// Publishes an empty payload with Retain on (QoS 1). The broker then deletes its copy.
    fn clear_retained(&mut self, topic: String, window: &mut Window, cx: &mut Context<Self>) {
        let d = Draft {
            topic,
            qos: Qos::AtLeastOnce,
            retain: true,
            ..Draft::default()
        };
        self.publish_draft(&d, false, window, cx);
    }

    // -----------------------------------------------------------------------------------------
    // Render
    // -----------------------------------------------------------------------------------------

    /// The pane. `sheet`: it is drawn in the narrow-window sheet (no close or expand button).
    pub(super) fn render_details(
        &mut self,
        window: &mut Window,
        sheet: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = self.shown();
        self.ensure_render(shown.as_ref(), window, cx);
        // The path input shows the topic. It changes with the topic only.
        let path: SharedString = shown
            .as_ref()
            .map(|s| s.topic.clone().into())
            .unwrap_or_default();
        if path != self.insp.shown_path {
            self.insp.shown_path = path.clone();
            self.insp
                .path
                .update(cx, |p, cx| p.set_value(path.to_string(), window, cx));
        }
        let wrap = self.insp.wrap;
        if self.insp.applied_wrap != wrap {
            self.insp.applied_wrap = wrap;
            self.insp
                .editor
                .update(cx, |e, cx| e.set_soft_wrap(wrap, window, cx));
        }

        let theme = cx.theme().clone();
        let tab = self.insp.tab;
        let head = h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("MESSAGE"),
            )
            .when(!sheet, |d| {
                d.child(
                    Button::new("expand-details")
                        .ghost()
                        .xsmall()
                        .icon(if self.insp.expanded {
                            IconName::Minimize
                        } else {
                            IconName::Maximize
                        })
                        .tooltip(if self.insp.expanded {
                            "Restore the details"
                        } else {
                            "Expand the details"
                        })
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.insp.expanded = !v.insp.expanded;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("close-details")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Close the details")
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.details_open = false;
                            cx.notify();
                        })),
                )
            });
        let tabs = TabBar::new("detail-tabs")
            .underline()
            .small()
            .selected_index(usize::from(tab == DetailTab::History))
            .on_click(cx.listener(|v, ix: &usize, window, cx| {
                v.insp.tab = if *ix == 0 {
                    DetailTab::Value
                } else {
                    v.insp.hist_focus.focus(window, cx);
                    DetailTab::History
                };
                cx.notify();
            }))
            .child("Value")
            .child("History");

        let topic = self.selected_topic();
        let body = match &shown {
            None => div()
                .text_color(theme.muted_foreground)
                .child(if topic.is_some() {
                    "This topic has no stored message."
                } else {
                    "Select a topic with a message."
                })
                .into_any_element(),
            Some(s) => match tab {
                DetailTab::Value => self.render_value(s, cx).into_any_element(),
                DetailTab::History => self.render_history(s, cx).into_any_element(),
            },
        };
        let actions = topic.map(|_| self.render_actions(shown.as_ref(), cx));
        let width = if self.insp.expanded {
            (self.last_size.0 - 260.).max(DETAIL_WIDTH) * 0.75
        } else {
            DETAIL_WIDTH
        };
        v_flex()
            .when(!sheet, |d| {
                d.w(px(width))
                    .h_full()
                    .border_l_1()
                    .border_color(theme.border)
            })
            .when(sheet, |d| d.size_full())
            .p_3()
            .gap_2()
            .child(head)
            .child(tabs)
            .child(div().flex_1().min_h_0().child(body))
            .children(actions)
            .into_any_element()
    }

    fn render_value(&mut self, s: &Shown, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let m = &s.msg;
        let meta = format!(
            "{}{} · {}",
            if s.direction == Direction::Sent {
                "↑ Sent · "
            } else {
                ""
            },
            format_time(m.received_at, self.utc_offset, true),
            meta_line(
                m.qos,
                m.retain,
                m.original_size,
                m.payload.len(),
                m.truncated
            ),
        );
        let rows = props_rows(&m.props);
        let choice = self.insp.choice;
        let stored = m.payload.len();
        let (running, blank, error) = match &self.insp.view {
            PayloadView::Idle => (true, true, None),
            PayloadView::Running { blank } => (true, *blank, None),
            PayloadView::Done { error, .. } => (false, false, error.clone()),
        };
        let show_all = self.can_show_all(Some(s));
        let can_copy = self.can_copy(Some(s));
        let more = self.can_show_more(Some(s)).then(|| {
            (
                self.insp.shown_len,
                self.insp.full.len().min(self.insp.shown_len + MORE_BYTES),
                self.insp.full.len(),
            )
        });
        let can_hex = error.is_some() && choice.pick != DecoderPick::Hex;

        let toolbar = h_flex()
            .gap_2()
            .items_center()
            .child(
                div().w(px(96.)).child(
                    Select::new(&self.insp.decoder_select)
                        .small()
                        .accessibility_label("Decoder"),
                ),
            )
            .when(choice.pick == DecoderPick::Number, |d| {
                d.child(
                    div().w(px(96.)).child(
                        Select::new(&self.insp.kind_select)
                            .small()
                            .accessibility_label("Number type"),
                    ),
                )
                .child(
                    div().w(px(80.)).child(
                        TabBar::new("endian")
                            .segmented()
                            .small()
                            .selected_index(usize::from(choice.endian == Endian::Big))
                            .on_click(cx.listener(|v, ix: &usize, _, cx| {
                                v.insp.choice.endian = if *ix == 0 {
                                    Endian::Little
                                } else {
                                    Endian::Big
                                };
                                cx.notify();
                            }))
                            .child("LE")
                            .child("BE"),
                    ),
                )
            })
            .child(div().flex_1())
            .child(
                Switch::new("wrap")
                    .checked(self.insp.wrap)
                    .label("Wrap")
                    .on_click(cx.listener(|v, on: &bool, _, cx| v.set_wrap(*on, cx))),
            );

        let payload = if blank && running {
            div()
                .flex_1()
                .text_color(muted)
                .child("Formatting…")
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h(px(120.))
                .child(
                    Editor::new(&self.insp.editor)
                        .h_full()
                        .bordered(true)
                        // The view shows the message; edits go through the composer.
                        .readonly(true),
                )
                .into_any_element()
        };

        v_flex()
            .size_full()
            .gap_2()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .font_family(theme.mono_font_family.clone())
                            .child(
                                Input::new(&self.insp.path)
                                    .small()
                                    .appearance(false)
                                    .readonly(true)
                                    .aria_label("Topic"),
                            ),
                    )
                    .child(
                        Button::new("copy-value")
                            .small()
                            .ghost()
                            .icon(IconName::Copy)
                            .label("Copy")
                            .disabled(!can_copy)
                            .tooltip(if can_copy {
                                "Copy the value shown below"
                            } else {
                                "The value is not ready."
                            })
                            .on_click(cx.listener(|v, _, window, cx| v.copy_value(window, cx))),
                    ),
            )
            .child(toolbar)
            .child(div().text_xs().text_color(muted).child(meta))
            .when(s.evicted, |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("Removed from the message list (memory limit)."),
                )
            })
            .when(!rows.is_empty(), |d| {
                d.child(
                    div().id("props").max_h(px(140.)).overflow_y_scroll().child(
                        // One column: the details pane is narrow, and values are long.
                        DescriptionList::new()
                            .columns(1)
                            .label_width(px(120.))
                            .children(
                                rows.into_iter()
                                    .map(|(k, v)| DescriptionItem::new(k).value(v)),
                            ),
                    ),
                )
            })
            .when_some(error, |d, e| {
                d.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(div().flex_1().child(e))
                        .when(can_hex, |d| {
                            d.child(Button::new("use-hex").xsmall().label("Use Hex").on_click(
                                cx.listener(|v, _, window, cx| {
                                    v.set_decoder(DecoderPick::Hex, window, cx)
                                }),
                            ))
                        }),
                )
            })
            .when(running && !blank, |d| {
                d.child(div().text_xs().text_color(muted).child("Formatting…"))
            })
            .child(payload)
            .when(show_all, |d| {
                d.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("Showing the first {}.", size_text(DEFAULT_LIMIT)))
                        .child(
                            Button::new("show-all")
                                .xsmall()
                                .label(format!("Show all ({})", size_text(stored)))
                                .on_click(cx.listener(|v, _, _, cx| v.show_all(cx))),
                        ),
                )
            })
            .when_some(more, |d, (shown_to, next, total)| {
                d.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(muted)
                        .child(format!(
                            "Showing the first {} of {} of text.",
                            size_text(shown_to),
                            size_text(total)
                        ))
                        .child(
                            Button::new("show-more")
                                .xsmall()
                                .label(format!("Show more (+{})", size_text(next - shown_to)))
                                .on_click(cx.listener(|v, _, window, cx| v.show_more(window, cx))),
                        ),
                )
            })
    }

    fn render_history(&mut self, s: &Shown, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(node) = s.node else {
            return div()
                .text_color(muted)
                .child("This topic has no history.")
                .into_any_element();
        };
        let count = self.store.history(node).count();
        if count == 0 {
            return div()
                .text_color(muted)
                .child("No history for this topic. The store may have removed it (memory limit).")
                .into_any_element();
        }
        // One focus handle for the whole list. Up and Down move the cursor, Enter shows the row.
        // Only the visible rows are built.
        v_flex()
            .id("history")
            .size_full()
            .track_focus(&self.insp.hist_focus)
            .key_context(HIST_CONTEXT)
            .on_action(cx.listener(|v, _: &SelectPrev, _, cx| v.step_history(-1, cx)))
            .on_action(cx.listener(|v, _: &SelectNext, _, cx| v.step_history(1, cx)))
            .on_action(cx.listener(|v, _: &ConfirmRow, _, cx| v.confirm_history(cx)))
            .child(
                uniform_list(
                    "history-rows",
                    count,
                    cx.processor(move |v, range: std::ops::Range<usize>, _window, cx| {
                        let theme = cx.theme().clone();
                        let picked = v.insp.hist_pick.as_ref().map(|m| m.received_at);
                        let cursor = v.insp.hist_cursor;
                        let utc = v.utc_offset;
                        let rows: Vec<_> = v
                            .store
                            .history(node)
                            .skip(range.start)
                            .take(range.len())
                            .map(|m| {
                                (
                                    m.received_at,
                                    m.original_size,
                                    m.qos.as_u8(),
                                    SharedString::from(decode::preview(&m.payload, 120)),
                                )
                            })
                            .collect();
                        rows.into_iter()
                            .map(|(at, size, qos, text)| {
                                v.history_row(
                                    at, size, qos, text, node, picked, cursor, utc, &theme, cx,
                                )
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.insp.hist_scroll)
                .flex_1(),
            )
            .into_any_element()
    }

    /// One History row. Its height is `HIST_ROW_H`, so that the list can be virtual.
    #[allow(clippy::too_many_arguments)]
    fn history_row(
        &self,
        at: SystemTime,
        size: usize,
        qos: u8,
        text: SharedString,
        node: NodeId,
        picked: Option<SystemTime>,
        cursor: Option<SystemTime>,
        utc: i32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = theme.muted_foreground;
        let nanos = at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        v_flex()
            .id(("hist", nanos))
            .h(px(HIST_ROW_H))
            .flex_none()
            .overflow_hidden()
            .px_2()
            .py_1()
            .rounded_sm()
            .border_1()
            .border_color(if cursor == Some(at) {
                theme.ring
            } else {
                gpui::transparent_black()
            })
            .when(picked == Some(at), |d| d.bg(theme.accent))
            .hover(|d| d.bg(theme.muted))
            .on_click(cx.listener(move |v, _, window, cx| {
                v.insp.hist_focus.focus(window, cx);
                v.insp.hist_cursor = Some(at);
                v.pick_history(node, at, cx)
            }))
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .child(format_time(at, utc, true)),
                    )
                    .child(format!("QoS {qos}"))
                    .child(div().flex_1())
                    .child(format!("{size} B")),
            )
            .child(
                div()
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(text),
            )
            .into_any_element()
    }

    /// Up and Down in History: moves the cursor. The row stays in view.
    fn step_history(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some((_, Some(node))) = self.selected_topic() else {
            return;
        };
        let times: Vec<SystemTime> = self.store.history(node).map(|m| m.received_at).collect();
        if times.is_empty() {
            return;
        }
        let from = self
            .insp
            .hist_cursor
            .or(self.insp.hist_pick.as_ref().map(|m| m.received_at));
        let ix = match from.and_then(|t| times.iter().position(|x| *x == t)) {
            Some(i) => i.saturating_add_signed(step).min(times.len() - 1),
            None => 0,
        };
        self.insp.hist_cursor = Some(times[ix]);
        self.insp
            .hist_scroll
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Enter in History: shows the row of the cursor in the Value tab.
    fn confirm_history(&mut self, cx: &mut Context<Self>) {
        if let (Some((_, Some(node))), Some(at)) = (self.selected_topic(), self.insp.hist_cursor) {
            self.pick_history(node, at, cx);
        }
    }

    /// A History row was chosen: the Value tab shows it.
    fn pick_history(&mut self, node: NodeId, at: SystemTime, cx: &mut Context<Self>) {
        if let Some(m) = self.store.history(node).find(|m| m.received_at == at) {
            self.insp.hist_pick = Some(m.clone());
            self.insp.tab = DetailTab::Value;
            cx.notify();
        }
    }

    /// The actions. `shown` is `None` for a topic without a stored message: then only the
    /// actions that need a topic show.
    fn render_actions(&self, shown: Option<&Shown>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let edit_off = shown.and_then(|s| composer::edit_unavailable(&s.msg, s.evicted));
        let reply = shown.is_some_and(|s| {
            s.msg
                .props
                .response_topic
                .as_deref()
                .is_some_and(|t| !t.is_empty())
        });
        let retained = self.has_retained() && self.connected();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("publish-to-topic")
                            .small()
                            .label("Publish to this topic")
                            .on_click(
                                cx.listener(|v, _, window, cx| v.publish_to_topic(window, cx)),
                            ),
                    )
                    .when(shown.is_some(), |d| {
                        d.child(
                            Button::new("edit-and-publish")
                                .small()
                                .label("Edit and publish")
                                .disabled(edit_off.is_some())
                                .on_click(
                                    cx.listener(|v, _, window, cx| v.edit_and_publish(window, cx)),
                                ),
                        )
                    })
                    .when(reply, |d| {
                        d.child(
                            Button::new("reply")
                                .small()
                                .label("Reply")
                                .on_click(cx.listener(|v, _, window, cx| v.reply(window, cx))),
                        )
                    }),
            )
            .when_some(edit_off, |d, why| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(why),
                )
            })
            .when(retained, |d| {
                d.child(div().h(px(1.)).bg(theme.border)).child(
                    Button::new("clear-retained")
                        .small()
                        .danger()
                        .label("Clear retained")
                        .on_click(
                            cx.listener(|v, _, window, cx| v.confirm_clear_retained(window, cx)),
                        ),
                )
            })
    }
}
