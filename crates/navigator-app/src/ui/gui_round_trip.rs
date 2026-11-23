//! End to end: the tests use only the window. They connect, subscribe, publish and read the
//! messages back from the list, through a real broker (`tests/broker/up.sh mosquitto`).

use std::time::{Duration, Instant};

#[cfg(feature = "broker-tests")]
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::InputState;
use gpui_kit::{
    AppContext, AsKeystroke as _, Entity, Modifiers, TestAppContext, VisualTestContext, px, size,
};
use navigator_core::net::{Profile, Transport};
use navigator_core::settings::{Load, Settings};
use navigator_core::store::{Direction, RowView};
use uuid::Uuid;

#[cfg(feature = "broker-tests")]
use super::View;
use super::{ConnState, Navigator, SubState};

/// A short random id. The GUI types each character, so a full UUID costs time.
fn rid() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// Every row of the message list: direction, topic and payload text.
fn rows(v: &Navigator) -> Vec<(Direction, String, String)> {
    (0..v.store.list_len())
        .filter_map(|i| match v.store.list_row(i)? {
            RowView::Message {
                topic,
                msg,
                direction,
                ..
            } => Some((
                direction,
                topic.to_string(),
                String::from_utf8_lossy(&msg.payload).into_owned(),
            )),
            _ => None,
        })
        .collect()
}

/// Runs the test executor and the real net thread until `done`. The test clock is not real time,
/// so each turn moves it on and then sleeps a little.
fn wait(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    cx.executor().allow_parking();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(
            Instant::now() < end,
            "no {what} after 10 s. Is the broker on localhost:18830 (tests/broker/up.sh mosquitto)?"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A real mouse click in the middle of the element that has this debug selector.
fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("no element with the debug selector {selector:?}"));
    cx.simulate_click(bounds.center(), Modifiers::none());
}

/// A real double click: the second press has a click count of 2.
fn double_click(cx: &mut VisualTestContext, selector: &'static str) {
    use gpui_kit::{MouseButton, MouseDownEvent, MouseUpEvent};
    let at = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("no element with the debug selector {selector:?}"))
        .center();
    for n in 1..=2 {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            click_count: n,
            ..Default::default()
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: at,
            click_count: n,
            ..Default::default()
        });
    }
}

/// Opens the composer with Ctrl+Shift+N, fills it and clicks Send. The shortcut focuses the
/// topic while it is empty, and the payload after that: then Shift+Tab goes back to the topic.
fn publish(cx: &mut VisualTestContext, topic: &str, text: &str, first: bool) {
    cx.simulate_keystrokes("secondary-shift-n");
    if !first {
        cx.simulate_keystrokes("shift-tab");
    }
    cx.simulate_keystrokes("secondary-a");
    cx.simulate_input(topic);
    // Tab moves the focus from the topic to the payload.
    cx.simulate_keystrokes("tab secondary-a");
    cx.simulate_input(text);
    click(cx, "send");
}

/// Types a filter in the input of the left pane, presses Enter and waits for the SUBACK.
fn subscribe(cx: &mut VisualTestContext, nav: &Entity<Navigator>, f: &str) {
    let input: Entity<InputState> = nav.read_with(cx, |v, _| v.sub_input.clone());
    cx.update(|w, cx| input.update(cx, |i, cx| i.focus(w, cx)));
    cx.simulate_input(f);
    cx.simulate_keystrokes("enter");
    wait(cx, &format!("SUBACK for {f}"), |cx| {
        nav.read_with(cx, |v, _| {
            v.subs
                .rows
                .iter()
                .any(|s| &*s.filter == f && matches!(s.state, SubState::Active(_)))
        })
    });
}

/// Opens the window with a saved profile for the test Mosquitto, and clicks Connect.
fn start(cx: &mut TestAppContext) -> (VisualTestContext, Entity<Navigator>) {
    let (mut cx, nav) = open(cx);
    click(&mut cx, "connect");
    wait(&mut cx, "connection", |cx| {
        nav.read_with(cx, |v, _| v.status.state == ConnState::Connected)
    });
    (cx, nav)
}

