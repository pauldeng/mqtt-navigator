//! The details pane (inspector): Value and History tabs, the decoder, the background render
//! job, and the actions that fill the composer. Below 1 100 px the pane opens as a sheet.

use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::description_list::{DescriptionItem, DescriptionList};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::sheet::Sheet;
use gpui_kit::component::{IndexPath, Placement};
use navigator_core::decode::{self, DEFAULT_LIMIT, Endian, NumberKind, Rendered};

use super::*;
use crate::composer::{self, Draft};
use crate::inspector::{
    AUTO_BYTES, DecoderChoice, DecoderPick, FIRST_BYTES, Jobs, MORE_BYTES, NUMBER_KINDS,
    history_note, meta_line, next_cut, next_step, props_count, props_rows, size_text,
};

/// Below this window width the details open as a right sheet.
pub(super) const NARROW: f32 = 1100.;
/// Width of the details sheet (design pixels, see [`ui`]).
const SHEET_WIDTH: f32 = 420.;
/// Height of one History row (design pixels): two lines of small text and the padding.
const HIST_ROW_H: f32 = 48.;

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
pub(super) struct Want {
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
pub(super) enum PayloadView {
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
    /// The wrap that the editor has. A pane that is closed applies the last choice when it opens.
    applied_wrap: bool,
    /// Hash of the text in the payload editor.
    text_hash: Option<u64>,
    show_all: bool,
    /// The whole rendered text. The editor shows its first `shown_len` bytes. The text grows in
    /// steps (see `crate::inspector::next_step`) up to `more_target`.
    pub(super) full: String,
    pub(super) shown_len: usize,
    pub(super) more_target: usize,
    /// The size that Show more asked for. It belongs to the subject: a later text of the same
    /// subject grows to it too.
    pub(super) more_floor: usize,
    /// The size of the next step.
    step: usize,
    pub(super) more_task: Option<Task<()>>,
    jobs: Jobs,
    pub(super) want: Option<Want>,
    /// What `full` was rendered for (`want` of the last finished job).
    done: Option<Want>,
    pub(super) view: PayloadView,
    task: Option<Task<()>>,
    /// A render job runs that nothing replaced.
    pub(super) running: bool,
    pub(super) editor: Entity<EditorState>,
    pub(super) decoder_select: Entity<SelectState<Vec<DecoderItem>>>,
    kind_select: Entity<SelectState<Vec<KindItem>>>,
    /// A History row that the user chose. The Value tab shows it instead of the selection.
    pub(super) hist_pick: Option<(u64, Msg)>,
    /// The History list takes the keys. `hist_cursor` is the row that Up and Down moved to.
    hist_focus: FocusHandle,
    hist_cursor: Option<u64>,
    hist_scroll: UniformListScrollHandle,
    /// The full topic of the header, as a read-only input, so that the text can be selected.
    topic_view: Entity<InputState>,
    topic_text: String,
    /// The escaped metadata line of the Value tab, for the message of this identity.
    meta: Option<(DetailKey, i32, SharedString)>,
    /// The last `Navigator::selection` that the pane saw: a new one drops the History pick.
    selection: u64,
    /// The Properties section of the Value tab is open (collapsed by default).
    props_open: bool,
    /// The Value tab takes the focus when a History row is chosen.
    value_focus: FocusHandle,
    /// The pane. When it closes with the focus inside, the root takes the focus.
    pub(super) pane_focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl Inspector {
    fn new(window: &mut Window, cx: &mut Context<DetailsPane>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(false)
                // The read-only editor shows the active line only while it has the focus.
                .active_line_only_when_focused(true)
                .searchable(false)
                .soft_wrap(true)
        });
        editor.update(cx, |e, cx| e.set_readonly(true, cx));
        let items = DecoderPick::ALL.iter().map(|p| DecoderItem(*p)).collect();
        let decoder_select =
            cx.new(|cx| SelectState::new(items, Some(IndexPath::new(0)), window, cx));
        let kinds: Vec<KindItem> = NUMBER_KINDS.iter().map(|(k, n)| KindItem(*k, n)).collect();
        let kind_select = cx.new(|cx| SelectState::new(kinds, Some(IndexPath::new(4)), window, cx));
        let subs = vec![
            cx.subscribe_in(
                &decoder_select,
                window,
                |v, _, ev: &SelectEvent<Vec<DecoderItem>>, window, cx| {
                    let SelectEvent::Confirm(pick) = ev;
                    if let Some(p) = pick {
                        v.insp.choice.pick = *p;
                        v.changed(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &kind_select,
                window,
                |v, _, ev: &SelectEvent<Vec<KindItem>>, window, cx| {
                    let SelectEvent::Confirm(kind) = ev;
                    if let Some(k) = kind {
                        v.insp.choice.kind = *k;
                        v.changed(window, cx);
                    }
                },
            ),
        ];
        Self {
            tab: DetailTab::Value,
            choice: DecoderChoice::default(),
            wrap: true,
            applied_wrap: true,
            text_hash: None,
            show_all: false,
            full: String::new(),
            shown_len: 0,
            more_target: 0,
            more_floor: 0,
            step: FIRST_BYTES,
            more_task: None,
            jobs: Jobs::default(),
            want: None,
            done: None,
            view: PayloadView::Idle,
            task: None,
            running: false,
            editor,
            decoder_select,
            kind_select,
            hist_pick: None,
            hist_focus: cx.focus_handle(),
            hist_cursor: None,
            hist_scroll: UniformListScrollHandle::new(),
            topic_view: cx.new(|cx| InputState::new(window, cx)),
            topic_text: String::new(),
            meta: None,
            selection: 0,
            props_open: false,
            value_focus: cx.focus_handle(),
            pane_focus: cx.focus_handle(),
            _subs: subs,
        }
    }

    /// The selection is gone (profile switch): forget the shown value.
    pub(super) fn reset(&mut self) {
        self.jobs.cancel();
        self.task = None;
        self.running = false;
        self.want = None;
        self.done = None;
        self.view = PayloadView::Idle;
        self.show_all = false;
        self.hist_pick = None;
        self.hist_cursor = None;
        self.text_hash = None;
        self.full = String::new();
        self.shown_len = 0;
        self.more_target = 0;
        self.more_floor = 0;
        self.more_task = None;
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

/// The details pane. Its own view: gpui reuses its last frame until the pane calls `notify`.
/// `Navigator` tells the pane when an input of it changes (see `Navigator::details_sig`). The
/// pane notifies itself for its own state.
pub(super) struct DetailsPane {
    nav: WeakEntity<Navigator>,
    pub(super) insp: Inspector,
    /// Tests only: how often the pane drew.
    #[cfg(test)]
    pub(super) renders: usize,
}

/// What the pane reads from `Navigator` when it draws.
struct Inputs {
    shown: Option<Shown>,
    topic: Option<(String, Option<NodeId>)>,
    /// "Clear retained" is on offer.
    retained: bool,
    /// The details are in the sheet of a narrow window.
    sheet: bool,
    expanded: bool,
    utc: i32,
    /// The result of the selected sent message.
    sent: Option<(String, bool)>,
    /// Identity and size of the base of the Value tab (selection or latest value).
    base_seq: u64,
    base_len: usize,
    base_at: Option<SystemTime>,
}

impl Navigator {
    /// What the Value tab shows now. `pick`: the History row that the user chose.
    pub(super) fn shown_with(&self, pick: Option<&(u64, Msg)>) -> Option<Shown> {
        let hist = |node: Option<NodeId>, topic: String, direction: Direction| {
            let (seq, m) = pick?.clone();
            let key = DetailKey::Hist(node?, seq);
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
                    key: DetailKey::Node(id, n.latest_seq),
                    msg: m,
                    direction: Direction::Received,
                    evicted: false,
                })
            }
        }
    }

    /// What the Value tab shows now.
    pub(super) fn shown(&self, cx: &App) -> Option<Shown> {
        self.shown_with(self.pane.read(cx).insp.hist_pick.as_ref())
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

    /// The details are on screen: the pane of a wide window or the sheet of a narrow one.
    pub(super) fn details_visible(&self) -> bool {
        if self.narrow {
            self.sheet_open
        } else {
            self.details_open
        }
    }

    /// The current result of the selected sent message. `None`: no sent row is selected, a
    /// History row shows, or the row is gone.
    fn sent_status(&self, pick: Option<&(u64, Msg)>) -> Option<SendStatus> {
        if pick.is_some() || self.detail_of != DetailOf::Message {
            return None;
        }
        let s = self.sel_msg.as_ref()?;
        if s.direction != Direction::Sent {
            return None;
        }
        // By row id: a list filter must not hide the result of the selected message.
        self.store.row_send_status(s.id).cloned()
    }

    /// The arrival time that the Value tab compares a History row with: the selected message,
    /// or the latest value of the selected topic.
    fn base_received_at(&self) -> Option<SystemTime> {
        match self.detail_of {
            DetailOf::Nothing => None,
            DetailOf::Message => self.sel_msg.as_ref().map(|s| s.msg.received_at),
            DetailOf::Topic => self
                .store
                .get(self.sel_topic?)
                .and_then(|n| n.latest)
                .map(|m| m.received_at),
        }
    }

    /// Everything the pane reads from here. `pick`: its History row.
    fn pane_inputs(&self, pick: Option<&(u64, Msg)>) -> Inputs {
        // The pick is the base itself when both have one identity. Its History copy can be
        // shorter (the store cuts it).
        let (base_seq, base_len) = match self.detail_of {
            DetailOf::Topic => self
                .sel_topic
                .and_then(|id| self.store.get(id))
                .map_or((0, 0), |n| {
                    (n.latest_seq, n.latest.map_or(0, |m| m.payload.len()))
                }),
            DetailOf::Message => self
                .sel_msg
                .as_ref()
                .map_or((0, 0), |s| (s.seq, s.msg.payload.len())),
            DetailOf::Nothing => (0, 0),
        };
        Inputs {
            shown: self.shown_with(pick),
            topic: self.selected_topic(),
            retained: self.has_retained() && self.connected(),
            sheet: self.narrow,
            expanded: self.expanded,
            utc: self.utc_offset,
            sent: self
                .sent_status(pick)
                .map(|st| (format!("Result: {}", status_text(&st)), status_failed(&st))),
            base_seq,
            base_len,
            base_at: self.base_received_at(),
        }
    }

    /// Changes when the pane must draw again because of an input from here. A merge for another
    /// topic changes nothing. The pane keeps its own state apart (it notifies itself).
    pub(super) fn details_sig(&self, theme: &Theme) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // A closed pane shows nothing: no work for it. Opening it changes the signature.
        if !self.details_visible() {
            false.hash(&mut h);
            return h.finish();
        }
        let node = self.selected_topic().and_then(|(_, n)| n);
        let latest = node.and_then(|n| self.store.get(n)).map(|n| n.latest_seq);
        // A message row can go with the memory limit.
        let row = self
            .sel_msg
            .as_ref()
            .filter(|_| self.detail_of == DetailOf::Message)
            .map(|s| (s.id, self.store.row_evicted(s.id)));
        (
            self.selection,
            self.detail_of as u8,
            self.sel_topic,
            row,
            node,
            latest,
            node.map(|n| self.store.history(n).count()),
            self.has_retained(),
            self.sent_status(None).map(|s| status_text(&s)),
        )
            .hash(&mut h);
        (
            self.details_visible(),
            self.connected(),
            self.narrow,
            self.expanded,
            self.utc_offset,
            self.rem.to_bits(),
            theme.theme_name(),
        )
            .hash(&mut h);
        h.finish()
    }

    /// The facts that the registry needs.
    pub(super) fn facts(&self, cx: &App) -> crate::palette::Facts {
        let pane = self.pane.read(cx);
        let shown = self.shown_with(pane.insp.hist_pick.as_ref());
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
            disconnecting: self.status.state == ConnState::Disconnecting,
            messages_view: self.view == View::Messages,
            filters_active: self.filters_active(),
            paused: self.store.is_paused(),
            follow: self.follow.on,
            two_lines: self.two_lines(),
            details_open: self.details_visible(),
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
            can_copy: pane.can_copy(shown.as_ref()),
            wrap: pane.insp.wrap,
            number: (pane.insp.choice.pick == DecoderPick::Number)
                .then_some((pane.insp.choice.kind, pane.insp.choice.endian)),
            can_show_all: pane.can_show_all(shown.as_ref()),
            can_show_more: pane.can_show_more(shown.as_ref()),
            payload_mode: self.comp.mode,
            theme: Some(self.settings.ui.theme_mode),
            preset: crate::themes::preset_by_id(&self.settings.ui.theme_preset).id,
            memory_mib: self.memory_mib(),
            check_updates: self.settings.ui.check_updates,
            sub_qos: self.show_only_sub().map(|s| s.qos),
            composer_open: self.comp.vis.open,
            sub_removing: self
                .show_only_sub()
                .is_some_and(|s| s.state.removal_accepted()),
            sub_remove_failed: self
                .show_only_sub()
                .is_some_and(|s| matches!(s.state, SubState::RemoveFailed(_))),
            sub_failed: self
                .show_only_sub()
                .is_some_and(|s| matches!(s.state, SubState::Failed(_))),
            v5: self.composer_protocol() == Protocol::V5,
        }
    }
}

impl DetailsPane {
    pub(super) fn new(
        nav: WeakEntity<Navigator>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            nav,
            insp: Inspector::new(window, cx),
            #[cfg(test)]
            renders: 0,
        }
    }

