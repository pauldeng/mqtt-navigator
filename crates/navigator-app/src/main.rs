//! MQTT Navigator desktop app. P1 tracer bullet: connect, subscribe, topic tree, latest value,
//! publish, disconnect.

mod composer;
mod connections;
mod diagnostics;
mod inspector;
mod logging;
mod messages;
mod palette;
mod persist;
mod secrets;
mod ui;

use std::time::Duration;

use gpui_kit::component::Theme;
use gpui_kit::component::input::CursorBlink;
use gpui_kit::*;
use navigator_core::Qos;
use navigator_core::net::{ConnectConfig, Transport};

actions!(navigator, [Quit]);

/// Process start, for the first-frame metric.
pub static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() {
    STARTED.get_or_init(std::time::Instant::now);
    logging::init();
    let cli = match cli_session(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // The default caret blinks every 500 ms for as long as an input has focus. This one
            // stops after 5 s without typing, so a focused idle window sleeps. A theme change
            // keeps the policy (`Theme::change` does not touch `cursor_blink`).
            Theme::update(cx, |theme| {
                theme.cursor_blink = CursorBlink::blinking(Duration::from_millis(500))
                    .with_idle_timeout(Duration::from_secs(5));
            });
            ui::init(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);
            let options = WindowOptions {
                window_min_size: Some(size(px(960.), px(600.))),
                ..gpui_kit::component::TitleBar::window_options()
            };
            if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| ui::Navigator::new(cli, window, cx))
            }) {
                eprintln!("Cannot open the window: {err:#}");
                cx.quit();
            }
        });
}

const USAGE: &str = "Usage: mqtt-navigator [--host HOST] [--port PORT] [--tcp | --ws | --wss] \
[--insecure] [--ca FILE] [--user NAME] [--password PASS] [--sub FILTER[:QOS]]...

TLS on port 8883 is the default. --tcp connects without encryption.";

/// The name of the session profile that the command line makes.
const SESSION_NAME: &str = "Command line";

/// With arguments, the command line makes an unsaved session profile "Command line". Its
/// password stays in memory (`ConnectConfig`). The app never writes the profile or the password
/// to the settings file or the keyring. Without arguments there is no session profile.
fn cli_session(args: impl Iterator<Item = String>) -> Result<Option<ConnectConfig>, String> {
    let args: Vec<String> = args.collect();
    if args.is_empty() {
        return Ok(None);
    }
    let mut config = parse_args(args.into_iter())?;
    config.profile.name = SESSION_NAME.into();
    // A value that is too long makes the MQTT library panic at the connect.
    config.validate()?;
    Ok(Some(config))
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<ConnectConfig, String> {
    let mut c = ConnectConfig::from(navigator_core::net::Profile::default());
    let p = &mut c.profile;
    let mut port = None;
    let mut subs = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value."));
        match arg.as_str() {
            "--host" => p.host = value("--host")?,
            "--port" => {
                let v = value("--port")?;
                port = Some(
                    v.parse::<u16>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("Use a port from 1 to 65535.")?,
                );
            }
            "--tcp" => p.transport = Transport::Tcp,
            "--ws" => p.transport = Transport::Ws,
            "--wss" => p.transport = Transport::Wss,
            "--insecure" => p.verify_tls = false,
            "--ca" => p.ca_file = Some(value("--ca")?.into()),
            "--user" => p.username = Some(value("--user")?),
            "--password" => c.password = Some(value("--password")?),
            "--sub" => {
                let v = value("--sub")?;
                let (filter, qos) = match v.rsplit_once(':') {
                    Some((f, "0")) => (f.to_string(), Qos::AtMostOnce),
                    Some((f, "1")) => (f.to_string(), Qos::AtLeastOnce),
                    _ => (v, Qos::AtMostOnce),
                };
                subs.push((filter, qos));
            }
            "-h" | "--help" => return Err("MQTT Navigator".into()),
            other => return Err(format!("Unknown option: {other}")),
        }
    }
    p.port = port.unwrap_or(p.transport.default_port());
    if !subs.is_empty() {
        p.subscriptions = subs;
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro.
    use super::{ConnectConfig, Qos, Transport, cli_session, parse_args};

    fn parse(s: &str) -> Result<navigator_core::net::Profile, String> {
        parse_args(s.split_whitespace().map(String::from)).map(|c: ConnectConfig| c.profile)
    }

    #[test]
    fn defaults_to_tls_on_8883() {
        let p = parse("").expect("valid");
        assert_eq!(
            (p.transport, p.port, p.verify_tls),
            (Transport::Tls, 8883, true)
        );
    }

    #[test]
    fn tcp_uses_1883_and_subs_parse() {
        let p = parse("--tcp --sub a/#:1 --sub b").expect("valid");
        assert_eq!(p.port, 1883);
        assert_eq!(
            p.subscriptions,
            vec![
                ("a/#".into(), Qos::AtLeastOnce),
                ("b".into(), Qos::AtMostOnce)
            ]
        );
    }

    fn session(s: &str) -> Result<Option<ConnectConfig>, String> {
        cli_session(s.split_whitespace().map(String::from))
    }

    #[test]
    fn no_arguments_make_no_session_profile() {
        assert!(session("").expect("valid").is_none());
    }

    #[test]
    fn arguments_make_an_unsaved_command_line_profile() {
        let c = session("--host b.example --tcp --user u --password secret")
            .expect("valid")
            .expect("a session");
        let p = &c.profile;
        assert_eq!(p.name, "Command line");
        assert_eq!((p.host.as_str(), p.port), ("b.example", 1883));
        assert_eq!(p.transport, Transport::Tcp);
        assert_eq!(p.username.as_deref(), Some("u"));
        // The password is in the connect config only. It is not in the profile, so it cannot
        // reach the settings file.
        assert_eq!(c.password.as_deref(), Some("secret"));
        assert!(!format!("{p:?}").contains("secret"));
    }

    #[test]
    fn too_long_values_are_rejected_with_a_plain_message() {
        let long = "a".repeat(65_536);
        let user = session(&format!("--user {long}")).expect_err("user");
        let pass = session(&format!("--password {long}")).expect_err("password");
        assert!(user.contains("user name"), "{user}");
        assert!(
            pass.contains("password") && !pass.contains("aaaa"),
            "{pass}"
        );
        assert!(session(&format!("--user {}", "a".repeat(65_535))).is_ok());
    }

    #[test]
    fn a_bad_argument_is_an_error() {
        assert!(session("--port 0").is_err());
        assert!(session("--nope").is_err());
    }

    #[test]
    fn bad_port_is_rejected() {
        assert!(parse("--port 0").is_err());
        assert!(parse("--port 70000").is_err());
        assert!(parse("--port").is_err());
    }
}