/// Opens the window with a saved profile for the test Mosquitto. It does not connect.
fn open(cx: &mut TestAppContext) -> (VisualTestContext, Entity<Navigator>) {
    // An in-memory keyring: the test never reads the keyring of the user.
    // The net thread wakes tasks of the UI at any time. The test must not stop it.
    cx.executor().allow_parking();
    keyring_core::set_default_store(keyring_core::mock::Store::new().expect("mock keyring"));
    cx.update(|cx| {
        gpui_kit::init(cx);
        super::init(cx);
    });
    let profile = Profile {
        name: "gui round trip".into(),
        host: "localhost".into(),
        port: 18830,
        transport: Transport::Tcp,
        client_id: format!("nav-gui-{}", Uuid::new_v4()),
        // The default profile subscribes to `#`. The tests subscribe by themselves.
        subscriptions: Vec::new(),
        ..Profile::default()
    };
    let mut settings = Settings {
        last_profile_id: Some(profile.id),
        profiles: vec![profile],
        ..Settings::default()
    };
    // No update check: it is a real network request.
    settings.ui.check_updates = false;
    let load = Load {
        settings,
        notice: None,
        read_only: false,
    };
    let (win, nav) = cx.update(|cx| {
        let (win, nav) = gpui_kit::open_window(
            gpui_kit::WindowOptions {
                window_bounds: Some(gpui_kit::WindowBounds::Windowed(gpui_kit::Bounds {
                    origin: Default::default(),
                    size: size(px(1400.), px(900.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| Navigator::new(None, load, window, cx)),
        )
        .expect("open the window");
        (win, nav)
    });
    let cx = VisualTestContext::from_window(win, cx);
    cx.run_until_parked();
    (cx, nav)
}

#[cfg(feature = "broker-tests")]
#[gpui_kit::test]
fn a_message_goes_through_the_broker_and_comes_back_as_received(cx: &mut TestAppContext) {
    let (mut cx, nav) = start(cx);
    let cx = &mut cx;

    // Control: a message to hello/world before any subscription. It must never come back.
    let control = format!("c-{}", rid());
    publish(cx, "hello/world", &control, true);

    subscribe(cx, &nav, "hello/world");
    // JSON mode: the text is not JSON, so the composer warns, and Publish still sends it.
    let mode = cx
        .debug_bounds("payload-mode")
        .expect("payload mode switch");
    let json_tab = gpui_kit::point(mode.origin.x + mode.size.width * 5. / 6., mode.center().y);
    cx.simulate_click(json_tab, gpui_kit::Modifiers::none());
    let text = format!("h-{}", rid());
    publish(cx, "hello/world", &text, false);

    // The broker sends it back: a Received row. Our own Sent row is there too.
    wait(cx, &format!("Received row {text:?}"), |cx| {
        nav.read_with(cx, |v, _| {
            rows(v).contains(&(Direction::Received, "hello/world".into(), text.clone()))
        })
    });
    assert!(nav.read_with(cx, |v, _| v.comp.mode == crate::composer::PayloadMode::Json));
    assert!(
        cx.debug_bounds("json-warning").is_some(),
        "the warning is not drawn"
    );
    let all = nav.read_with(cx, |v, _| rows(v));
    assert!(
        all.contains(&(Direction::Sent, "hello/world".into(), text.clone())),
        "no Sent row in {all:?}"
    );
    assert!(
        all.contains(&(Direction::Sent, "hello/world".into(), control.clone())),
        "no Sent control row in {all:?}"
    );
    assert!(
        !all.iter()
            .any(|(d, _, p)| *d == Direction::Received && *p == control),
        "the control came back without a subscription: {all:?}"
    );

    // The ✕ of the composer closes it.
    click(cx, "composer-close");
    cx.run_until_parked();
    assert!(
        !nav.read_with(cx, |v, _| v.comp.vis.open),
        "the composer is still open"
    );

    // The same button disconnects.
    click(cx, "connect");
    wait(cx, "disconnect", |cx| {
        nav.read_with(cx, |v, _| v.status.state == ConnState::Disconnected)
    });
}

/// The Received rows: topic and payload.
fn received(cx: &mut VisualTestContext, nav: &Entity<Navigator>) -> Vec<(String, String)> {
    nav.read_with(cx, |v, _| {
        rows(v)
            .into_iter()
            .filter(|r| r.0 == Direction::Received)
            .map(|r| (r.1, r.2))
            .collect()
    })
}

/// The debug selector of a subscription row (the test needs a `&'static str`).
fn sub_row(filter: &str) -> &'static str {
    Box::leak(format!("sub-{filter}").into_boxed_str())
}

#[cfg(feature = "broker-tests")]
#[gpui_kit::test]
fn wildcard_subscriptions_match_by_level_and_show_only_follows_the_row(cx: &mut TestAppContext) {
    let (mut cx, nav) = start(cx);
    let cx = &mut cx;
    let p = format!("w{}", rid());
    let (plus, hash) = (format!("{p}/+/temp"), format!("{p}/#"));
    subscribe(cx, &nav, &plus);
    subscribe(cx, &nav, &hash);

    // `+` is one level: `a/temp` has two levels between P and temp, so only `#` takes it.
    let sent: Vec<(String, String)> = ["room1/temp", "room1/hum", "room1/a/temp"]
        .into_iter()
        .map(|t| (format!("{p}/{t}"), format!("v{}", rid())))
        .collect();
    for (i, (topic, text)) in sent.iter().enumerate() {
        publish(cx, topic, text, i == 0);
    }
    // Mosquitto sends one copy for each matching subscription (the filters overlap on
    // `room1/temp`), and the app shows each copy: 2 + 1 + 1 Received rows.
    wait(cx, "4 Received rows", |cx| received(cx, &nav).len() >= 4);
    let count =
        |got: &[(String, String)], m: &(String, String)| got.iter().filter(|r| *r == m).count();
    let got = received(cx, &nav);
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(
        [
            count(&got, &sent[0]),
            count(&got, &sent[1]),
            count(&got, &sent[2])
        ],
        [2, 1, 1],
        "{got:?}"
    );

    // The tree is the first view. It shows the first level only: a click selects, and a double
    // click opens one level.
    let shown = |cx: &mut VisualTestContext| -> Vec<String> {
        nav.update(cx, |v, _| {
            let ids = v.store.rows().to_vec();
            ids.into_iter()
                .filter_map(|id| v.store.get(id))
                .map(|n| n.topic.to_owned())
                .filter(|t| t.starts_with(&p))
                .collect()
        })
    };
    assert!(nav.read_with(cx, |v, _| v.view == View::Tree));
    wait(cx, "the tree row", |cx| {
        shown(cx) == std::slice::from_ref(&p)
    });
    let row = nav.update(cx, |v, _| {
        let ids = v.store.rows().to_vec();
        let id = ids
            .into_iter()
            .find(|id| v.store.get(*id).is_some_and(|n| n.topic == p))
            .expect("tree row");
        &*Box::leak(format!("tree-{}", super::node_key(id)).into_boxed_str())
    });
    click(cx, row);
    cx.run_until_parked();
    assert_eq!(
        shown(cx),
        std::slice::from_ref(&p),
        "a single click opened the node"
    );
    double_click(cx, row);
    wait(cx, "the second level", |cx| {
        shown(cx) == [p.clone(), format!("{p}/room1")]
    });

    // The row of `room1/temp` has the colour of the first matching subscription: `+/temp`, row 0.
    let colours: Vec<Option<usize>> = nav.update(cx, |v, _| {
        sent.iter()
            .map(|(t, _)| v.store.colour_of_topic(t))
            .collect()
    });
    assert_eq!(colours, [Some(0), Some(1), Some(1)]);

    // Click the row of a subscription: Show only. Then the rows follow the filter.
    click(cx, sub_row(&plus));
    wait(cx, "Show only +/temp", |cx| received(cx, &nav).len() == 2);
    let got = received(cx, &nav);
    assert!(got.iter().all(|r| *r == sent[0]), "{got:?}");
    click(cx, sub_row(&hash));
    wait(cx, "Show only #", |cx| received(cx, &nav).len() == 4);
}

/// On Windows the title bar is the caption of the window. A press that no element stops goes to
/// the system move loop, and the loop takes the release: the click never comes. So each control
/// of the bar must stop the press, and the free bar must not (it moves the window).
#[gpui_kit::test]
fn title_bar_controls_take_the_press_and_the_free_bar_does_not(cx: &mut TestAppContext) {
    use gpui_kit::{
        MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, point,
    };
    let (mut cx, _nav) = open(cx);
    let cx = &mut cx;
    let press = |cx: &mut VisualTestContext, at| {
        cx.update(|w, cx| {
            w.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: at,
                    ..Default::default()
                }),
                cx,
            );
            let r = w.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: at,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            w.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: at,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
            r.propagate
        })
    };
    for id in ["connection-menu", "settings", "connect"] {
        let at = cx
            .debug_bounds(id)
            .unwrap_or_else(|| panic!("no {id}"))
            .center();
        assert!(!press(cx, at), "{id}: the press goes on to the window move");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
    }
    assert!(
        press(cx, point(px(700.), px(16.))),
        "the free bar must move the window"
    );
}

/// Tab from the topic filter input of the left pane reaches the list, also before a row is
/// selected.
#[gpui_kit::test]
fn tab_reaches_the_message_list(cx: &mut TestAppContext) {
    let (mut cx, nav) = open(cx);
    let input: Entity<InputState> = nav.read_with(&cx, |v, _| v.sub_input.clone());
    cx.update(|w, cx| input.update(cx, |i, cx| i.focus(w, cx)));
    let focused = |cx: &mut VisualTestContext| {
        let list = nav.read_with(cx, |v, _| v.list_focus.clone());
        cx.update(|w, _| list.is_focused(w))
    };
    assert!(!focused(&mut cx));
    // Some controls (the QoS list and the buttons of the header) come before the list in the Tab
    // order, so the test presses Tab until the list has the focus. No row is selected.
    for _ in 0..6 {
        if focused(&mut cx) {
            break;
        }
        cx.simulate_keystrokes("tab");
    }
    assert!(focused(&mut cx), "Tab did not reach the list");
}

/// The palette list is a snapshot. A "Disconnect" chosen after the connection dropped must not
/// start a connection: the action is checked again when it runs.
#[cfg(feature = "broker-tests")]
#[gpui_kit::test]
fn a_palette_action_chosen_after_the_state_changed_does_not_run(cx: &mut TestAppContext) {
    let (mut cx, nav) = start(cx);
    let cx = &mut cx;
    cx.simulate_keystrokes("secondary-k");
    cx.simulate_input("Disconnect");
    // The connection drops while the palette is open.
    nav.update(cx, |v, _| v.net.disconnect());
    wait(cx, "disconnect", |cx| {
        nav.read_with(cx, |v, _| v.status.state == ConnState::Disconnected)
    });
    let before = cx.update(|w, cx| w.notifications(cx).len());
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(nav.read_with(cx, |v, _| {
        v.status.state == ConnState::Disconnected && !v.connecting()
    }));
    // The palette says why nothing happened.
    assert_eq!(cx.update(|w, cx| w.notifications(cx).len()), before + 1);
}

/// Two messages of one topic can share the receive time and the size. A History row must still
/// show its own message. It needs no broker: the test fills the store itself.
#[gpui_kit::test]
fn a_history_row_shows_its_own_message_when_time_and_size_are_equal(cx: &mut TestAppContext) {
    use super::details::DetailTab;
    use navigator_core::net::Protocol;
    use navigator_core::store::Insert;
    use navigator_core::{Incoming, MsgProps, Qos};
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    let seqs = nav.update(cx, |v, cx| {
        let at = std::time::SystemTime::UNIX_EPOCH;
        let mut id = None;
        for text in ["aa", "bb"] {
            let r = v.store.insert(Incoming {
                generation: 0,
                topic: "t".into(),
                payload: text.into(),
                qos: Qos::AtMostOnce,
                retain: false,
                props: MsgProps::default(),
                received_at: at,
                protocol: Protocol::V5,
            });
            let Insert::Stored(n) = r else {
                panic!("not stored");
            };
            id = Some(n);
        }
        let id = id.expect("node");
        v.select(id, v.gate.generation(), false, cx);
        v.insp.tab = DetailTab::History;
        // Newest first.
        v.store.history_seq(id).map(|(q, _)| q).collect::<Vec<_>>()
    });
    cx.run_until_parked();
    // What the pane renders, and what a palette action would act on.
    let state = |cx: &mut VisualTestContext, text: &'static str| {
        wait(cx, "the formatted payload", |cx| {
            nav.read_with(cx, |v, _| v.insp.full == text)
        });
        nav.read_with(cx, |v, _| {
            (
                v.insp.full.clone(),
                v.palette_target(),
                v.shown().map(|s| s.msg.payload),
            )
        })
    };
    // The older row, then the newer one: equal time and size, so a key that ignores the
    // identity would keep the render of the first.
    click(cx, Box::leak(format!("hist-{}", seqs[1]).into_boxed_str()));
    let (text_a, target_a, shown_a) = state(cx, "aa");
    assert_eq!(&shown_a.expect("shown")[..], b"aa");
    assert_eq!(text_a, "aa");
    nav.update(cx, |v, cx| {
        v.insp.tab = DetailTab::History;
        cx.notify();
    });
    cx.run_until_parked();
    click(cx, Box::leak(format!("hist-{}", seqs[0]).into_boxed_str()));
    let (_, target_b, shown_b) = state(cx, "bb");
    assert_eq!(&shown_b.expect("shown")[..], b"bb");
    assert!(
        target_a != target_b,
        "a palette action must not act on the other message"
    );
    // The equal time must not hide the way back to the latest value. The newest row is the
    // latest message itself, so pick the older row again.
    nav.update(cx, |v, cx| {
        v.insp.tab = DetailTab::History;
        cx.notify();
    });
    cx.run_until_parked();
    click(cx, Box::leak(format!("hist-{}", seqs[1]).into_boxed_str()));
    click(cx, "show-latest");
    let shown = nav.read_with(cx, |v, _| v.shown().expect("shown").msg.payload.clone());
    assert_eq!(&shown[..], b"bb");
    // The History copy of a big latest value is cut. Picking it must offer the full value.
    let big_seq = nav.update(cx, |v, cx| {
        let r = v.store.insert(Incoming {
            generation: 0,
            topic: "big".into(),
            payload: vec![b'x'; 200 * 1024].into(),
            qos: Qos::AtMostOnce,
            retain: false,
            props: MsgProps::default(),
            received_at: std::time::SystemTime::UNIX_EPOCH,
            protocol: Protocol::V5,
        });
        let Insert::Stored(id) = r else {
            panic!("not stored");
        };
        v.select(id, v.gate.generation(), false, cx);
        v.insp.tab = DetailTab::History;
        v.store.history_seq(id).next().expect("entry").0
    });
    cx.run_until_parked();
    // The row is wider than the window (a 120 character preview): click near its left edge.
    let sel: &'static str = Box::leak(format!("hist-{big_seq}").into_boxed_str());
    let at = cx.debug_bounds(sel).expect("History row").origin;
    cx.simulate_click(at + gpui_kit::point(px(20.), px(10.)), Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("show-latest").is_some());
}