    /// What the Value tab shows now.
    fn shown(&self, cx: &App) -> Option<Shown> {
        let pick = self.insp.hist_pick.as_ref();
        self.nav
            .read_with(cx, |n, _| n.shown_with(pick))
            .ok()
            .flatten()
    }

    /// The finished render belongs to `s` (and to the decoder and limit now chosen).
    fn render_ready(&self, s: &Shown) -> bool {
        self.insp.done
            == Some(Want {
                key: s.key,
                choice: self.insp.choice,
                all: self.insp.show_all,
            })
    }

    /// The text on screen is of the shown subject, the chosen decoder and the chosen limit. It
    /// may be of an older message of the topic: the next render replaces it.
    fn subject_text_ready(&self, s: &Shown) -> bool {
        matches!(self.insp.view, PayloadView::Done { .. })
            && self.insp.want.is_some_and(|w| {
                same_subject(w.key, s.key)
                    && w.choice == self.insp.choice
                    && w.all == self.insp.show_all
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
            self.subject_text_ready(s)
                && !self.insp.show_all
                && s.msg.payload.len() > DEFAULT_LIMIT
                && (truncated_at.is_some() || error.is_some())
        })
    }

    /// "Show more": the rendered text is longer than the part in the editor.
    fn can_show_more(&self, shown: Option<&Shown>) -> bool {
        shown.is_some_and(|s| self.subject_text_ready(s))
            && self.insp.more_task.is_none()
            && self.insp.shown_len < self.insp.full.len()
    }

