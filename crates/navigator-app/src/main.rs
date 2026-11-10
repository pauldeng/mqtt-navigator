//! MQTT Navigator desktop app. P1 tracer bullet: connect, subscribe, topic tree, latest value,
//! publish, disconnect.

mod ui;

use gpui_kit::*;
use navigator_core::Qos;
use navigator_core::net::{Profile, Transport};

actions!(navigator, [Quit]);

/// Process start, for the first-frame metric.
pub static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() {
    STARTED.get_or_init(std::time::Instant::now);
    let profile = match parse_args(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{msg}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
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
                cx.new(|cx| ui::Navigator::new(profile, window, cx))
            }) {
                eprintln!("Cannot open the window: {err:#}");
                cx.quit();
            }
        });
}

const USAGE: &str = "Usage: mqtt-navigator [--host HOST] [--port PORT] [--tcp | --ws | --wss] \
[--insecure] [--ca FILE] [--user NAME] [--password PASS] [--sub FILTER[:QOS]]...

TLS on port 8883 is the default. --tcp connects without encryption.";

fn parse_args(args: impl Iterator<Item = String>) -> Result<Profile, String> {
    let mut p = Profile::default();
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
            "--password" => p.password = Some(value("--password")?),
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
    Ok(p)
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro.
    use super::{Profile, Qos, Transport, parse_args};

    fn parse(s: &str) -> Result<Profile, String> {
        parse_args(s.split_whitespace().map(String::from))
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

    #[test]
    fn bad_port_is_rejected() {
        assert!(parse("--port 0").is_err());
        assert!(parse("--port 70000").is_err());
        assert!(parse("--port").is_err());
    }
}