/// A key press that a focused button turns into a click: the click comes on the key up, and
/// `simulate_keystrokes` sends the key down only.
fn press(cx: &mut VisualTestContext, key: &'static str) {
    use gpui_kit::{KeyDownEvent, KeyUpEvent, Keystroke};
    let keystroke = Keystroke::parse(key).expect("key").clone();
    cx.simulate_event(KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    cx.simulate_event(KeyUpEvent { keystroke });
    cx.run_until_parked();
}

type Probe = (Option<&'static str>, Vec<&'static str>, [bool; 2], String);

/// What the open connection sheet shows: see `ConnectionSheet::probe`.
fn sheet_probe(cx: &mut VisualTestContext, nav: &Entity<Navigator>) -> Probe {
    let sheet = open_sheet(cx, nav);
    cx.update(|w, cx| sheet.read(cx).probe(w, cx))
}

fn open_sheet(
    cx: &mut VisualTestContext,
    nav: &Entity<Navigator>,
) -> Entity<crate::connections::ConnectionSheet> {
    nav.read_with(cx, |v, _| v.sheet.clone().and_then(|s| s.upgrade()))
        .expect("the sheet is open")
}

/// Opens the sheet with Ctrl+N, types a name, a host and the port of the test Mosquitto (a
/// default port could reach a broker on the machine), and leaves the focus in the port.
fn new_sheet(cx: &mut VisualTestContext, nav: &Entity<Navigator>) {
    cx.simulate_keystrokes("secondary-n");
    cx.run_until_parked();
    cx.simulate_input("n");
    cx.simulate_keystrokes("tab secondary-a");
    cx.simulate_input("localhost");
    cx.simulate_keystrokes("tab secondary-a");
    cx.simulate_input("18830");
    assert_eq!(sheet_probe(cx, nav).0, Some("port"));
}

/// Enter in a text field checks the form like "Save and connect": a wrong Will delay in the
/// closed Advanced section opens it and takes the focus.
#[gpui_kit::test]
fn enter_in_a_field_shows_the_wrong_field_and_focuses_it(cx: &mut TestAppContext) {
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    new_sheet(cx, &nav);
    let sheet = open_sheet(cx, &nav);
    cx.update(|w, cx| sheet.update(cx, |s, cx| s.make_invalid(true, w, cx)));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let (focus, errors, open, _) = sheet_probe(cx, &nav);
    assert_eq!(errors, ["will.delay_secs"]);
    assert_eq!(open, [true, false], "Advanced must open");
    assert_eq!(focus, Some("will.delay_secs"));
}

/// One main key for each platform: Cmd on macOS, Ctrl elsewhere. On Windows and Linux the Cmd
/// key is the Super key, and no shortcut may use it.
#[gpui_kit::test]
fn the_key_bindings_use_the_main_key_of_the_platform(cx: &mut TestAppContext) {
    cx.update(|cx| {
        super::init(cx);
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let mac = cfg!(target_os = "macos");
        let mut mains = 0;
        for b in keymap.bindings() {
            if !b.action().name().starts_with("navigator_ui::") {
                continue;
            }
            let m = b.keystrokes()[0].as_keystroke().modifiers;
            if m.control || m.platform {
                mains += 1;
                assert!(
                    m.platform == mac && m.control != mac,
                    "{} is bound with the wrong main key: {:?}",
                    b.action().name(),
                    b.keystrokes()
                );
            }
        }
        assert!(mains >= 6, "the shortcuts are missing: {mains}");
    });
}

/// The shortcut text is read from the key bindings, so the text and the key cannot differ: the
/// menus, the tooltip, the composer hint and the palette all use it.
#[gpui_kit::test]
fn shortcut_text_comes_from_the_key_bindings(cx: &mut TestAppContext) {
    use super::commands::bound_action;
    use super::{CONTEXT, OpenPalette, shortcut_text};
    let (mut cx, _nav) = open(cx);
    let main = if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl+"
    };
    cx.update(|w, _| {
        assert_eq!(
            shortcut_text(&OpenPalette, Some(CONTEXT), w).as_deref(),
            Some(format!("{main}K").as_str())
        );
        // Every palette command with a key shows its shortcut.
        for e in crate::palette::COMMANDS {
            if let Some((action, context)) = bound_action(e.cmd) {
                let text = shortcut_text(action.as_ref(), context, w);
                assert!(
                    text.is_some_and(|t| t.contains(main)),
                    "no shortcut for {}",
                    e.id
                );
            }
        }
    });
}

