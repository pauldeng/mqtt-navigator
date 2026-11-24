//! The publish composer at the bottom of the centre pane: fields, payload mode, properties,
//! History, validation and Send. The model (drafts, Text/Hex conversion) is in `crate::composer`.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

use gpui_kit::component::InteractiveElementExt as _;
use gpui_kit::component::button::{Toggle, ToggleGroup};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use navigator_core::decode;
use navigator_core::publish::{validate_outgoing, validate_v3_size};

use super::*;
use crate::composer::{
    ComposerModel, Draft, LineKeep, PayloadMode, PropKind, PropPresence, PropShown, Visibility,
    property_count,
};
use crate::inspector::size_text;

/// Height of the payload editor (design pixels, see [`ui`]).
const PAYLOAD_HEIGHT: f32 = 84.;
/// Properties section: the width of the label column, and the width of the column at the end of a
/// row ("Send empty" and Remove).
const LABEL_WIDTH: f32 = 160.;
const END_WIDTH: f32 = 150.;
/// A centre pane narrower than this stacks the label above the field: the label column, the end
/// column and two inputs of a user property need about this much width.
const STACK_BELOW: f32 = 560.;
/// Two limits for the maximum height of the composer (see [`composer_max_height`]): a part of the
/// window height, and a floor in design pixels.
const MAX_HEIGHT_PART: f32 = 0.55;
const MAX_HEIGHT_FLOOR: f32 = 280.;

/// Whether a property row puts its label above the field, for a composer of width `width`.
/// Before the first layout, the width is unknown (`None`): the rows stay in one line.
fn props_stacked(width: Option<Pixels>, rem: Pixels) -> bool {
    width.is_some_and(|w| w < ui(STACK_BELOW).to_pixels(rem))
}

/// Rows that the Properties area builds: the rows near the viewport `[top, top + view]` of the
/// scroll content (a margin of two rows on each side), the first and the last row, the rows with
/// the focus and two rows on each side of them (so that Tab and Shift+Tab find their next stop),
/// and every row without an extent. `extents` has the top and the height of each row from its
/// last draw. The first and the last row are always built, so their tops are current. The gap
/// between rows comes from them (the display scale rounds it), and the tops of the rows between
/// come from their heights and the gap.
fn rows_in_view(
    extents: &[Option<(Pixels, Pixels)>],
    focused: &[bool],
    top: Pixels,
    view: Pixels,
) -> Vec<bool> {
    const MARGIN_ROWS: f32 = 2.;
    let n = extents.len();
    let all: Option<Vec<(Pixels, Pixels)>> = extents.iter().copied().collect();
    let (Some(rows), true, true) = (all, n > 1, view > px(0.)) else {
        return vec![true; n];
    };
    let heights: Pixels = rows[..n - 1].iter().map(|r| r.1).sum();
    let gap = (rows[n - 1].0 - rows[0].0 - heights) / (n - 1) as f32;
    let margin = (rows[0].1 + gap) * MARGIN_ROWS;
    let mut y = rows[0].0;
    let near_focus = |i: usize| (i.saturating_sub(2)..=(i + 2).min(n - 1)).any(|k| focused[k]);
    rows.iter()
        .enumerate()
        .map(|(i, &(_, h))| {
            let at = y;
            y += h + gap;
            i == 0
                || i == n - 1
                || near_focus(i)
                || (at + h >= top - margin && at <= top + view + margin)
        })
        .collect()
}

/// The maximum height of the composer in a window of height `window`. First take 55% of the
/// window. If that is less than the floor of 280 design pixels, take the floor: the fixed rows
/// (topic, payload, action row) then keep their room at a large zoom. At the end, take at most 80%
/// of the window. The message list keeps the rest.
fn composer_max_height(window: Pixels, rem: Pixels) -> Pixels {
    (window * MAX_HEIGHT_PART)
        .max(ui(MAX_HEIGHT_FLOOR).to_pixels(rem))
        .min(window * 0.8)
}

/// Format refuses a payload above this size, and a result above [`FORMAT_MAX_OUT`]: setting a
/// large text into the editor runs on the UI thread (see docs/ui-design.md, 3.3).
const FORMAT_MAX_IN: usize = 1024 * 1024;
const FORMAT_MAX_OUT: usize = 2 * 1024 * 1024;

/// A single-line input and the exact string that it stands for (see [`LineKeep`]).
struct Field {
    input: Entity<InputState>,
    keep: LineKeep,
}

impl Field {
    fn new(input: Entity<InputState>) -> Self {
        Self {
            input,
            keep: LineKeep::default(),
        }
    }

    /// The exact value: the kept string with line breaks, or the text of the input.
    fn get(&self, cx: &App) -> String {
        self.keep.value(&self.input.read(cx).value())
    }

    fn set(&mut self, text: &str, window: &mut Window, cx: &mut App) {
        let shown = self.keep.set(text);
        self.input
            .update(cx, |i, cx| i.set_value(shown, window, cx));
    }

    /// The note under the field, if the exact string has line breaks or hidden characters.
    fn note(&self, what: &str, cx: &App) -> Option<String> {
        self.keep.note(what, &self.input.read(cx).value())
    }
}

/// The place of a row in the Properties scroll area, and the focus handle that the row element
/// tracks. A focus anywhere in the row (input, switch, Remove) reveals it.
struct Spot {
    /// Where the row was drawn last (window coordinates).
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The top of the row in the scroll content, and its height, from the last draw. A row that
    /// the window skips (see [`rows_to_draw`]) keeps its last value.
    extent: Rc<Cell<Option<(Pixels, Pixels)>>>,
    focus: FocusHandle,
    /// The "Send empty" switch (in a wrapper that is not a tab stop) and the Remove button. A
    /// move from one to the other stays inside the row, so the row handle sees no focus-in.
    switch: FocusHandle,
    remove: FocusHandle,
}

impl Spot {
    /// The "Send empty" switch in a wrapper that tracks the switch handle.
    fn switch_div(&self, switch: Switch) -> Div {
        div().track_focus(&self.switch).child(switch)
    }

    fn new(cx: &mut Context<Navigator>) -> Self {
        Self {
            bounds: Rc::default(),
            extent: Rc::default(),
            focus: cx.focus_handle(),
            switch: cx.focus_handle(),
            remove: cx.focus_handle().tab_stop(true),
        }
    }
}

