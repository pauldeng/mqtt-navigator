//! End to end: the tests use only the window. They connect, subscribe, publish and read the
//! messages back from the list, through a real broker (`tests/broker/up.sh mosquitto`).

use std::time::{Duration, Instant};

#[cfg(feature = "broker-tests")]
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::InputState;
use gpui_kit::{AppContext, Entity, Modifiers, TestAppContext, VisualTestContext, px, size};
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
    cx.simulate_keystrokes("ctrl-shift-n");
    if !first {
        cx.simulate_keystrokes("shift-tab");
    }
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input(topic);
    // Tab moves the focus from the topic to the payload.
    cx.simulate_keystrokes("tab ctrl-a");
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

/// The palette list is a snapshot. A "Disconnect" chosen after the connection dropped must not
/// start a connection: the action is checked again when it runs.
#[cfg(feature = "broker-tests")]
#[gpui_kit::test]
fn a_palette_action_chosen_after_the_state_changed_does_not_run(cx: &mut TestAppContext) {
    let (mut cx, nav) = start(cx);
    let cx = &mut cx;
    cx.simulate_keystrokes("ctrl-k");
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