/// Tab can focus a user property row that the scroll area hides. The area scrolls to the row.
#[gpui_kit::test]
fn focus_in_a_hidden_user_property_row_scrolls_to_it(cx: &mut TestAppContext) {
    use crate::composer::PropKind;
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    // Properties are for MQTT 5. The saved profile of `open` may use another protocol.
    cx.update(|_, cx| {
        nav.update(cx, |v, _| {
            v.settings.profiles[0].protocol = navigator_core::net::Protocol::V5;
            v.profile = v.settings.profiles.first().cloned();
        })
    });
    cx.simulate_keystrokes("secondary-shift-n");
    for _ in 0..12 {
        cx.update(|w, cx| nav.update(cx, |v, cx| v.add_prop(PropKind::User, w, cx)));
        cx.run_until_parked();
    }
    for _ in 0..3 {
        cx.update(|w, _| w.refresh());
        cx.run_until_parked();
    }
    let offset = |cx: &mut VisualTestContext| nav.read_with(cx, |v, _| v.props_offset());
    assert!(offset(cx) < px(0.), "the area must show the last row");
    cx.update(|_, cx| nav.update(cx, |v, cx| v.reveal_user_row(0, cx)));
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update(|w, _| w.refresh());
    }
    // The Name and Value captions (about 28 px) are above the first row: the least scroll shows the row.
    assert!(
        offset(cx) >= px(-30.),
        "the area must show the focused first row: {:?}",
        offset(cx)
    );
}