/// The row element of a [`Spot`]: it tracks the focus and measures itself.
fn spot_div(spot: &Spot, scroll: &ScrollHandle, child: impl IntoElement) -> Div {
    let (place, extent, scroll) = (spot.bounds.clone(), spot.extent.clone(), scroll.clone());
    div()
        .relative()
        .track_focus(&spot.focus)
        .child(
            canvas(
                move |b, _, _| {
                    place.set(Some(b));
                    let content_top = scroll.bounds().top() + scroll.offset().y;
                    extent.set(Some((b.top() - content_top, b.size.height)));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .child(child)
}

/// One row of user properties: two inputs and a remove button. A row with two empty inputs is
/// unused, unless `keep_empty` is on: then the empty pair is sent.
struct UserRow {
    id: u64,
    key: Field,
    value: Field,
    keep_empty: bool,
    spot: Spot,
    _subs: Vec<Subscription>,
}

pub(super) struct Composer {
    topic: Field,
    payload: Entity<EditorState>,
    content_type: Field,
    /// Send the content type also when it is empty.
    content_type_empty: bool,
    response_topic: Field,
    correlation: Entity<InputState>,
    /// Send the correlation data also when it is empty.
    correlation_empty: bool,
    expiry: Entity<InputState>,
    rows: Vec<UserRow>,
    next_row: u64,
    /// The rows of the standard properties, in the order of [`PropKind`] (without `User`).
    std_spots: [Spot; 5],
    /// The size of the composer at the last layout. Its width sets the stacking of the property
    /// rows. A change of the size shows the row with the focus again.
    size: Rc<Cell<Option<Size<Pixels>>>>,
    /// The `⋯` button and the "Add property" button. Escape in their menus returns the focus here.
    more_focus: FocusHandle,
    add_focus: FocusHandle,
    /// Wait two frames after the composer size, the stacking or the height of the Properties
    /// area changes. Then show the row with the focus.
    settle: Rc<Cell<u8>>,
    /// The height of the Properties area at the last layout.
    view_height: Rc<Cell<Pixels>>,
    /// The scroll position of the Properties area (the standard fields and the user rows).
    props_scroll: ScrollHandle,
    /// The row that had the focus last (it stays built, see `rows_to_draw`).
    focus_row: Cell<Option<u64>>,
    /// Build every user row in the next frame (see `render_props`).
    draw_all: Cell<bool>,
    /// The stacking, the rem size and the width that the row heights (`Spot::extent`) belong to.
    rows_key: Cell<(bool, Pixels, Option<Pixels>)>,
    /// The window height of the last frame. The scroll area has the old size until the next layout.
    window_h: Cell<Pixels>,
    #[cfg(all(test, debug_assertions))]
    resized: Cell<bool>,
    /// For the window tests: how many user rows the last frame built.
    #[cfg(all(test, debug_assertions))]
    drawn_rows: Cell<usize>,
    /// For the window tests: the rows built in the last frame that had a new window height.
    #[cfg(all(test, debug_assertions))]
    resized_rows: Cell<usize>,
    pub(super) mode: PayloadMode,
    pub(super) qos: Qos,
    retain: bool,
    utf8: bool,
    /// Whether the composer and its Properties section show. Closed by default.
    pub(super) vis: Visibility,
    /// Which property fields show (separate from their text).
    shown: PropShown,
    pub(super) model: ComposerModel,
    /// Why the last Send or conversion did not work. It shows under the composer.
    pub(super) error: Option<String>,
    format_task: Option<Task<()>>,
    /// JSON mode: why the payload is not valid JSON. A warning only: Publish still works.
    json_warning: Option<String>,
    /// JSON mode: the payload is valid and has JSON highlighting.
    json_lang: bool,
    json_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl Composer {
    /// The row element of a standard property.
    fn std_spot(&self, kind: PropKind, child: impl IntoElement) -> Div {
        spot_div(&self.std_spots[kind as usize], &self.props_scroll, child)
    }

    pub(super) fn new(window: &mut Window, cx: &mut Context<Navigator>) -> Self {
        let input = |window: &mut Window, cx: &mut Context<Navigator>, hint: &'static str| {
            cx.new(|cx| InputState::new(window, cx).placeholder(hint))
        };
        let topic = input(window, cx, "Topic");
        // The name of each property field is its label (see `render_props`). The hint is an example.
        let content_type = input(window, cx, "For example application/json");
        let response_topic = input(window, cx, "Topic for the reply");
        let correlation = input(window, cx, "Hex bytes, for example 0a1b");
        let expiry = input(window, cx, "Seconds");
        let payload = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(false)
                // No fold column: the payload text starts at the same edge as the topic.
                .folding(false)
                .searchable(false)
                .soft_wrap(true)
                .placeholder("Payload")
        });
        let mut subs: Vec<Subscription> = [
            &topic,
            &content_type,
            &response_topic,
            &correlation,
            &expiry,
        ]
        .into_iter()
        .map(|i| {
            cx.subscribe(i, |v, _, ev: &InputEvent, _| {
                if matches!(ev, InputEvent::Change) {
                    v.comp.model.edited();
                }
            })
        })
        .collect();
        subs.push(cx.subscribe(&payload, |v, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                v.comp.model.edited();
                v.check_json(cx);
            }
        }));
        let std_spots: [Spot; 5] = std::array::from_fn(|_| Spot::new(cx));
        for kind in [
            PropKind::PayloadFormat,
            PropKind::ContentType,
            PropKind::ResponseTopic,
            PropKind::Correlation,
            PropKind::Expiry,
        ] {
            let spot = &std_spots[kind as usize];
            for handle in [&spot.focus, &spot.switch, &spot.remove] {
                subs.push(cx.on_focus_in(handle, window, move |v, _, cx| v.reveal_std(kind, cx)));
            }
        }
        // A move inside a standard row (from Remove to the input) reveals the row too.
        for (kind, input) in [
            (PropKind::ContentType, &content_type),
            (PropKind::ResponseTopic, &response_topic),
            (PropKind::Correlation, &correlation),
            (PropKind::Expiry, &expiry),
        ] {
            subs.push(cx.subscribe(input, move |v, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Focus) {
                    v.reveal_std(kind, cx);
                }
            }));
        }
        Self {
            topic: Field::new(topic),
            payload,
            content_type: Field::new(content_type),
            content_type_empty: false,
            response_topic: Field::new(response_topic),
            correlation,
            correlation_empty: false,
            expiry,
            rows: Vec::new(),
            next_row: 1,
            std_spots,
            size: Rc::default(),
            more_focus: cx.focus_handle().tab_stop(true),
            add_focus: cx.focus_handle().tab_stop(true),
            settle: Rc::default(),
            view_height: Rc::default(),
            props_scroll: ScrollHandle::new(),
            focus_row: Cell::new(None),
            draw_all: Cell::new(false),
            rows_key: Cell::new((false, px(0.), None)),
            window_h: Cell::new(px(0.)),
            #[cfg(all(test, debug_assertions))]
            resized: Cell::new(false),
            #[cfg(all(test, debug_assertions))]
            drawn_rows: Cell::new(0),
            #[cfg(all(test, debug_assertions))]
            resized_rows: Cell::new(0),
            mode: PayloadMode::Text,
            qos: Qos::AtMostOnce,
            retain: false,
            utf8: false,
            vis: Visibility::default(),
            shown: PropShown::default(),
            model: ComposerModel::default(),
            error: None,
            format_task: None,
            json_warning: None,
            json_lang: false,
            json_task: None,
            _subs: subs,
        }
    }

    /// The profile changed: the composer and History start empty.
    pub(super) fn reset(&mut self, window: &mut Window, cx: &mut App) {
        self.model.clear();
        self.error = None;
        self.format_task = None;
        self.json_warning = None;
        self.json_task = None;
        if self.json_lang {
            self.json_lang = false;
            self.payload
                .update(cx, |e, cx| e.set_highlighter("text", cx));
        }
        self.mode = PayloadMode::Text;
        self.qos = Qos::AtMostOnce;
        self.retain = false;
        self.utf8 = false;
        self.rows.clear();
        self.vis = Visibility::default();
        self.shown = PropShown::default();
        self.content_type_empty = false;
        self.correlation_empty = false;
        for f in [
            &mut self.topic,
            &mut self.content_type,
            &mut self.response_topic,
        ] {
            f.set("", window, cx);
        }
        for i in [&self.correlation, &self.expiry] {
            i.update(cx, |i, cx| i.set_value("", window, cx));
        }
        self.payload.update(cx, |e, cx| e.set_value("", window, cx));
    }
}

