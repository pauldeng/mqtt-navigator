//! End to end: the tests use only the window. They connect, subscribe, publish and read the
//! messages back from the list, through a real broker (`tests/broker/up.sh mosquitto`).

use std::time::{Duration, Instant};

use gpui_kit::component::input::InputState;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, Entity, TestAppContext, WindowHandle, base::Root, px, size};
use navigator_core::net::{Profile, Transport};
use navigator_core::settings::{Load, Settings};
use navigator_core::store::{Direction, RowView};
use uuid::Uuid;

use super::{ConnState, Navigator, SubState};

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
    cx: &mut TestAppContext,
    win: WindowHandle<Root>,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    cx.executor().allow_parking();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        // Draw the frame, as the window does.
        cx.update_window(win.into(), |_, window, cx| window.render_frame(cx))
            .expect("window");
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

/// Opens the composer with Ctrl+Shift+N, fills it and clicks Send. The shortcut focuses the
/// topic while it is empty, and the payload after that: then Shift+Tab goes back to the topic.
fn publish(cx: &mut TestAppContext, win: WindowHandle<Root>, topic: &str, text: &str, first: bool) {
    cx.update_window(win.into(), |_, window, cx| {
        window.press("ctrl-shift-n", cx);
        if !first {
            window.press("shift-tab", cx);
        }
        window.press("ctrl-a", cx);
        window.input(topic, cx);
        // Tab moves the focus from the topic to the payload.
        window.press("tab", cx);
        window.press("ctrl-a", cx);
        window.input(text, cx);
        window.click("send", cx);
    })
    .expect("window");
}

/// Types a filter in the input of the left pane, presses Enter and waits for the SUBACK.
fn subscribe(cx: &mut TestAppContext, win: WindowHandle<Root>, nav: &Entity<Navigator>, f: &str) {
    let input: Entity<InputState> = cx.read_entity(nav, |v, _| v.sub_input.clone());
    cx.update_window(win.into(), |_, w, cx| {
        input.update(cx, |i, cx| i.focus(w, cx));
        w.input(f, cx);
        w.press("enter", cx);
    })
    .expect("window");
    wait(cx, win, &format!("SUBACK for {f}"), |cx| {
        cx.read_entity(nav, |v, _| {
            v.subs
                .rows
                .iter()
                .any(|s| &*s.filter == f && matches!(s.state, SubState::Active(_)))
        })
    });
}

/// Opens the window with a saved profile for the test Mosquitto, and clicks Connect.
fn start(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<Navigator>) {
    // An in-memory keyring: the test never reads the keyring of the user.
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
    let load = Load {
        settings: Settings {
            last_profile_id: Some(profile.id),
            profiles: vec![profile],
            ..Settings::default()
        },
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
        (win.downcast::<Root>().expect("root"), nav)
    });
    cx.update_window(win.into(), |_, w, cx| {
        w.render_frame(cx);
        w.click("connect", cx);
    })
    .expect("window");
    wait(cx, win, "connection", |cx| {
        cx.read_entity(&nav, |v, _| v.status.state == ConnState::Connected)
    });
    (win, nav)
}

#[gpui_kit::test]
fn a_message_goes_through_the_broker_and_comes_back_as_received(cx: &mut TestAppContext) {
    let (win, nav) = start(cx);

    // Control: a message to hello/world before any subscription. It must never come back.
    let control = format!("control-{}", Uuid::new_v4());
    publish(cx, win, "hello/world", &control, true);

    subscribe(cx, win, &nav, "hello/world");
    let text = format!("hello-{}", Uuid::new_v4());
    publish(cx, win, "hello/world", &text, false);

    // The broker sends it back: a Received row. Our own Sent row is there too.
    wait(cx, win, &format!("Received row {text:?}"), |cx| {
        cx.read_entity(&nav, |v, _| {
            rows(v).contains(&(Direction::Received, "hello/world".into(), text.clone()))
        })
    });
    let all = cx.read_entity(&nav, |v, _| rows(v));
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
}

/// The Received rows: topic and payload.
fn received(cx: &mut TestAppContext, nav: &Entity<Navigator>) -> Vec<(String, String)> {
    cx.read_entity(nav, |v, _| {
        rows(v)
            .into_iter()
            .filter(|r| r.0 == Direction::Received)
            .map(|r| (r.1, r.2))
            .collect()
    })
}

#[gpui_kit::test]
fn wildcard_subscriptions_match_by_level_and_show_only_follows_the_row(cx: &mut TestAppContext) {
    let (win, nav) = start(cx);
    let p = format!("wild-{}", Uuid::new_v4());
    let (plus, hash) = (format!("{p}/+/temp"), format!("{p}/#"));
    subscribe(cx, win, &nav, &plus);
    subscribe(cx, win, &nav, &hash);

    // `+` is one level: `a/temp` has two levels between P and temp, so only `#` takes it.
    let sent: Vec<(String, String)> = ["room1/temp", "room1/hum", "room1/a/temp"]
        .into_iter()
        .map(|t| (format!("{p}/{t}"), format!("v-{}", Uuid::new_v4())))
        .collect();
    for (i, (topic, text)) in sent.iter().enumerate() {
        publish(cx, win, topic, text, i == 0);
    }
    // Mosquitto sends one copy for each matching subscription (the filters overlap on
    // `room1/temp`), and the app shows each copy: 2 + 1 + 1 Received rows.
    wait(cx, win, "4 Received rows", |cx| {
        received(cx, &nav).len() >= 4
    });
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

    // The row of `room1/temp` has the colour of the first matching subscription: `+/temp`, row 0.
    let colours: Vec<Option<usize>> = cx.update_entity(&nav, |v, _| {
        sent.iter()
            .map(|(t, _)| v.store.colour_of_topic(t))
            .collect()
    });
    assert_eq!(colours, [Some(0), Some(1), Some(1)]);

    // Click the row of a subscription: Show only. Then the rows follow the filter.
    cx.update_window(win.into(), |_, w, cx| {
        w.click(gpui_kit::SharedString::from(plus.clone()), cx)
    })
    .expect("window");
    wait(cx, win, "Show only +/temp", |cx| {
        received(cx, &nav).len() == 2
    });
    let got = received(cx, &nav);
    assert!(got.iter().all(|r| *r == sent[0]), "{got:?}");
    cx.update_window(win.into(), |_, w, cx| {
        w.click(gpui_kit::SharedString::from(hash.clone()), cx)
    })
    .expect("window");
    wait(cx, win, "Show only #", |cx| received(cx, &nav).len() == 4);
}