/// Any control of a user property row scrolls the row into view when the focus enters the row, not
/// only the inputs: Shift+Tab from "Add property" reaches a control of the last row (the Remove
/// button in a wide pane). A hidden Remove button must not keep the focus (Enter would delete a
/// row out of view).
#[gpui_kit::test]
fn shift_tab_to_a_hidden_remove_button_scrolls_to_its_row(cx: &mut TestAppContext) {
    use crate::composer::PropKind;
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    cx.update(|_, cx| {
        nav.update(cx, |v, _| {
            v.settings.profiles[0].protocol = navigator_core::net::Protocol::V5;
            v.profile = v.settings.profiles.first().cloned();
        })
    });
    // Focus-in events run only in an active window.
    cx.update(|w, _| w.activate_window());
    cx.simulate_keystrokes("secondary-shift-n");
    for _ in 0..12 {
        cx.update(|w, cx| nav.update(cx, |v, cx| v.add_prop(PropKind::User, w, cx)));
        cx.run_until_parked();
    }
    let draw = |cx: &mut VisualTestContext| {
        for _ in 0..3 {
            cx.update(|w, _| w.refresh());
            cx.run_until_parked();
        }
    };
    draw(cx);
    // From the last Value, Tab leaves the row (the order of the controls depends on the layout).
    // Then Shift+Tab comes back into the row from outside. A narrow window stacks the rows.
    for width in [1400., 700.] {
        cx.simulate_resize(size(px(width), px(900.)));
        draw(cx);
        let last = nav.read_with(cx, |v, _| v.last_user_value());
        cx.update(|w, cx| last.update(cx, |i, cx| i.focus(w, cx)));
        let row = nav.read_with(cx, |v, _| v.last_user_row_focus());
        for _ in 0..6 {
            if !cx.update(|w, cx| row.contains_focused(w, cx)) {
                break;
            }
            cx.simulate_keystrokes("tab");
        }
        assert!(
            !cx.update(|w, cx| row.contains_focused(w, cx)),
            "Tab did not leave the last row at {width} px"
        );
        nav.update(cx, |v, _| v.scroll_props_to_top());
        draw(cx);
        assert_eq!(nav.read_with(cx, |v, _| v.props_offset()), px(0.));
        cx.simulate_keystrokes("shift-tab");
        draw(cx);
        assert!(cx.update(|w, cx| row.contains_focused(w, cx)));
        assert!(
            nav.read_with(cx, |v, _| v.props_offset()) < px(0.),
            "the area did not show the last row at {width} px"
        );
        assert!(
            nav.read_with(cx, |v, _| v.last_user_row_in_view()),
            "the last row is not in the viewport at {width} px"
        );
        if width > 1000. {
            // The wide order: Remove is the last control of the row.
            let remove = nav.read_with(cx, |v, _| v.last_user_remove_focus());
            assert!(
                cx.update(|w, _| remove.is_focused(w)),
                "Remove has no focus"
            );
        }
    }
    // A move from Value to "Send empty", and from there to Remove, stays inside the row. The row
    // is scrolled away before each step.
    cx.simulate_resize(size(px(1400.), px(900.)));
    draw(cx);
    let last = nav.read_with(cx, |v, _| v.last_user_value());
    cx.update(|w, cx| last.update(cx, |i, cx| i.focus(w, cx)));
    for step in ["Send empty", "Remove"] {
        nav.update(cx, |v, _| v.scroll_props_to_top());
        draw(cx);
        assert!(!nav.read_with(cx, |v, _| v.last_user_row_in_view()));
        cx.simulate_keystrokes("tab");
        draw(cx);
        assert!(
            nav.read_with(cx, |v, _| v.last_user_row_in_view()),
            "the row is not in view after Tab to {step}"
        );
    }
    let remove = nav.read_with(cx, |v, _| v.last_user_remove_focus());
    assert!(
        cx.update(|w, _| remove.is_focused(w)),
        "Remove has no focus"
    );
    // A new standard field is at the top of the area: the area scrolls there. Payload format has
    // no input to focus.
    for kind in [PropKind::ContentType, PropKind::PayloadFormat] {
        cx.update(|w, cx| nav.update(cx, |v, cx| v.add_prop(PropKind::User, w, cx)));
        draw(cx);
        assert!(nav.read_with(cx, |v, _| v.props_offset()) < px(0.));
        cx.update(|w, cx| nav.update(cx, |v, cx| v.add_prop(kind, w, cx)));
        draw(cx);
        assert_eq!(
            nav.read_with(cx, |v, _| v.props_offset()),
            px(0.),
            "{kind:?}"
        );
    }
}