impl Navigator {
    /// What the composer holds now.
    pub(super) fn read_draft(&self, cx: &App) -> Draft {
        let c = &self.comp;
        let text = |i: &Entity<InputState>| i.read(cx).value().to_string();
        // A field with text is present. An empty field is present only if the user said so.
        let present = |t: String, empty: bool| (!t.is_empty() || empty).then_some(t);
        Draft {
            topic: c.topic.get(cx),
            mode: c.mode,
            payload: c.payload.read(cx).value().to_string(),
            qos: c.qos,
            retain: c.retain,
            content_type: present(c.content_type.get(cx), c.content_type_empty),
            response_topic: c.response_topic.get(cx),
            correlation: present(text(&c.correlation), c.correlation_empty),
            expiry: text(&c.expiry),
            utf8: c.utf8,
            user: c
                .rows
                .iter()
                .map(|r| (r.key.get(cx), r.value.get(cx), r.keep_empty))
                .filter(|(k, v, keep)| !k.is_empty() || !v.is_empty() || *keep)
                .map(|(k, v, _)| (k, v))
                .collect(),
        }
    }

    fn new_row(
        &mut self,
        key: &str,
        value: &str,
        keep_empty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> UserRow {
        let mk = |window: &mut Window, cx: &mut Context<Self>, hint: &'static str, text: &str| {
            let e = cx.new(|cx| InputState::new(window, cx).placeholder(hint));
            let mut f = Field::new(e);
            f.set(text, window, cx);
            f
        };
        let (k, v) = (mk(window, cx, "Name", key), mk(window, cx, "Value", value));
        let id = self.comp.next_row;
        self.comp.next_row += 1;
        let spot = Spot::new(cx);
        // A focus that enters the row reveals it. An input also reveals the row when the focus
        // moves inside the row (for example from Name to Value, after the user scrolled away).
        let mut subs: Vec<Subscription> = [&k.input, &v.input]
            .into_iter()
            .map(|i| {
                cx.subscribe(i, move |v, _, ev: &InputEvent, cx| match ev {
                    InputEvent::Change => v.comp.model.edited(),
                    InputEvent::Focus => v.reveal_user_row_id(id, cx),
                    _ => {}
                })
            })
            .collect();
        for handle in [&spot.focus, &spot.switch, &spot.remove] {
            subs.push(cx.on_focus_in(handle, window, move |v, _, cx| v.reveal_user_row_id(id, cx)));
        }
        UserRow {
            id,
            spot,
            key: k,
            value: v,
            // A pair that comes from a draft or a message is sent also when it is empty.
            keep_empty: key.is_empty() && value.is_empty() && keep_empty,
            _subs: subs,
        }
    }