    /// The selection is gone (profile switch): forget the shown value.
    pub(super) fn reset(&mut self, cx: &mut Context<Self>) {
        self.insp.reset();
        cx.notify();
    }

    /// An input from `Navigator` changed (selection, a new message of the shown topic, the
    /// store, the connection, the layout). `selection` counts the user's selections: a new one
    /// drops the History row. This is the one entry for such a change.
    pub(super) fn sync(&mut self, selection: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.insp.selection != selection {
            self.insp.selection = selection;
            self.insp.hist_pick = None;
        }
        self.changed(window, cx);
    }

    /// The shown subject, the decoder or the limit may have changed: start a job, push the topic
    /// into the header, and draw.
    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Nothing on screen: no job. Becoming visible syncs again.
        let visible = self
            .nav
            .read_with(cx, |n, _| n.details_visible())
            .unwrap_or(false);
        if visible {
            let shown = self.shown(cx);
            self.ensure_render(shown.as_ref(), window, cx);
            // A new wrap setting resets the scroll position: apply the last choice only.
            if self.insp.applied_wrap != self.insp.wrap {
                self.insp.applied_wrap = self.insp.wrap;
                let wrap = self.insp.wrap;
                self.insp
                    .editor
                    .update(cx, |e, cx| e.set_soft_wrap(wrap, window, cx));
            }
        }
        let topic_text = self
            .nav
            .read_with(cx, |n, _| n.selected_topic())
            .ok()
            .flatten()
            .map(|(t, _)| display_safe(&t).into_owned())
            .unwrap_or_default();
        if self.insp.topic_text != topic_text {
            self.insp
                .topic_view
                .update(cx, |i, cx| i.set_value(topic_text.clone(), window, cx));
            self.insp.topic_text = topic_text;
        }
        cx.notify();
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
                self.insp.running = false;
                self.insp.view = PayloadView::Idle;
            }
            return;
        };
        let subject_changed = self.insp.want.is_none_or(|w| !same_subject(w.key, s.key));
        if subject_changed {
            // Metrics only (`MQTT_NAVIGATOR_AUTOSELECT`): a new subject starts with Show all.
            self.insp.show_all = self
                .nav
                .read_with(cx, |n, _| n.autoselected)
                .unwrap_or(false);
            self.insp.more_floor = 0;
        }
        let want = Want {
            key: s.key,
            choice: self.insp.choice,
            all: self.insp.show_all,
        };
        if self.insp.want == Some(want) {
            return;
        }
        // A new message of the shown topic changes only the key.
        let same_job = !subject_changed
            && self
                .insp
                .want
                .is_some_and(|w| w.choice == want.choice && w.all == want.all);
        // A slow job must finish: a new message each time would cancel it, and the text would
        // never change. `render_done` starts a job for the newest one.
        if same_job && self.insp.running {
            return;
        }
        // The finished text stays on screen until the new text is ready. A busy topic would
        // show "Formatting…" for ever otherwise.
        let refresh = same_job && matches!(self.insp.view, PayloadView::Done { .. });
        self.insp.want = Some(want);
        self.insp.running = true;
        let (generation, cancel) = self.insp.jobs.start();
        if !refresh {
            self.insp.view = PayloadView::Running {
                blank: subject_changed || matches!(self.insp.view, PayloadView::Idle),
            };
        }
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
        self.insp.running = false;
        self.insp.done = self.insp.want;
        let Rendered {
            text,
            truncated_at,
            error,
        } = out;
        // A live topic sends the same text again and again: set_value would reset the scroll
        // position and the selection. The same text for the same subject changes nothing.
        let same = !matches!(
            self.insp.view,
            PayloadView::Idle | PayloadView::Running { blank: true }
        ) && self.insp.text_hash == Some(hash);
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
            self.insp.more_target = self
                .insp
                .full
                .len()
                .min(AUTO_BYTES.max(self.insp.more_floor));
            self.insp.more_task = None;
            self.schedule_step(window, cx);
        }
        self.insp.view = PayloadView::Done {
            truncated_at,
            error,
        };
        self.changed(window, cx);
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
        self.changed(window, cx);
    }

    /// Copies the whole rendered text, but only if the finished render is the one of the
    /// current selection (not of another message, and not while it runs).
    pub(super) fn copy_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.shown(cx);
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

    /// Copies the original topic of the selection. The label shows the escaped form.
    pub(super) fn copy_topic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((topic, _)) = self
            .nav
            .read_with(cx, |n, _| n.selected_topic())
            .ok()
            .flatten()
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(topic));
        window.push_notification(Notification::success("Topic copied."), cx);
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

    pub(super) fn set_wrap(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.insp.wrap = on;
        self.changed(window, cx);
    }

    /// Renders all of the payload (in the background). The editor still gets chunks.
    pub(super) fn show_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.insp.show_all = true;
        self.changed(window, cx);
    }

    /// "Show more": the text grows by [`MORE_BYTES`], in steps over the next frames.
    pub(super) fn show_more(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let want = self.insp.shown_len.max(self.insp.more_floor) + MORE_BYTES;
        self.insp.more_floor = self.insp.more_floor.max(want);
        let target = want.min(self.insp.full.len());
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
        self.nav
            .read_with(cx, |n, _| n.metrics.ui_step("grow", to - from, took))
            .ok();
        self.insp.step = next_step(to - from, took.as_secs_f32() * 1000.);
        self.insp.shown_len = to;
        self.schedule_step(window, cx);
        cx.notify();
    }
}