/// A wrong field below the visible part of the sheet scrolls into view when Enter focuses it.
#[gpui_kit::test]
fn a_wrong_field_below_the_fold_scrolls_into_view(cx: &mut TestAppContext) {
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    new_sheet(cx, &nav);
    let sheet = open_sheet(cx, &nav);
    cx.update(|w, cx| sheet.update(cx, |s, cx| s.make_invalid(true, w, cx)));
    cx.simulate_keystrokes("enter");
    // The test window draws on a refresh only: one more draw for each frame that is needed.
    for _ in 0..2 {
        cx.run_until_parked();
        cx.update(|w, _| w.refresh());
    }
    cx.run_until_parked();
    let field = cx
        .debug_bounds("field:will.delay_secs")
        .expect("the Will delay field is drawn");
    // The title bar of the sheet is above 40 px and the footer is below `window - 60 px`.
    let window = cx.update(|w, _| w.viewport_size());
    assert!(
        field.top() > px(40.) && field.bottom() < window.height - px(60.),
        "the field is outside the visible part of the sheet: {field:?}"
    );
}

/// New text in the password field cancels "Remove the saved password". The box is off while
/// the field has text, so the text never wins in silence.
#[gpui_kit::test]
fn new_password_text_cancels_the_removal_of_the_saved_password(cx: &mut TestAppContext) {
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    let id = nav.read_with(cx, |v, _| v.settings.profiles[0].id);
    cx.update(|w, cx| nav.update(cx, |v, cx| v.open_sheet(Some(id), false, w, cx)));
    cx.run_until_parked();
    let sheet = open_sheet(cx, &nav);
    let boxes = |cx: &mut VisualTestContext| cx.update(|_, cx| sheet.read(cx).removal_boxes());
    let hint = |cx: &mut VisualTestContext| cx.update(|_, cx| sheet.read(cx).password_hint());
    assert_eq!(hint(cx), "Empty: keep the saved password");
    click(cx, "clear-password");
    assert_eq!(boxes(cx), [true, false]);
    assert_eq!(hint(cx), "Empty: remove the saved password");
    cx.update(|w, cx| sheet.update(cx, |s, cx| s.focus_password(w, cx)));
    cx.simulate_input("x");
    assert_eq!(
        boxes(cx),
        [false, false],
        "the new text must cancel the removal"
    );
    // The box is off now. A click on it does nothing.
    click(cx, "clear-password");
    assert_eq!(boxes(cx), [false, false]);
}

/// A certificate without a key blocks Save with TLS: the section opens and "Choose…" has the
/// focus.
#[gpui_kit::test]
fn a_certificate_without_a_key_opens_its_section_and_takes_the_focus(cx: &mut TestAppContext) {
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    new_sheet(cx, &nav);
    let sheet = open_sheet(cx, &nav);
    cx.update(|w, cx| sheet.update(cx, |s, cx| s.make_invalid(false, w, cx)));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let (focus, errors, open, _) = sheet_probe(cx, &nav);
    assert_eq!(errors, ["client_cert"]);
    assert!(open[1], "the Client certificate section must open");
    assert_eq!(focus, Some("client_cert"));
    // The handle belongs to a rendered button: Enter presses it and opens the file prompt.
    press(cx, "enter");
    assert!(
        cx.did_prompt_for_paths(),
        "the focused handle is not the Choose button"
    );
}