    /// Scrolls the Properties area the least that shows the user row `id`. A row that was never
    /// drawn has no place: the caller scrolls.
    fn reveal_user_row_id(&mut self, id: u64, cx: &mut Context<Self>) {
        let b = self
            .comp
            .rows
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.spot.bounds.take());
        self.reveal(b, cx);
    }

    /// The same for a standard property field.
    fn reveal_std(&mut self, kind: PropKind, cx: &mut Context<Self>) {
        let b = self.comp.std_spots[kind as usize].bounds.take();
        self.reveal(b, cx);
    }

    /// The same for the row that has the focus, if any.
    fn reveal_focused_row(&mut self, window: &Window, cx: &mut Context<Self>) {
        let b = self
            .comp
            .rows
            .iter()
            .map(|r| &r.spot)
            .chain(self.comp.std_spots.iter())
            .find(|s| s.focus.contains_focused(window, cx))
            .and_then(|s| s.bounds.take());
        self.reveal(b, cx);
    }

    /// Scrolls the Properties area the least that shows `b`. Tab can focus a row that the area
    /// hides. Each caller reads and clears the saved bounds of the row. After a scroll the bounds
    /// are old until the next frame, so a second reveal in the same frame (the row and its input
    /// both report the focus) must not scroll again.
    fn reveal(&mut self, b: Option<Bounds<Pixels>>, cx: &mut Context<Self>) {
        let Some(b) = b else {
            return;
        };
        let scroll = &self.comp.props_scroll;
        let (view, at) = (scroll.bounds(), scroll.offset());
        let mut y = at.y;
        if b.top() < view.top() {
            y += view.top() - b.top();
        } else if b.bottom() > view.bottom() {
            y -= b.bottom() - view.bottom();
        }
        let y = y.clamp(-scroll.max_offset().y, px(0.));
        if y != at.y {
            scroll.set_offset(point(at.x, y));
            cx.notify();
        }
    }

    /// For the window tests: how many user rows the last frame built.
    #[cfg(all(test, debug_assertions))]
    pub(super) fn drawn_rows(&self) -> usize {
        self.comp.drawn_rows.get()
    }

    #[cfg(all(test, debug_assertions))]
    pub(super) fn resized_rows(&self) -> usize {
        self.comp.resized_rows.get()
    }

    /// For the window tests: the scroll offset of the user rows.
    #[cfg(all(test, debug_assertions))]
    pub(super) fn props_offset(&self) -> Pixels {
        self.comp.props_scroll.offset().y
    }

    #[cfg(all(test, debug_assertions))]
    pub(super) fn last_user_row_focus(&self) -> FocusHandle {
        self.comp
            .rows
            .last()
            .expect("a user row")
            .spot
            .focus
            .clone()
    }

    #[cfg(all(test, debug_assertions))]
    pub(super) fn last_user_value(&self) -> Entity<InputState> {
        self.comp
            .rows
            .last()
            .expect("a user row")
            .value
            .input
            .clone()
    }

    pub(super) fn scroll_props_to_top(&mut self) {
        let at = self.comp.props_scroll.offset();
        self.comp.props_scroll.set_offset(point(at.x, px(0.)));
    }

    #[cfg(all(test, debug_assertions))]
    pub(super) fn user_row_focus(&self, i: usize) -> FocusHandle {
        self.comp.rows[i].spot.focus.clone()
    }

    #[cfg(all(test, debug_assertions))]
    pub(super) fn first_user_row_focus(&self) -> FocusHandle {
        self.comp.rows[0].spot.focus.clone()
    }

    /// For the window tests: the height of the last user row at the last layout.
    #[cfg(all(test, debug_assertions))]
    pub(super) fn last_user_row_height(&self) -> Pixels {
        let row = self.comp.rows.last().expect("a user row");
        row.spot
            .bounds
            .get()
            .expect("the row was drawn")
            .size
            .height
    }

    /// For the window tests: the Remove button of the last user row.
    #[cfg(all(test, debug_assertions))]
    pub(super) fn last_user_remove_focus(&self) -> FocusHandle {
        self.comp
            .rows
            .last()
            .expect("a user row")
            .spot
            .remove
            .clone()
    }

    /// For the window tests: the last user row is inside the scroll viewport.
    #[cfg(all(test, debug_assertions))]
    pub(super) fn last_user_row_in_view(&self) -> bool {
        let row = self.comp.rows.last().expect("a user row");
        let (Some(b), view) = (row.spot.bounds.get(), self.comp.props_scroll.bounds()) else {
            return false;
        };
        b.top() >= view.top() && b.bottom() <= view.bottom()
    }

    /// For the window tests: sets the result line (an error) of the composer.
    #[cfg(all(test, debug_assertions))]
    pub(super) fn set_composer_error(&mut self, text: &str, cx: &mut Context<Self>) {
        self.comp.error = Some(text.into());
        cx.notify();
    }

    /// For the window tests: the focus handles of the `⋯` button and of "Add property".
    #[cfg(all(test, debug_assertions))]
    pub(super) fn menu_buttons_focus(&self) -> [FocusHandle; 2] {
        [self.comp.more_focus.clone(), self.comp.add_focus.clone()]
    }

    /// For the window tests: whether the property rows are stacked now.
    #[cfg(all(test, debug_assertions))]
    pub(super) fn rows_stacked(&self, rem: Pixels) -> bool {
        props_stacked(self.comp.size.get().map(|s| s.width), rem)
    }

    /// Writes a draft into the composer fields. It does not touch History.
    fn apply_draft(&mut self, d: &Draft, window: &mut Window, cx: &mut Context<Self>) {
        // A new set of rows starts at the top.
        self.comp.props_scroll.set_offset(point(px(0.), px(0.)));
        self.comp.topic.set(&d.topic, window, cx);
        self.comp
            .content_type
            .set(d.content_type.as_deref().unwrap_or(""), window, cx);
        self.comp.content_type_empty = d.content_type.as_deref() == Some("");
        self.comp.response_topic.set(&d.response_topic, window, cx);
        let corr = d.correlation.clone().unwrap_or_default();
        self.comp.correlation_empty = d.correlation.as_deref() == Some("");
        self.comp
            .correlation
            .update(cx, |i, cx| i.set_value(corr, window, cx));
        self.comp
            .expiry
            .update(cx, |i, cx| i.set_value(d.expiry.clone(), window, cx));
        self.comp
            .payload
            .update(cx, |e, cx| e.set_value(d.payload.clone(), window, cx));
        self.comp.mode = d.mode;
        self.set_payload_hint(window, cx);
        self.comp.qos = d.qos;
        self.comp.retain = d.retain;
        self.comp.utf8 = d.utf8;
        let rows: Vec<UserRow> = d
            .user
            .iter()
            .map(|(k, v)| self.new_row(k, v, true, window, cx))
            .collect();
        self.comp.rows = rows;
        // A recall, an Edit or a Reply opens Properties when it fills a field.
        self.comp.shown = PropShown::of_draft(d);
        self.comp.vis.props |= property_count(d) > 0;
    }

    /// JSON mode: checks the payload in the background. Valid JSON gets highlighting. Invalid
    /// JSON gets a warning, and Publish still sends it. A result for text that changed since is
    /// dropped. Other modes: no warning and no highlighting.
    fn check_json(&mut self, cx: &mut Context<Self>) {
        if self.comp.mode != PayloadMode::Json {
            self.comp.json_task = None;
            self.comp.json_warning = None;
            self.set_json_lang(false, cx);
            return;
        }
        let snapshot = self.comp.payload.read(cx).value();
        let text = snapshot.clone();
        self.comp.json_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { decode::json_valid(text.as_bytes()) })
                .await;
            this.update(cx, |v, cx| {
                if v.comp.mode != PayloadMode::Json || v.comp.payload.read(cx).value() != snapshot {
                    return;
                }
                v.comp.json_warning = result.as_ref().err().cloned();
                v.set_json_lang(result.is_ok(), cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Changes the highlighting only when it changes, so typing keeps a stable editor.
    fn set_json_lang(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.comp.json_lang != on {
            self.comp.json_lang = on;
            let lang = if on { "json" } else { "text" };
            self.comp
                .payload
                .update(cx, |e, cx| e.set_highlighter(lang, cx));
        }
    }

    fn set_payload_hint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.check_json(cx);
        let hint = match self.comp.mode {
            PayloadMode::Text => "Payload",
            PayloadMode::Hex => "Payload as hex bytes, for example 01 ff 7a",
            PayloadMode::Json => "Payload as JSON",
        };
        self.comp
            .payload
            .update(cx, |e, cx| e.set_placeholder(hint, window, cx));
    }

    /// Saves the unsent draft that the composer has now, before anything replaces it. Recall,
    /// Edit, Reply, Publish to this topic, Format and the Text/Hex switch all call this.
    fn save_before_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.read_draft(cx);
        if !self.comp.model.before_replace(current) {
            window.push_notification(
                Notification::warning("The old draft is larger than 1 MiB. It is not kept."),
                cx,
            );
        }
    }

    /// EVERY replacement of the composer content goes through here: the current draft is saved
    /// first, if it is unsent and has content. Then the composer gets `d` and the focus moves to
    /// the payload.
    pub(super) fn replace_composer(
        &mut self,
        d: Draft,
        unsent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_before_replace(window, cx);
        self.apply_draft(&d, window, cx);
        self.comp.model.loaded(unsent);
        self.comp.error = None;
        self.comp.vis.open();
        self.comp.payload.update(cx, |e, cx| e.focus(window, cx));
        cx.notify();
    }

    /// Fills the composer from an entry of History. A draft leaves Drafts.
    fn recall(&mut self, seq: u64, draft: bool, window: &mut Window, cx: &mut Context<Self>) {
        let h = &mut self.comp.model.history;
        let d = if draft {
            h.take_draft(seq)
        } else {
            h.sent_draft(seq)
        };
        if let Some(d) = d {
            self.replace_composer(d, draft, window, cx);
        }
    }

    pub(super) fn set_payload_mode(
        &mut self,
        to: PayloadMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut d = self.read_draft(cx);
        let old = d.clone();
        match d.switch_mode(to) {
            Ok(()) => {
                if old.payload != d.payload {
                    // The unsent original stays in Drafts (Text and Hex are not the same text).
                    if !self.comp.model.replace_with(old, self.comp.model.unsent) {
                        window.push_notification(
                            Notification::warning(
                                "The old draft is larger than 1 MiB. It is not kept.",
                            ),
                            cx,
                        );
                    }
                    self.comp
                        .payload
                        .update(cx, |e, cx| e.set_value(d.payload, window, cx));
                }
                self.comp.mode = to;
                self.comp.error = None;
                self.set_payload_hint(window, cx);
            }
            Err(e) => self.comp.error = Some(e),
        }
        cx.notify();
    }

    /// New message (`Ctrl/Cmd+Shift+N`, the button, the palette): opens the composer. The focus
    /// goes to the topic when it is empty, else to the payload.
    pub(super) fn open_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.comp.vis.open();
        if self.comp.topic.get(cx).is_empty() {
            self.comp
                .topic
                .input
                .update(cx, |i, cx| i.focus(window, cx));
        } else {
            self.comp.payload.update(cx, |e, cx| e.focus(window, cx));
        }
        cx.notify();
    }

    /// The one close operation: the Close item, `Esc` and the palette. The draft stays. The
    /// focus moves to the message list, so that it never stays on a control that is gone.
    pub(super) fn close_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.comp.vis.close() {
            self.list_focus.focus(window, cx);
            cx.notify();
        }
    }

    /// "Composer: Properties": opens the composer and the Properties section.
    pub(super) fn open_properties(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(window, cx);
        self.comp.vis.props = true;
        cx.notify();
    }

    pub(super) fn focus_publish_topic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.comp.vis.open();
        self.comp
            .topic
            .input
            .update(cx, |i, cx| i.focus(window, cx));
        cx.notify();
    }

    /// The number in "Properties (N)", from the property fields only.
    fn props_count(&self, cx: &App) -> usize {
        let c = &self.comp;
        let filled = |i: &Entity<InputState>| !i.read(cx).value().is_empty();
        PropPresence {
            payload_format: c.utf8,
            content_type: c.content_type_empty || filled(&c.content_type.input),
            response_topic: filled(&c.response_topic.input),
            correlation: c.correlation_empty || filled(&c.correlation),
            expiry: filled(&c.expiry),
            user: c
                .rows
                .iter()
                .filter(|r| r.keep_empty || filled(&r.key.input) || filled(&r.value.input))
                .count(),
        }
        .count()
    }

    /// Adds a property to the section. It shows at once, also while it is empty.
    pub(super) fn add_prop(&mut self, kind: PropKind, window: &mut Window, cx: &mut Context<Self>) {
        // The focus goes to the new field, so that the user can type at once.
        let focus = match kind {
            PropKind::User => {
                let row = self.new_row("", "", false, window, cx);
                let input = row.key.input.clone();
                self.comp.rows.push(row);
                Some(input)
            }
            PropKind::PayloadFormat => {
                self.comp.utf8 = true;
                self.comp.shown.set(kind, true);
                // No input takes the focus: the row is the first in the area.
                self.scroll_props_to_top();
                None
            }
            _ => {
                self.comp.shown.set(kind, true);
                match kind {
                    PropKind::ContentType => Some(self.comp.content_type.input.clone()),
                    PropKind::ResponseTopic => Some(self.comp.response_topic.input.clone()),
                    PropKind::Correlation => Some(self.comp.correlation.clone()),
                    _ => Some(self.comp.expiry.clone()),
                }
            }
        };
        if let Some(input) = focus {
            input.update(cx, |i, cx| i.focus(window, cx));
        }
        // A new user row is the last in the scroll area, and it has no place yet. The focus in a
        // standard field reveals it (`reveal_std`).
        if kind == PropKind::User {
            self.comp.props_scroll.scroll_to_bottom();
            self.comp.draw_all.set(true);
        }
        self.comp.vis.props = true;
        self.comp.model.edited();
        cx.notify();
    }

    /// Removes a property: it hides and its value is gone.
    fn remove_prop(&mut self, kind: PropKind, window: &mut Window, cx: &mut Context<Self>) {
        match kind {
            PropKind::PayloadFormat => self.comp.utf8 = false,
            PropKind::ContentType => {
                self.comp.content_type.set("", window, cx);
                self.comp.content_type_empty = false;
            }
            PropKind::ResponseTopic => self.comp.response_topic.set("", window, cx),
            PropKind::Correlation => {
                self.comp
                    .correlation
                    .update(cx, |i, cx| i.set_value("", window, cx));
                self.comp.correlation_empty = false;
            }
            PropKind::Expiry => self
                .comp
                .expiry
                .update(cx, |i, cx| i.set_value("", window, cx)),
            PropKind::User => return,
        }
        self.comp.shown.set(kind, false);
        self.comp.model.edited();
        // The remove button is gone: the focus moves to the payload, which stays.
        self.comp.payload.update(cx, |e, cx| e.focus(window, cx));
        cx.notify();
    }

    /// "History…": opens the composer and a list of Drafts and Sent. A choice fills the composer.
    pub(super) fn open_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer(window, cx);
        if window.has_active_dialog(cx) {
            return;
        }
        let list = |items: Vec<(u64, SharedString)>| items;
        let drafts = list(
            self.comp
                .model
                .history
                .drafts()
                .map(|e| (e.seq, e.label.clone().into()))
                .collect(),
        );
        let sent = list(
            self.comp
                .model
                .history
                .sent()
                .map(|e| (e.seq, e.label.clone().into()))
                .collect(),
        );
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let group = |title: &'static str, items: &[(u64, SharedString)], is_draft: bool| {
                (!items.is_empty()).then(|| {
                    v_flex()
                        .child(div().text_xs().text_color(muted).child(title))
                        .children(items.iter().map(|(seq, label)| {
                            let (weak, seq) = (weak.clone(), *seq);
                            let id = (if is_draft { "draft" } else { "sent" }, seq as usize);
                            // A button is a tab stop: Tab moves, Enter or Space recalls.
                            Button::new(id)
                                .ghost()
                                .w_full()
                                .justify_start()
                                .label(label.clone())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    weak.update(cx, |v, cx| v.recall(seq, is_draft, window, cx))
                                        .ok();
                                })
                        }))
                })
            };
            let empty = drafts.is_empty() && sent.is_empty();
            dialog
                .title("History")
                .child(
                    v_flex()
                        .id("history-list")
                        .key_context("HistoryList")
                        .gap_2()
                        .max_h(ui(360.))
                        .overflow_y_scroll()
                        .when(empty, |d| d.child(div().text_sm().child("No history yet.")))
                        .children(group("Drafts", &drafts, true))
                        .children(group("Sent", &sent, false)),
                )
                .footer(
                    h_flex().justify_end().child(
                        Button::new("history-close")
                            .label("Close")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    /// Beautify (`pretty`) or Minify the JSON payload. The work runs in the background. A result
    /// for text that the user has changed since is dropped.
    pub(super) fn format_json(
        &mut self,
        pretty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.comp.mode.is_text() {
            return;
        }
        let size = self.comp.payload.read(cx).text().len();
        if size > FORMAT_MAX_IN {
            self.comp.error = Some(format!(
                "Format works up to {}. This payload has {}.",
                size_text(FORMAT_MAX_IN),
                size_text(size)
            ));
            cx.notify();
            return;
        }
        let snapshot = self.comp.payload.read(cx).value().to_string();
        let text = snapshot.clone();
        self.comp.format_task = Some(cx.spawn_in(window, async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move {
                    let bytes = text.into_bytes();
                    decode::json(&bytes, pretty, &AtomicBool::new(false))
                })
                .await;
            this.update_in(cx, |v, window, cx| {
                if v.comp.payload.read(cx).value() != snapshot.as_str() {
                    return;
                }
                match out.error {
                    Some(e) => v.comp.error = Some(e),
                    None if out.text.len() > FORMAT_MAX_OUT => {
                        v.comp.error = Some(format!(
                            "The formatted text has {}. Format works up to {}.",
                            size_text(out.text.len()),
                            size_text(FORMAT_MAX_OUT)
                        ));
                    }
                    None => {
                        let old = v.read_draft(cx);
                        if !v.comp.model.replace_with(old, true) {
                            window.push_notification(
                                Notification::warning(
                                    "The old draft is larger than 1 MiB. It is not kept.",
                                ),
                                cx,
                            );
                        }
                        v.comp
                            .payload
                            .update(cx, |e, cx| e.set_value(out.text, window, cx));
                        v.comp.error = None;
                        // `set_value` sends no Change event: check the new text here.
                        v.check_json(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Ctrl/Cmd+Enter and the Send button.
    pub(super) fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The result line already shows the hint. An error here would hide a recorded result.
        if !self.connected() {
            return;
        }
        let d = self.read_draft(cx);
        self.publish_draft(&d, true, window, cx);
    }

    /// Checks `d` with the core rules, sends it and records it in the list. `from_composer`: the
    /// draft goes to Sent and errors show under the composer.
    pub(super) fn publish_draft(
        &mut self,
        d: &Draft,
        from_composer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let op_id = self.next_op;
        let v3 = self.composer_protocol() == Protocol::V311;
        // An MQTT 3.1.1 message has no properties. The hidden fields must not block it.
        let mut wire = d.clone();
        if v3 {
            wire.drop_properties();
        }
        let checked = wire.to_outgoing(op_id).and_then(|o| {
            validate_outgoing(&o, self.net.broker_max_packet())?;
            if v3 {
                validate_v3_size(&o, NetLimits::default().max_packet)?;
            }
            Ok(o)
        });
        let out = match checked {
            Ok(o) => o,
            Err(e) => {
                if from_composer {
                    self.comp.error = Some(e);
                } else {
                    window.push_notification(Notification::error(e), cx);
                }
                cx.notify();
                return;
            }
        };
        self.next_op += 1;
        match self.net.send(Command::Publish(Box::new(out.clone()))) {
            Ok(()) => {
                // The list row shows the progress. The broker sends the message back as a
                // normal received message if a subscription matches.
                self.capture_anchor();
                self.store.record_sent(
                    op_id,
                    &out.topic,
                    out.payload,
                    out.qos,
                    out.retain,
                    out.props,
                    SystemTime::now(),
                    self.conn_protocol,
                );
                self.restore_anchor();
                self.last_result = Some((op_id, status_text(&SendStatus::Sending)));
                if from_composer {
                    self.comp.model.sent(d.clone());
                    self.comp.error = None;
                }
            }
            Err(e) => {
                let why = send_error_text(e).to_string();
                if from_composer {
                    self.comp.error = Some(why);
                } else {
                    window.push_notification(Notification::error(why), cx);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn render_composer(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        if !self.comp.vis.open {
            // Closed: one text button. The editors and the draft stay.
            return h_flex()
                .border_t_1()
                .border_color(theme.border)
                .p_2()
                .gap_2()
                .items_center()
                .child(
                    Button::new("new-message")
                        .small()
                        .label("New message")
                        .on_click(cx.listener(|v, _, window, cx| v.open_composer(window, cx))),
                )
                .child(div().text_xs().text_color(muted).children(shortcut_text(
                    &NewMessage,
                    Some(CONTEXT),
                    window,
                )))
                .into_any_element();
        }
        let connected = self.connected();
        let v5 = self.composer_protocol() == Protocol::V5;
        let result = self
            .last_result
            .as_ref()
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        // Only for MQTT 5, and from the small property fields: no payload copy in a frame.
        let count = if v5 { self.props_count(cx) } else { 0 };
        let hex = self.comp.mode == PayloadMode::Hex;
        let props_open = self.comp.vis.props;
        let weak = cx.entity().downgrade();
        let (rem, size, settle) = (
            window.rem_size(),
            self.comp.size.clone(),
            self.comp.settle.clone(),
        );
        let (scroll, view_height) = (
            self.comp.props_scroll.clone(),
            self.comp.view_height.clone(),
        );
        let stacked = props_stacked(size.get().map(|s| s.width), rem);
        // The composer measures itself. A change of the stacking draws the window again, and two
        // frames later (the rows have their new height) the row with the focus, if any, is shown.
        let this = cx.entity().downgrade();
        let (paint_this, paint_settle) = (this.clone(), settle.clone());
        let measure = canvas(
            move |b, window, cx| {
                let old = size.get();
                size.set(Some(b.size));
                let was = props_stacked(old.map(|s| s.width), rem);
                let moved = old.is_some_and(|o| {
                    (o.width - b.size.width).abs() > px(0.5)
                        || (o.height - b.size.height).abs() > px(0.5)
                });
                if was != props_stacked(Some(b.size.width), rem) || moved {
                    settle.set(2);
                }
                let left = settle.get();
                if left > 0 {
                    settle.set(left - 1);
                    let this = this.clone();
                    window.defer(cx, move |window, cx| {
                        this.update(cx, |v, cx| {
                            if left == 1 {
                                v.reveal_focused_row(window, cx);
                            }
                            cx.notify();
                        })
                        .ok();
                    });
                }
            },
            // A result line can take room from the Properties area: its height changes. The area
            // has its new size when the window paints, so the check is here.
            move |_, _, window, cx| {
                let view = scroll.bounds().size.height;
                if (view_height.get() - view).abs() > px(0.5) {
                    view_height.set(view);
                    paint_settle.set(2);
                    let this = paint_this.clone();
                    window.defer(cx, move |_, cx| {
                        this.update(cx, |_, cx| cx.notify()).ok();
                    });
                }
            },
        )
        .absolute()
        .size_full();
        // The `⋯` menu: Beautify JSON, Minify JSON, History… and Properties (MQTT 5).
        let more_focus = self.comp.more_focus.clone();
        let more = Button::new("composer-more")
            .debug_selector(|| "composer-more".into())
            .track_focus(&self.comp.more_focus)
            .small()
            .ghost()
            .icon(IconName::Ellipsis)
            .tab_stop(true)
            .tooltip("Composer actions")
            .dropdown_menu(move |menu, _, _| {
                let menu = menu.action_context(more_focus.clone());
                let item = |label: &'static str,
                            run: fn(&mut Navigator, &mut Window, &mut Context<Navigator>)| {
                    let weak = weak.clone();
                    PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        weak.update(cx, |v, cx| run(v, window, cx)).ok();
                    })
                };
                let mut menu = menu
                    .item(
                        item("Beautify JSON", |v, w, cx| v.format_json(true, w, cx)).disabled(hex),
                    )
                    .item(item("Minify JSON", |v, w, cx| v.format_json(false, w, cx)).disabled(hex))
                    .separator()
                    .item(item("History…", |v, w, cx| v.open_history(w, cx)));
                if v5 {
                    menu = menu.item(
                        item("Properties", |v, _, cx| {
                            v.comp.vis.props = !v.comp.vis.props;
                            cx.notify();
                        })
                        .checked(props_open),
                    );
                }
                menu
            });

        let topic_note = self.comp.topic.note("topic", cx);
        let header = h_flex()
            .gap_2()
            .items_center()
            .child(
                div().flex_1().child(
                    Input::new(&self.comp.topic.input)
                        .small()
                        .aria_label("Topic"),
                ),
            )
            .when(v5 && count > 0, |d| {
                d.child(
                    Button::new("props-tag")
                        .xsmall()
                        .label(format!("Properties ({count})"))
                        .tooltip("Show or hide the properties")
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.comp.vis.props = !v.comp.vis.props;
                            cx.notify();
                        })),
                )
            })
            .child(
                Button::new("composer-close")
                    .debug_selector(|| "composer-close".into())
                    .small()
                    .ghost()
                    .icon(IconName::Close)
                    // Tab goes from the topic to the payload. The keyboard closes with Esc.
                    .tab_stop(false)
                    .tooltip("Close (Esc). The draft stays.")
                    .on_click(cx.listener(|v, _, window, cx| v.close_composer(window, cx))),
            );

        // The last result, the error or the JSON warning, under the payload. Show long results on a maximum of three lines.
        let line = if let Some(e) = self.comp.error.clone() {
            div().text_xs().text_color(theme.danger).child(e)
        } else if let Some(w) = self.comp.json_warning.clone() {
            div()
                .debug_selector(|| "json-warning".into())
                .text_xs()
                .text_color(theme.warning)
                .child(format!("{w} Publish still sends it as is."))
        } else {
            div()
                .text_xs()
                .text_color(muted)
                .child(result_line(connected, result))
        };

        let mode_ix = match self.comp.mode {
            PayloadMode::Text => 0,
            PayloadMode::Hex => 1,
            PayloadMode::Json => 2,
        };
        // A toggle group with a selected state. A click on the selected one keeps it.
        let qos = ToggleGroup::new("qos")
            .segmented()
            .single()
            .small()
            .child(
                Toggle::new("qos-0")
                    .label("QoS 0")
                    .checked(self.comp.qos == Qos::AtMostOnce),
            )
            .child(
                Toggle::new("qos-1")
                    .label("QoS 1")
                    .checked(self.comp.qos == Qos::AtLeastOnce),
            )
            .on_click(cx.listener(|v, checks: &Vec<bool>, _, cx| {
                if let Some(ix) = checks.iter().position(|c| *c) {
                    v.comp.qos = if ix == 0 {
                        Qos::AtMostOnce
                    } else {
                        Qos::AtLeastOnce
                    };
                    v.comp.model.edited();
                    cx.notify();
                }
            }));
        let controls = h_flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(
                div()
                    .debug_selector(|| "payload-mode".into())
                    .w(ui(165.))
                    .child(
                        TabBar::new("payload-mode")
                            .segmented()
                            .small()
                            .selected_index(mode_ix)
                            .on_click(cx.listener(|v, ix: &usize, window, cx| {
                                let to = match ix {
                                    0 => PayloadMode::Text,
                                    1 => PayloadMode::Hex,
                                    _ => PayloadMode::Json,
                                };
                                v.set_payload_mode(to, window, cx);
                            }))
                            .child("Text")
                            .child("Hex")
                            .child("JSON"),
                    ),
            )
            .child(qos)
            .child(
                Switch::new("retain")
                    .checked(self.comp.retain)
                    .label("Retain")
                    .on_change(cx.listener(|v, on: &bool, _, cx| {
                        v.comp.retain = *on;
                        v.comp.model.edited();
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(more)
            .child(
                Button::new("send")
                    .debug_selector(|| "send".into())
                    .small()
                    .primary()
                    .label("Publish")
                    .tooltip_with_action("Publish the message", &SendMessage, Some("Composer"))
                    .disabled(!connected)
                    .on_click(cx.listener(|v, _, window, cx| v.send(window, cx))),
            );

        // The composer has a maximum height. The properties are the only part that shrinks: they scroll
        // and the other rows (topic, payload, the action row) stay in view.
        v_flex()
            .relative()
            .key_context("Composer")
            .debug_selector(|| "composer".into())
            .max_h(composer_max_height(
                window.viewport_size().height,
                window.rem_size(),
            ))
            .on_action(cx.listener(|v, _: &CloseComposer, window, cx| v.close_composer(window, cx)))
            .border_t_1()
            .border_color(theme.border)
            .p_2()
            .gap_2()
            .child(measure)
            .child(header.flex_none())
            .when_some(topic_note, |d, n| {
                d.child(div().flex_none().text_xs().text_color(muted).child(n))
            })
            .child(
                div().flex_none().child(
                    Editor::new(&self.comp.payload)
                        .h(ui(PAYLOAD_HEIGHT))
                        .bordered(true)
                        .small(),
                ),
            )
            // Show long results on a maximum of three lines: the action row stays in view.
            .child(line.flex_none().line_clamp(3).text_ellipsis())
            .when(v5 && props_open, |d| {
                d.child(self.render_props(stacked, window, cx))
            })
            .child(controls.flex_none())
            .into_any_element()
    }

    /// The "Properties" section: only the fields that are present, and an "Add property" menu
    /// for the others. "Send empty" keeps a property that is present but empty. A line break or
    /// a hidden character in a text is kept (see [`LineKeep`]).
    /// Which user rows to build in this frame. Each row has two inputs, and each input costs
    /// about 0.1 ms in every frame (a focus move draws the whole window, also the cached views).
    /// So the rows far from the viewport are empty boxes. A row that never had a place, the first
    /// and the last row (the ends of the tab order) and the row with the focus are always built.
    fn rows_to_draw(&self, window: &Window, cx: &App) -> Vec<bool> {
        let rows = &self.comp.rows;
        let extents: Vec<_> = rows.iter().map(|r| r.spot.extent.get()).collect();
        let mut focused: Vec<_> = rows
            .iter()
            .map(|r| {
                [&r.spot.focus, &r.spot.switch, &r.spot.remove]
                    .into_iter()
                    .any(|h| h.contains_focused(window, cx))
            })
            .collect();
        // Another part of the window (the palette) can take the focus and give it back to an
        // input of a row. The row must stay built, or the focus handle has no element.
        match focused.iter().position(|f| *f) {
            Some(i) => self.comp.focus_row.set(Some(rows[i].id)),
            None => {
                let at = rows
                    .iter()
                    .position(|r| Some(r.id) == self.comp.focus_row.get());
                at.into_iter().for_each(|i| focused[i] = true);
            }
        }
        let scroll = &self.comp.props_scroll;
        // The size of the scroll area is from the last layout. After a resize it is too small for
        // this frame, so take the largest size that the area can have.
        let window_h = window.viewport_size().height;
        let resized = self.comp.window_h.replace(window_h) != window_h;
        let view = if resized {
            composer_max_height(window_h, window.rem_size())
        } else {
            scroll.bounds().size.height
        };
        #[cfg(all(test, debug_assertions))]
        self.comp.resized.set(resized);
        let (max, old_view) = (scroll.max_offset().y, scroll.bounds().size.height);
        // A wheel scroll past an end is clamped in the next layout.
        let mut top = (-scroll.offset().y).clamp(px(0.), max);
        if resized {
            // A taller area has a smaller maximum offset, so the next layout moves the top up.
            top = top.min(max + old_view - view).max(px(0.));
        }
        rows_in_view(&extents, &focused, top, view)
    }

    fn render_props(
        &self,
        stacked: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let shown = self.comp.shown;
        let empty_switch =
            |id: &'static str, on: bool| Switch::new(id).checked(on).label("Send empty");
        let remove = |kind: PropKind, cx: &mut Context<Self>| {
            Button::new(("prop-remove", kind as usize))
                .track_focus(&self.comp.std_spots[kind as usize].remove)
                .small()
                .ghost()
                .icon(IconName::Close)
                .tooltip("Remove this property")
                .on_click(cx.listener(move |v, _, window, cx| v.remove_prop(kind, window, cx)))
        };
        // The row height depends on the stacking, the rem size and the width: the old heights are wrong.
        let key = (
            stacked,
            window.rem_size(),
            self.comp.size.get().map(|s| s.width.round()),
        );
        if self.comp.rows_key.replace(key) != key {
            self.comp.rows.iter().for_each(|r| r.spot.extent.set(None));
        }
        // A new row scrolls the area to its end in the next layout. The rows that the scroll shows
        // are built in this frame too.
        let draw = if self.comp.draw_all.take() {
            vec![true; self.comp.rows.len()]
        } else {
            self.rows_to_draw(window, cx)
        };
        #[cfg(all(test, debug_assertions))]
        {
            let n = draw.iter().filter(|d| **d).count();
            self.comp.drawn_rows.set(n);
            if self.comp.resized.get() {
                self.comp.resized_rows.set(n);
            }
        }
        let rows = self.comp.rows.iter().zip(draw).map(|(r, draw)| {
            if !draw {
                // Out of view: an empty box of the height that the row had. It is no tab stop.
                let (_, h) = r.spot.extent.get().expect("a skipped row was drawn");
                return div().h(h).flex_none().into_any_element();
            }
            let id = r.id;
            let n = r.key.note("name", cx).or_else(|| r.value.note("value", cx));
            let names = h_flex()
                .gap_2()
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&r.key.input)
                            .small()
                            .aria_label("User property name"),
                    ),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&r.value.input)
                            .small()
                            .aria_label("User property value"),
                    ),
                );
            let switch = Switch::new(("user-prop-empty", id as usize))
                .checked(r.keep_empty)
                .label("Send empty")
                .on_change(cx.listener(move |v, on: &bool, _, cx| {
                    if let Some(r) = v.comp.rows.iter_mut().find(|r| r.id == id) {
                        r.keep_empty = *on;
                    }
                    v.comp.model.edited();
                    cx.notify();
                }));
            let remove = Button::new(("user-prop-remove", id as usize))
                .track_focus(&r.spot.remove)
                .small()
                .ghost()
                .icon(IconName::Close)
                .tooltip("Remove this property")
                .on_click(cx.listener(move |v, _, window, cx| {
                    v.comp.rows.retain(|r| r.id != id);
                    v.comp.model.edited();
                    // The row and its button are gone: focus a control that stays.
                    v.comp.payload.update(cx, |e, cx| e.focus(window, cx));
                    cx.notify();
                }));
            spot_div(
                &r.spot,
                &self.comp.props_scroll,
                prop_row(
                    stacked,
                    // A stacked row has no Name and Value captions above it: the label says it.
                    if stacked {
                        "User property (name, value)"
                    } else {
                        "User property"
                    },
                    muted,
                    names,
                    Some(r.spot.switch_div(switch)),
                    remove,
                    n,
                ),
            )
            .into_any_element()
        });
        let rows: Vec<_> = rows.collect();
        let has_rows = !rows.is_empty();
        let addable = shown.addable();
        let weak = cx.entity().downgrade();
        let add_focus = self.comp.add_focus.clone();
        let add = Button::new("add-prop")
            .debug_selector(|| "add-prop".into())
            .track_focus(&self.comp.add_focus)
            .xsmall()
            .ghost()
            .icon(IconName::Plus)
            .label("Add property")
            .dropdown_caret(true)
            .dropdown_menu(move |menu, _, _| {
                let menu = menu.action_context(add_focus.clone());
                addable.iter().fold(menu, |menu, kind| {
                    let (weak, kind) = (weak.clone(), *kind);
                    menu.item(
                        PopupMenuItem::new(kind.label()).on_click(move |_, window, cx| {
                            weak.update(cx, |v, cx| v.add_prop(kind, window, cx)).ok();
                        }),
                    )
                })
            });
        let section = v_flex()
            .gap_1()
            .when(shown.payload_format, |d| {
                d.child(self.comp.std_spot(
                    PropKind::PayloadFormat,
                    prop_row(
                        stacked,
                        "Payload format",
                        muted,
                        div().text_sm().child("UTF-8"),
                        None,
                        remove(PropKind::PayloadFormat, cx),
                        None,
                    ),
                ))
            })
            .when(shown.content_type, |d| {
                d.child(
                    self.comp.std_spot(
                        PropKind::ContentType,
                        prop_row(
                            stacked,
                            "Content type",
                            muted,
                            Input::new(&self.comp.content_type.input)
                                .small()
                                .aria_label("Content type"),
                            Some(
                                self.comp.std_spots[PropKind::ContentType as usize].switch_div(
                                    empty_switch(
                                        "content-type-empty",
                                        self.comp.content_type_empty,
                                    )
                                    .on_click(cx.listener(
                                        |v, on: &bool, _, cx| {
                                            v.comp.content_type_empty = *on;
                                            v.comp.model.edited();
                                            cx.notify();
                                        },
                                    )),
                                ),
                            ),
                            remove(PropKind::ContentType, cx),
                            self.comp.content_type.note("value", cx),
                        ),
                    ),
                )
            })
            .when(shown.response_topic, |d| {
                d.child(
                    self.comp.std_spot(
                        PropKind::ResponseTopic,
                        prop_row(
                            stacked,
                            "Response topic",
                            muted,
                            Input::new(&self.comp.response_topic.input)
                                .small()
                                .aria_label("Response topic"),
                            None,
                            remove(PropKind::ResponseTopic, cx),
                            self.comp.response_topic.note("value", cx),
                        ),
                    ),
                )
            })
            .when(shown.correlation, |d| {
                d.child(
                    self.comp.std_spot(
                        PropKind::Correlation,
                        prop_row(
                            stacked,
                            "Correlation data (hex)",
                            muted,
                            Input::new(&self.comp.correlation)
                                .small()
                                .aria_label("Correlation data (hex)"),
                            Some(
                                self.comp.std_spots[PropKind::Correlation as usize].switch_div(
                                    empty_switch("correlation-empty", self.comp.correlation_empty)
                                        .on_click(cx.listener(|v, on: &bool, _, cx| {
                                            v.comp.correlation_empty = *on;
                                            v.comp.model.edited();
                                            cx.notify();
                                        })),
                                ),
                            ),
                            remove(PropKind::Correlation, cx),
                            None,
                        ),
                    ),
                )
            })
            .when(shown.expiry, |d| {
                d.child(
                    self.comp.std_spot(
                        PropKind::Expiry,
                        prop_row(
                            stacked,
                            "Message expiry (seconds)",
                            muted,
                            Input::new(&self.comp.expiry)
                                .small()
                                .aria_label("Message expiry (seconds)"),
                            None,
                            remove(PropKind::Expiry, cx),
                            None,
                        ),
                    ),
                )
            });
        // The Name and Value captions (a stacked row has the hints in the inputs).
        let captions = (has_rows && !stacked).then(|| {
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(muted)
                .child(div().w(ui(LABEL_WIDTH)).min_w_0())
                .child(
                    h_flex()
                        .flex_1()
                        .gap_2()
                        .child(div().flex_1().child("Name"))
                        .child(div().flex_1().child("Value")),
                )
                .child(div().w(ui(END_WIDTH)).min_w_0())
        });
        // The properties scroll as one block, down to nothing when the composer is at its
        // maximum height. The Add button and the action row stay in view.
        v_flex()
            .gap_1()
            .min_h_0()
            .child(
                v_flex()
                    .relative()
                    .min_h_0()
                    .child(
                        v_flex()
                            .gap_1()
                            .pr_2()
                            .min_h_0()
                            .child(section)
                            .children(captions)
                            .children(rows)
                            .id("props-rows")
                            .overflow_y_scroll()
                            .lock_scroll_axis()
                            .track_scroll(&self.comp.props_scroll),
                    )
                    .vertical_scrollbar(&self.comp.props_scroll),
            )
            .child(h_flex().flex_none().child(add))
    }
}

/// One row of the Properties section: a label column, the field, and a column at the end for
/// "Send empty" and Remove. A note (a line break or a hidden character) shows under the field.
fn prop_row(
    stacked: bool,
    label: &'static str,
    muted: Hsla,
    field: impl IntoElement,
    empty: Option<Div>,
    remove: Button,
    note: Option<String>,
) -> impl IntoElement {
    let end = h_flex().gap_1().justify_end().children(empty).child(remove);
    let label = div().min_w_0().text_xs().text_color(muted).child(label);
    // A narrow pane: the label and the end column share the first line, the field has the second.
    let (head, indent) = if stacked {
        (
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .justify_between()
                        .child(label.flex_1())
                        .child(end.flex_none()),
                )
                .child(div().min_w_0().child(field)),
            0.,
        )
    } else {
        (
            v_flex().child(
                h_flex()
                    .gap_2()
                    .child(label.w(ui(LABEL_WIDTH)))
                    .child(div().flex_1().min_w_0().child(field))
                    .child(end.w(ui(END_WIDTH)).min_w_0()),
            ),
            LABEL_WIDTH + 8.,
        )
    };
    head.children(note.map(|n| div().pl(ui(indent)).text_xs().text_color(muted).child(n)))
}

/// The line under the payload: the last result while there is one, else the hint for a closed
/// connection.
fn result_line(connected: bool, result: String) -> String {
    if connected || !result.is_empty() {
        result
    } else {
        "Connect to a broker first.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::{STACK_BELOW, composer_max_height, props_stacked, result_line, ui};
    use gpui_kit::px;

    #[core::prelude::v1::test]
    fn a_closed_connection_keeps_the_last_result_on_screen() {
        let unknown = "Unknown: the connection closed before the result.";
        assert_eq!(result_line(false, unknown.into()), unknown);
        assert_eq!(
            result_line(false, String::new()),
            "Connect to a broker first."
        );
        assert_eq!(result_line(true, "Sent".into()), "Sent");
    }

    #[core::prelude::v1::test]
    fn rows_stack_in_a_narrow_pane_only() {
        let rem = px(16.);
        // The least centre width (360) stacks. The limit and a wider centre do not. An unknown
        // width (before the first layout) does not.
        assert!(props_stacked(Some(ui(360.).to_pixels(rem)), rem));
        assert!(!props_stacked(Some(ui(STACK_BELOW).to_pixels(rem)), rem));
        assert!(!props_stacked(None, rem));
    }

    #[core::prelude::v1::test]
    fn the_composer_height_is_a_part_of_the_window_with_a_floor() {
        let (rem, big) = (px(16.), px(24.));
        // 55% of a window. It is above the floor of 280 design pixels at the default zoom.
        assert_eq!(composer_max_height(px(900.), rem), px(495.));
        assert_eq!(composer_max_height(px(600.), rem), px(330.));
        // At 150% zoom the floor (420 px) is above 55% of the window (330 px).
        assert_eq!(composer_max_height(px(600.), big), px(420.));
        // The composer takes at most 80% of the window.
        assert_eq!(composer_max_height(px(400.), big), px(320.));
    }
}