impl Navigator {
    /// "Expand" widens the details pane over the message list. "Restore" returns it to the width
    /// that it had before. The expanded width is not saved.
    pub(super) fn set_expanded(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if on == self.expanded {
            return;
        }
        self.expanded = on;
        let limits = self.pane_limits(window);
        let current = self.panes.read(cx).sizes().get(2).copied();
        let target = if on {
            self.expand_restore = current;
            let room = window.viewport_size().width - limits.left.0;
            (room * 0.75).max(limits.right.0)
        } else {
            self.expand_restore.take().unwrap_or(limits.right.0)
        };
        self.panes
            .update(cx, |s, cx| s.resize_panel(2, target, window, cx));
        cx.notify();
    }

    /// Runs `f` on the pane after the current update. The pane reads `Navigator` then, which is
    /// not possible while this update holds it.
    pub(super) fn in_pane(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut DetailsPane, &mut Window, &mut Context<DetailsPane>) + 'static,
    ) {
        let pane = self.pane.clone();
        window.defer(cx, move |window, cx| {
            pane.update(cx, |p, cx| f(p, window, cx))
        });
    }

    /// The palette entry "Show or hide the details". Every close goes through the shared
    /// close, so the focus returns to the list and `sheet_open` stays right.
    pub(super) fn toggle_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.details_visible() {
            self.close_details_area(window, cx);
        } else {
            self.open_details(window, cx);
        }
    }

    /// The one close for the pane and the sheet. The focus returns to the list.
    fn close_details_area(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.narrow {
            // `close_sheet` does not call the callback of the sheet: clear the flag here.
            self.close_details_sheet(window, cx);
            self.list_focus.focus(window, cx);
            cx.notify();
        } else {
            self.close_details(window, cx);
        }
    }

    /// "Details: Open": the pane in a wide window, the sheet in a narrow one.
    pub(super) fn open_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.narrow {
            self.open_details_sheet(window, cx);
        } else {
            self.details_open = true;
        }
        self.sync_details(cx);
        cx.notify();
    }

    /// The close button and `Esc` in the pane. The focus moves to the list.
    pub(super) fn close_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.details_open {
            self.details_open = false;
            self.list_focus.focus(window, cx);
            cx.notify();
        }
    }

    /// Opens the details as a right sheet. Only one sheet can be open.
    pub(super) fn open_details_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sheet_open || window.has_active_sheet(cx) {
            return;
        }
        self.sheet_open = true;
        self.sync_details(cx);
        let pane = self.pane.clone();
        let weak = cx.entity().downgrade();
        window.open_sheet_at(Placement::Right, cx, move |sheet: Sheet, _, _| {
            let weak = weak.clone();
            sheet
                .title("Details")
                .size(ui(SHEET_WIDTH))
                .on_close(move |_, window, cx| {
                    // The focus returns to the list, so it never stays on a closed sheet.
                    weak.update(cx, |v, cx| {
                        v.sheet_open = false;
                        v.list_focus.focus(window, cx);
                    })
                    .ok();
                })
                .child(pane.clone().cached(StyleRefinement::default().size_full()))
        });
    }

    /// The sheet closes before an action fills the composer.
    fn close_details_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sheet_open {
            self.sheet_open = false;
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
        let Some(s) = self.shown(cx) else {
            return;
        };
        if composer::edit_unavailable(&s.msg, s.evicted).is_some() {
            return;
        }
        let mut d = Draft::from_message(&s.topic, &s.msg);
        let notice = self.v3_notice(&s.msg.props);
        self.close_details_sheet(window, cx);
        self.prepare_v3_draft(&mut d, notice, window, cx);
        self.replace_composer(d, true, window, cx);
    }

    pub(super) fn reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.shown(cx) else {
            return;
        };
        let Some(mut d) = Draft::reply(&s.msg, self.comp.qos) else {
            return;
        };
        let notice = self.v3_notice(&s.msg.props);
        self.close_details_sheet(window, cx);
        self.prepare_v3_draft(&mut d, notice, window, cx);
        self.replace_composer(d, true, window, cx);
    }

    /// True if the notice "MQTT 3.1.1 does not send properties." is due: the composer follows an
    /// MQTT 3.1.1 connection, and the source message has an MQTT 5 property.
    fn v3_notice(&self, source: &navigator_core::MsgProps) -> bool {
        composer::v3_drops_properties(self.composer_protocol(), source)
    }

    /// An MQTT 3.1.1 connection sends no properties. The draft loses them.
    fn prepare_v3_draft(
        &mut self,
        d: &mut Draft,
        notice: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composer_protocol() == Protocol::V311 {
            d.drop_properties();
        }
        if notice {
            self.notify(Notification::info(composer::V3_NO_PROPERTIES), window, cx);
        }
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
                .title(format!(
                    "Clear the retained message on {}?",
                    display_safe(&topic)
                ))
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
}