/// TCP does not use the certificate: the same draft saves.
#[gpui_kit::test]
fn a_certificate_without_a_key_does_not_block_a_tcp_save(cx: &mut TestAppContext) {
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    new_sheet(cx, &nav);
    let sheet = open_sheet(cx, &nav);
    cx.update(|w, cx| {
        sheet.update(cx, |s, cx| {
            s.make_invalid(false, w, cx);
            s.use_transport(Transport::Tcp, w, cx);
        })
    });
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        nav.read_with(cx, |v, _| v.settings.profiles.iter().any(|p| p.name == "n")),
        "a half certificate blocked a TCP save"
    );
}

/// Enter in a valid form saves the profile and starts the connect (without a broker it ends
/// in a failure; the test checks the save and the start).
#[gpui_kit::test]
fn enter_in_a_valid_form_saves_and_connects(cx: &mut TestAppContext) {
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    new_sheet(cx, &nav);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let saved = nav.read_with(cx, |v, _| {
        v.settings
            .profiles
            .iter()
            .any(|p| p.name == "n" && p.host == "localhost")
    });
    assert!(saved, "Enter did not save the profile");
    assert!(
        nav.read_with(cx, |v, _| v.connecting()
            || v.status.state != ConnState::Disconnected),
        "Enter saved but did not start the connect"
    );
}

/// The rows of the saved connections are in the Tab order. Enter presses the row with the focus
/// and Down moves to the next row.
#[gpui_kit::test]
fn the_connection_rows_work_with_the_keyboard(cx: &mut TestAppContext) {
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    cx.simulate_keystrokes("secondary-n");
    cx.run_until_parked();
    assert_eq!(sheet_probe(cx, &nav).3, "New connection");
    // From the name field: back to the "New connection" row, then to the saved profile.
    cx.simulate_keystrokes("shift-tab shift-tab");
    press(cx, "enter");
    assert_eq!(sheet_probe(cx, &nav).3, "Edit gui round trip");
    cx.simulate_keystrokes("down");
    press(cx, "enter");
    assert_eq!(sheet_probe(cx, &nav).3, "New connection");
}

/// Escape in a menu returns the focus to the button that opened it, not to the window root.
/// The next Tab then continues from the button.
#[gpui_kit::test]
fn escape_in_a_menu_returns_the_focus_to_its_button(cx: &mut TestAppContext) {
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    for button in ["settings", "connection-menu"] {
        click(cx, button);
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        let handle = nav.read_with(cx, |v, _| match button {
            "settings" => v.settings_focus.clone(),
            _ => v.menu_focus.clone(),
        });
        assert!(
            cx.update(|w, _| handle.is_focused(w)),
            "Escape did not return the focus to {button}"
        );
    }
}

/// The same for the menus of the composer: "Composer actions" and "Add property".
#[gpui_kit::test]
fn escape_in_a_composer_menu_returns_the_focus_to_its_button(cx: &mut TestAppContext) {
    use crate::composer::PropKind;
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    cx.update(|_, cx| {
        nav.update(cx, |v, _| {
            v.settings.profiles[0].protocol = navigator_core::net::Protocol::V5;
            v.profile = v.settings.profiles.first().cloned();
        })
    });
    cx.simulate_keystrokes("secondary-shift-n");
    cx.update(|w, cx| nav.update(cx, |v, cx| v.add_prop(PropKind::User, w, cx)));
    cx.run_until_parked();
    cx.update(|w, _| w.refresh());
    cx.run_until_parked();
    let handles = nav.read_with(cx, |v, _| v.menu_buttons_focus());
    for (button, handle) in ["composer-more", "add-prop"].into_iter().zip(handles) {
        // The focus is in the payload, not on the button.
        cx.simulate_keystrokes("secondary-a");
        click(cx, button);
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            cx.update(|w, _| handle.is_focused(w)),
            "Escape did not return the focus to {button}"
        );
    }
}