impl DetailsPane {
    // -----------------------------------------------------------------------------------------
    // Render
    // -----------------------------------------------------------------------------------------

    /// The pane. It reads `Navigator` here and changes nothing: the events that cause a change
    /// (`sync`, the handlers below) start the jobs and set the editor values.
    fn render_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let pick = self.insp.hist_pick.as_ref();
        let Ok(inputs) = self.nav.read_with(cx, |n, _| n.pane_inputs(pick)) else {
            return div().into_any_element();
        };
        let sheet = inputs.sheet;
        let shown = &inputs.shown;
        let nav = self.nav.clone();
        let theme = cx.theme().clone();
        let tab = self.insp.tab;
        // The header shows the full topic as selectable text. The input is read-only.
        let topic_empty = self.insp.topic_text.is_empty();
        let head = h_flex()
            .gap_1()
            .items_center()
            .child(div().flex_1().min_w_0().child(if topic_empty {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Details")
                    .into_any_element()
            } else {
                Input::new(&self.insp.topic_view)
                    .small()
                    .readonly(true)
                    .appearance(false)
                    .into_any_element()
            }))
            .child(self.render_details_menu(&inputs, cx))
            .when(!sheet, |d| {
                d.child(
                    Button::new("close-details")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Close the details")
                        .on_click({
                            let nav = nav.clone();
                            move |_, window, cx| {
                                nav.update(cx, |v, cx| v.close_details(window, cx)).ok();
                            }
                        }),
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

        let has_topic = inputs.topic.is_some();
        let body = match shown {
            None => {
                if has_topic {
                    empty_state("No message", "This topic has no stored message.", [])
                } else {
                    empty_state("Nothing selected", "Select a topic with a message.", [])
                }
            }
            Some(s) => match tab {
                DetailTab::Value => self.render_value(s, &inputs, cx).into_any_element(),
                DetailTab::History => self.render_history(s, cx).into_any_element(),
            },
        };
        let actions = shown
            .as_ref()
            .map(|s| self.render_actions(s, cx).into_any_element());
        v_flex()
            .size_full()
            .debug_selector(|| "details-pane".into())
            .when(!sheet, |d| {
                d.track_focus(&self.insp.pane_focus)
                    .key_context("DetailsPane")
                    .on_action(move |_: &CloseDetails, window, cx| {
                        nav.update(cx, |v, cx| v.close_details(window, cx)).ok();
                    })
            })
            .p_3()
            .gap_2()
            .child(head)
            .child(tabs)
            .child(div().flex_1().min_h_0().child(body))
            .children(actions)
            .into_any_element()
    }

    fn render_value(
        &mut self,
        s: &Shown,
        inputs: &Inputs,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let m = &s.msg;
        let sent = s.direction == Direction::Sent;
        // The selected publish keeps its result here, also after the composer shows a newer one.
        let result = inputs.sent.clone();
        // One text for one message: the pane draws again for a click, with the same message.
        let meta = match &self.insp.meta {
            Some((key, utc, text)) if *key == s.key && *utc == inputs.utc => text.clone(),
            _ => {
                let text = SharedString::from(format!(
                    "{} · {}",
                    format_time(m.received_at, inputs.utc, true),
                    meta_line(
                        m.qos,
                        m.retain,
                        m.original_size,
                        m.payload.len(),
                        m.truncated
                    ),
                ));
                self.insp.meta = Some((s.key, inputs.utc, text.clone()));
                text
            }
        };
        let choice = self.insp.choice;
        let stored = m.payload.len();
        let (running, blank, error) = match &self.insp.view {
            PayloadView::Idle => (true, true, None),
            PayloadView::Running { blank } => (true, *blank, None),
            PayloadView::Done { error, .. } => (false, false, error.clone()),
        };
        let show_all = self.can_show_all(Some(s));
        let more = self.can_show_more(Some(s)).then(|| {
            (
                self.insp.shown_len,
                self.insp.full.len().min(self.insp.shown_len + MORE_BYTES),
                self.insp.full.len(),
            )
        });
        let can_hex = error.is_some() && choice.pick != DecoderPick::Hex;

        // The decoder is in the `⋯` menu. The Number decoder shows its two settings here.
        let toolbar =
            h_flex()
                .gap_2()
                .items_center()
                .when(choice.pick == DecoderPick::Number, |d| {
                    d.child(
                        div().w(ui(96.)).child(
                            Select::new(&self.insp.kind_select)
                                .small()
                                .accessibility_label("Number type"),
                        ),
                    )
                    .child(
                        div().w(ui(80.)).child(
                            TabBar::new("endian")
                                .segmented()
                                .small()
                                .selected_index(usize::from(choice.endian == Endian::Big))
                                .on_click(cx.listener(|v, ix: &usize, window, cx| {
                                    v.insp.choice.endian = if *ix == 0 {
                                        Endian::Little
                                    } else {
                                        Endian::Big
                                    };
                                    v.changed(window, cx);
                                }))
                                .child("LE")
                                .child("BE"),
                        ),
                    )
                });

        let payload = if blank && running {
            div()
                .flex_1()
                .text_color(muted)
                .child("Formatting…")
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h(ui(120.))
                .child(
                    Editor::new(&self.insp.editor)
                        .h_full()
                        .bordered(true)
                        // The view shows the message; edits go through the composer.
                        .readonly(true),
                )
                .into_any_element()
        };

        // A History row that differs from the selection: say so, and offer the way back.
        // The pick is the base itself when both have one identity.
        // Its History copy can be shorter (the store cuts it).
        let (base_seq, base_len) = (inputs.base_seq, inputs.base_len);
        let earlier = self.insp.hist_pick.as_ref().and_then(|(q, p)| {
            let at = format_time(p.received_at, inputs.utc, false);
            if *q == base_seq {
                return (p.payload.len() != base_len).then(|| format!("Shortened copy from {at}"));
            }
            Some(
                history_note(p.received_at, inputs.base_at, &at)
                    // Same time as the base, but another message.
                    .unwrap_or_else(|| format!("Another message from {at}")),
            )
        });
        let props_open = self.insp.props_open;
        // A closed section needs the count only: the rows are built when it opens.
        let props_count = props_count(&m.props);
        v_flex()
            .id("value-tab")
            .track_focus(&self.insp.value_focus)
            .size_full()
            .gap_2()
            .when_some(earlier, |d, text| {
                d.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(div().flex_1().child(text))
                        .child(
                            Button::new("show-latest")
                                .debug_selector(|| "show-latest".into())
                                .xsmall()
                                .label("Show latest")
                                .on_click(cx.listener(|v, _, window, cx| {
                                    v.insp.hist_pick = None;
                                    // The button goes away: the focus moves to the Value area.
                                    v.insp.value_focus.focus(window, cx);
                                    v.changed(window, cx);
                                })),
                        ),
                )
            })
            .child(toolbar)
            .when_some(result, |d, (text, failed)| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(if failed { theme.danger } else { muted })
                        .child(text),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .when(sent, |d| {
                        d.child(Icon::new(IconName::ArrowUp).xsmall())
                            .child("Sent ·")
                    })
                    .child(meta),
            )
            .when(s.evicted, |d| {
                d.child(
                    div()
                        .debug_selector(|| "evicted-note".into())
                        .text_xs()
                        .text_color(theme.warning)
                        .child("Removed from the message list (memory limit)."),
                )
            })
            .when_some(
                crate::inspector::no_properties_note(m.protocol, &m.props),
                |d, note| d.child(div().text_xs().text_color(muted).child(note)),
            )
            .when(props_count > 0, |d| {
                d.child(
                    Collapsible::new()
                        .open(props_open)
                        .child(
                            Button::new("props-toggle")
                                .xsmall()
                                .ghost()
                                .icon(if props_open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .label(format!("Properties ({props_count})"))
                                .on_click(cx.listener(|v, _, _, cx| {
                                    v.insp.props_open = !v.insp.props_open;
                                    cx.notify();
                                })),
                        )
                        .content(
                            div().id("props").max_h(ui(140.)).overflow_y_scroll().child(
                                // One column: the details pane is narrow, and values are long.
                                DescriptionList::new()
                                    .columns(1)
                                    .label_width(ui(120.))
                                    .children(
                                        props_open
                                            .then(|| props_rows(&m.props))
                                            .into_iter()
                                            .flatten()
                                            .map(|(k, v)| {
                                                DescriptionItem::new(display_safe(&k).into_owned())
                                                    .value(display_safe(&v).into_owned())
                                            }),
                                    ),
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
                        .flex_wrap()
                        .gap_x_2()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("Showing the first {}.", size_text(DEFAULT_LIMIT)))
                        .child(
                            Button::new("show-all")
                                .xsmall()
                                .label(format!("Show all ({})", size_text(stored)))
                                .on_click(cx.listener(|v, _, window, cx| v.show_all(window, cx))),
                        ),
                )
            })
            .when_some(more, |d, (shown_to, next, total)| {
                d.child(
                    h_flex()
                        .flex_wrap()
                        .gap_x_2()
                        .text_xs()
                        .text_color(muted)
                        .child(format!(
                            "Showing the first {} of {} of text.",
                            size_text(shown_to),
                            size_text(total)
                        ))
                        .child(
                            div().debug_selector(|| "show-more".into()).child(
                                Button::new("show-more")
                                    .xsmall()
                                    .label(format!("Show more (+{})", size_text(next - shown_to)))
                                    .on_click(
                                        cx.listener(|v, _, window, cx| v.show_more(window, cx)),
                                    ),
                            ),
                        ),
                )
            })
    }

    fn render_history(&mut self, s: &Shown, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(node) = s.node else {
            return empty_state("No history", "This topic has no history.", []);
        };
        let count = self
            .nav
            .read_with(cx, |n, _| n.store.history(node).count())
            .unwrap_or(0);
        if count == 0 {
            return empty_state(
                "No history",
                "The store may have removed it (memory limit).",
                [],
            );
        }
        // One focus handle for the whole list. Up and Down move the cursor, Enter shows the row.
        // Only the visible rows are built.
        v_flex()
            .id("history")
            .relative()
            .size_full()
            .track_focus(&self.insp.hist_focus)
            .key_context(HIST_CONTEXT)
            .on_action(cx.listener(|v, _: &SelectPrev, _, cx| v.step_history(-1, cx)))
            .on_action(cx.listener(|v, _: &SelectNext, _, cx| v.step_history(1, cx)))
            .on_action(cx.listener(|v, _: &ConfirmRow, window, cx| v.confirm_history(window, cx)))
            .child(
                uniform_list(
                    "history-rows",
                    count,
                    cx.processor(move |v, range: std::ops::Range<usize>, _window, cx| {
                        let theme = cx.theme().clone();
                        let picked = v.insp.hist_pick.as_ref().map(|(q, _)| *q);
                        let cursor = v.insp.hist_cursor;
                        let (utc, rows): (i32, Vec<_>) = v
                            .nav
                            .read_with(cx, |n, _| {
                                let rows = n
                                    .store
                                    .history_seq(node)
                                    .skip(range.start)
                                    .take(range.len())
                                    .map(|(seq, m)| {
                                        (
                                            seq,
                                            m.received_at,
                                            m.original_size,
                                            m.qos.as_u8(),
                                            SharedString::from(decode::preview(&m.payload, 120)),
                                        )
                                    })
                                    .collect();
                                (n.utc_offset, rows)
                            })
                            .unwrap_or_default();
                        rows.into_iter()
                            .map(|(seq, at, size, qos, text)| {
                                v.history_row(
                                    seq, at, size, qos, text, node, picked, cursor, utc, &theme, cx,
                                )
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.insp.hist_scroll)
                .flex_1(),
            )
            .vertical_scrollbar(&self.insp.hist_scroll)
            .into_any_element()
    }

    /// One History row. Its height is `HIST_ROW_H`, so that the list can be virtual.
    #[allow(clippy::too_many_arguments)]
    fn history_row(
        &self,
        seq: u64,
        at: SystemTime,
        size: usize,
        qos: u8,
        text: SharedString,
        node: NodeId,
        picked: Option<u64>,
        cursor: Option<u64>,
        utc: i32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = theme.muted_foreground;
        let is_picked = picked == Some(seq);
        let tone = move |d: Div| if is_picked { d } else { d.text_color(muted) };
        // The picked row is the selected one. The keyboard cursor is the outline.
        ListItem::new(("hist", seq))
            .debug_selector(move || format!("hist-{seq}"))
            .h(ui(HIST_ROW_H))
            .flex_none()
            .overflow_hidden()
            .rounded_sm()
            .pl_2()
            .pr_3()
            .py_0p5()
            .selected(is_picked)
            .secondary_selected(cursor == Some(seq))
            .on_click(cx.listener(move |v, _, window, cx| {
                v.insp.hist_focus.focus(window, cx);
                v.insp.hist_cursor = Some(seq);
                v.pick_history(node, seq, window, cx)
            }))
            .child(
                v_flex()
                    .w_full()
                    .child(tone(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .child(
                                div()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(format_time(at, utc, true)),
                            )
                            .child(format!("QoS {qos}"))
                            .child(div().flex_1())
                            .child(format!("{size} B")),
                    ))
                    .child(tone(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(text),
                    )),
            )
            .into_any_element()
    }

    /// The node of the selected topic.
    fn selected_node(&self, cx: &App) -> Option<NodeId> {
        self.nav
            .read_with(cx, |n, _| n.selected_topic())
            .ok()
            .flatten()
            .and_then(|(_, node)| node)
    }

    /// Up and Down in History: moves the cursor. The row stays in view.
    fn step_history(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(node) = self.selected_node(cx) else {
            return;
        };
        let seqs: Vec<u64> = self
            .nav
            .read_with(cx, |n, _| {
                n.store.history_seq(node).map(|(q, _)| q).collect()
            })
            .unwrap_or_default();
        if seqs.is_empty() {
            return;
        }
        let from = self
            .insp
            .hist_cursor
            .or(self.insp.hist_pick.as_ref().map(|(q, _)| *q));
        let ix = match from.and_then(|t| seqs.iter().position(|x| *x == t)) {
            Some(i) => i.saturating_add_signed(step).min(seqs.len() - 1),
            None => 0,
        };
        self.insp.hist_cursor = Some(seqs[ix]);
        self.insp
            .hist_scroll
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Enter in History: shows the row of the cursor in the Value tab.
    fn confirm_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(node), Some(seq)) = (self.selected_node(cx), self.insp.hist_cursor) {
            self.pick_history(node, seq, window, cx);
        }
    }

    /// A History row was chosen: the Value tab shows it, and the focus moves to that tab.
    fn pick_history(
        &mut self,
        node: NodeId,
        seq: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = self
            .nav
            .read_with(cx, |n, _| {
                n.store
                    .history_seq(node)
                    .find(|(q, _)| *q == seq)
                    .map(|(_, m)| m.clone())
            })
            .ok()
            .flatten();
        if let Some(m) = found {
            self.insp.hist_pick = Some((seq, m));
            self.insp.tab = DetailTab::Value;
            self.insp.value_focus.focus(window, cx);
            self.changed(window, cx);
        }
    }

    /// The one visible action: Edit and publish. The reason shows when it is off.
    fn render_actions(&self, shown: &Shown, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let edit_off = composer::edit_unavailable(&shown.msg, shown.evicted);
        v_flex()
            .gap_1()
            .child(
                h_flex().child(
                    Button::new("edit-and-publish")
                        .small()
                        .label("Edit and publish")
                        .disabled(edit_off.is_some())
                        .on_click({
                            let nav = self.nav.clone();
                            move |_, window, cx| {
                                nav.update(cx, |v, cx| v.edit_and_publish(window, cx)).ok();
                            }
                        }),
                ),
            )
            .when_some(edit_off, |d, why| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(why),
                )
            })
    }

    /// The `⋯` menu of the header: Copy topic, Copy payload, Publish to this topic, Reply,
    /// Decoder, Wrap, Expand and, last, Clear retained….
    fn render_details_menu(&self, inputs: &Inputs, cx: &mut Context<Self>) -> impl IntoElement {
        let nav = self.nav.clone();
        let pane = cx.entity().downgrade();
        let shown = inputs.shown.as_ref();
        let has_topic = inputs.topic.is_some();
        let has_value = shown.is_some();
        let can_copy = self.can_copy(shown);
        let reply = shown.is_some_and(|s| {
            s.msg
                .props
                .response_topic
                .as_deref()
                .is_some_and(|t| !t.is_empty())
        });
        let (retained, sheet, expanded) = (inputs.retained, inputs.sheet, inputs.expanded);
        let (pick, wrap) = (self.insp.choice.pick, self.insp.wrap);
        Button::new("details-more")
            .ghost()
            .xsmall()
            .icon(IconName::Ellipsis)
            .tab_stop(true)
            .tooltip("Details actions")
            .dropdown_menu(move |menu, window, cx| {
                // An action of the pane and an action of the window.
                let own = |label: &'static str,
                           run: fn(&mut DetailsPane, &mut Window, &mut Context<DetailsPane>)| {
                    let pane = pane.clone();
                    PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        pane.update(cx, |v, cx| run(v, window, cx)).ok();
                    })
                };
                let item = |label: &'static str,
                            run: fn(&mut Navigator, &mut Window, &mut Context<Navigator>)| {
                    let nav = nav.clone();
                    PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        nav.update(cx, |v, cx| run(v, window, cx)).ok();
                    })
                };
                let mut menu = menu
                    .item(own("Copy topic", |v, w, cx| v.copy_topic(w, cx)).disabled(!has_topic))
                    .item(own("Copy payload", |v, w, cx| v.copy_value(w, cx)).disabled(!can_copy))
                    .item(
                        item("Publish to this topic", |v, w, cx| {
                            v.publish_to_topic(w, cx)
                        })
                        .disabled(!has_topic),
                    );
                if reply {
                    menu = menu.item(item("Reply", |v, w, cx| v.reply(w, cx)));
                }
                let p = pane.clone();
                menu = menu
                    .separator()
                    .submenu("Decoder", window, cx, move |m, _, _| {
                        DecoderPick::ALL.iter().fold(m, |m, d| {
                            let (p, d) = (p.clone(), *d);
                            m.item(
                                PopupMenuItem::new(d.label())
                                    .checked(pick == d)
                                    .disabled(!has_value)
                                    .on_click(move |_, window, cx| {
                                        p.update(cx, |v, cx| v.set_decoder(d, window, cx)).ok();
                                    }),
                            )
                        })
                    });
                let p = pane.clone();
                menu = menu.item(PopupMenuItem::new("Wrap").checked(wrap).on_click(
                    move |_, window, cx| {
                        p.update(cx, |v, cx| v.set_wrap(!wrap, window, cx)).ok();
                    },
                ));
                if !sheet {
                    let nav = nav.clone();
                    menu = menu.item(PopupMenuItem::new("Expand").checked(expanded).on_click(
                        move |_, window, cx| {
                            nav.update(cx, |v, cx| v.set_expanded(!expanded, window, cx))
                                .ok();
                        },
                    ));
                }
                if retained {
                    menu = menu.separator().item(item("Clear retained…", |v, w, cx| {
                        v.confirm_clear_retained(w, cx)
                    }));
                }
                menu
            })
    }
}

impl Render for DetailsPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        self.render_pane(cx)
    }
}