/// The composer measures its own width and stacks the property rows below the limit: a user row
/// has two lines in a narrow window. A window resize changes the stacking.
#[gpui_kit::test]
fn the_composer_stacks_its_property_rows_from_its_own_width(cx: &mut TestAppContext) {
    use crate::composer::PropKind;
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    cx.update(|_, cx| {
        nav.update(cx, |v, _| {
            v.settings.profiles[0].protocol = navigator_core::net::Protocol::V5;
            v.profile = v.settings.profiles.first().cloned();
        })
    });
    cx.update(|w, _| w.activate_window());
    cx.simulate_keystrokes("secondary-shift-n");
    for _ in 0..20 {
        cx.update(|w, cx| nav.update(cx, |v, cx| v.add_prop(PropKind::User, w, cx)));
        cx.run_until_parked();
    }
    let draw = |cx: &mut VisualTestContext| {
        for _ in 0..6 {
            cx.update(|w, _| w.refresh());
            cx.run_until_parked();
        }
        let rem = cx.update(|w, _| w.rem_size());
        nav.read_with(cx, |v, _| {
            (
                v.rows_stacked(rem),
                v.last_user_row_height(),
                v.last_user_row_in_view(),
            )
        })
    };
    // The focus is in the last row (it is the last row of the area).
    let last = nav.read_with(cx, |v, _| v.last_user_value());
    cx.update(|w, cx| last.update(cx, |i, cx| i.focus(w, cx)));
    let (stacked, wide, in_view) = draw(cx);
    assert!(!stacked && in_view, "a wide window");
    cx.simulate_resize(size(px(700.), px(900.)));
    let (stacked, narrow, in_view) = draw(cx);
    assert!(
        stacked && narrow > wide + px(10.),
        "a narrow window: {narrow:?} and {wide:?}"
    );
    assert!(
        in_view,
        "the row with the focus is not in view after the resize"
    );
    // A lower window gives the properties less height. The row with the focus stays in view.
    cx.simulate_resize(size(px(1400.), px(900.)));
    draw(cx);
    nav.update(cx, |v, _| v.scroll_props_to_top());
    let (_, _, in_view) = draw(cx);
    assert!(!in_view, "the row is scrolled away");
    cx.simulate_resize(size(px(1400.), px(600.)));
    let (_, _, in_view) = draw(cx);
    assert!(
        in_view,
        "the row with the focus is not in view in a low window"
    );
    cx.simulate_resize(size(px(1400.), px(900.)));
    let (stacked, again, in_view) = draw(cx);
    assert!(
        !stacked && again == wide && in_view,
        "a wide window again: {again:?}"
    );
}

/// A result line that wraps takes room from the Properties area while the composer stays at its
/// maximum height. The row with the focus stays in view.
#[gpui_kit::test]
fn a_long_result_line_keeps_the_focused_property_row_in_view(cx: &mut TestAppContext) {
    use crate::composer::PropKind;
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    cx.update(|_, cx| {
        nav.update(cx, |v, _| {
            v.settings.profiles[0].protocol = navigator_core::net::Protocol::V5;
            v.profile = v.settings.profiles.first().cloned();
        })
    });
    cx.update(|w, _| w.activate_window());
    cx.simulate_resize(size(px(960.), px(600.)));
    cx.simulate_keystrokes("secondary-shift-n");
    for _ in 0..20 {
        cx.update(|w, cx| nav.update(cx, |v, cx| v.add_prop(PropKind::User, w, cx)));
        cx.run_until_parked();
    }
    let draw = |cx: &mut VisualTestContext| {
        for _ in 0..6 {
            cx.update(|w, _| w.refresh());
            cx.run_until_parked();
        }
        nav.read_with(cx, |v, _| v.last_user_row_in_view())
    };
    let last = nav.read_with(cx, |v, _| v.last_user_value());
    cx.update(|w, cx| last.update(cx, |i, cx| i.focus(w, cx)));
    assert!(draw(cx), "the last row is in view first");
    let long = "The broker refused the message for a long reason. ".repeat(6);
    nav.update(cx, |v, cx| v.set_composer_error(&long, cx));
    assert!(draw(cx), "the focused row is clipped by the result line");
}

/// A full composer (all the standard properties and many user properties) keeps its action row
/// above the status bar in a small window. The properties scroll inside the composer.
#[gpui_kit::test]
fn a_full_composer_keeps_the_publish_button_above_the_status_bar(cx: &mut TestAppContext) {
    use crate::composer::PropKind;
    let (mut cx, nav) = open(cx);
    let cx = &mut cx;
    cx.update(|_, cx| {
        nav.update(cx, |v, _| {
            v.settings.profiles[0].protocol = navigator_core::net::Protocol::V5;
            v.profile = v.settings.profiles.first().cloned();
        })
    });
    cx.simulate_resize(size(px(960.), px(600.)));
    cx.simulate_keystrokes("secondary-shift-n");
    for kind in [
        PropKind::PayloadFormat,
        PropKind::ContentType,
        PropKind::ResponseTopic,
        PropKind::Correlation,
        PropKind::Expiry,
    ]
    .into_iter()
    .chain([PropKind::User; 20])
    {
        cx.update(|w, cx| nav.update(cx, |v, cx| v.add_prop(kind, w, cx)));
        cx.run_until_parked();
    }
    for _ in 0..3 {
        cx.update(|w, _| w.refresh());
        cx.run_until_parked();
    }
    // The default zoom (16 px rem) and 150% zoom (24 px rem).
    for rem in [16., 24.] {
        // The zoom is the theme font size: a redraw sets the rem size from it.
        cx.update(|_, cx| gpui_kit::component::Theme::global_mut(cx).font_size = px(rem));
        for _ in 0..4 {
            cx.update(|w, _| w.refresh());
            cx.run_until_parked();
        }
        assert_eq!(
            cx.update(|w, _| w.rem_size()),
            px(rem),
            "the zoom is not in use"
        );
        let send = cx
            .debug_bounds("send")
            .expect("the Publish button is drawn");
        let composer = cx.debug_bounds("composer").expect("the composer is drawn");
        let window = cx.update(|w, _| w.viewport_size());
        let status = cx
            .debug_bounds("status-bar")
            .expect("the status bar is drawn");
        // The composer takes 55% of the window at the default zoom, and at 150% zoom the floor
        // (375 px). The status bar is below it.
        let most = if rem == 16. { 0.55 } else { 0.625 };
        assert!(
            composer.size.height <= window.height * most + px(1.)
                && composer.bottom() <= status.top()
                && send.bottom() <= composer.bottom(),
            "the composer is too tall at {rem} px: {composer:?}, Publish {send:?}, window {window:?}"
        );
    }
}
