//! MQTT Navigator desktop app. P1 tracer bullet: connect, subscribe, topic tree, latest value,
//! publish, disconnect.

mod composer;
mod connections;
#[cfg(target_os = "linux")]
mod desktop_entry;
mod diagnostics;
mod inspector;
mod logging;
mod messages;
mod palette;
mod persist;
mod secrets;
mod themes;
mod ui;

use gpui_kit::*;
use navigator_core::Qos;
use navigator_core::net::{ConnectConfig, Transport};
use navigator_core::settings::{self, Load};

actions!(navigator, [Quit]);

/// The Linux app id (Wayland `app_id`, X11 class). The desktop entry and the icons have this name.
const APP_ID: &str = "io.github.pauldeng.mqtt-navigator";

/// Process start, for the first-frame metric.
pub static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() {
    STARTED.get_or_init(std::time::Instant::now);
    logging::init();
    let cli = match cli_session(
        std::env::args().skip(1),
        std::env::var(PASSWORD_ENV).ok(),
        &mut std::io::stdin().lock(),
    ) {
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
            ui::init(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);
            // The settings load in the background. The window opens when they are here, so that
            // the first frame has the saved theme and pane widths.
            let path = persist::settings_path();
            let task = cx.background_executor().spawn(async move {
                let load = match path {
                    Some(path) => {
                        // Our own settings folder: only the owner may use it (Unix).
                        if let Some(dir) = path.parent() {
                            let _ = navigator_core::private_fs::tighten_dir(dir);
                        }
                        persist::remove_stale_tmp(&path);
                        settings::load(&path)
                    }
                    None => Load {
                        settings: settings::Settings::default(),
                        notice: Some(
                            "The system has no settings folder. Changes are not saved.".into(),
                        ),
                        read_only: true,
                    },
                };
                Prepared {
                    load,
                    // About 1 ms. It stays off the UI thread, before the window opens.
                    #[cfg(target_os = "linux")]
                    icon: desktop_entry::window_icon(),
                }
            });
            cx.spawn(async move |cx| {
                let prepared = task.await;
                cx.update(|cx| open(cli, prepared, cx));
            })
            .detach();
        });
}

/// What the background task makes before the window opens.
struct Prepared {
    load: Load,
    /// X11 window icon. Wayland, Windows and macOS take the icon from the system (see
    /// `docs/release.md`).
    #[cfg(target_os = "linux")]
    icon: Option<std::sync::Arc<image::RgbaImage>>,
}

fn open(cli: Option<ConnectConfig>, prepared: Prepared, cx: &mut App) {
    let Prepared {
        load,
        #[cfg(target_os = "linux")]
        icon,
    } = prepared;
    let ui = &load.settings.ui;
    themes::init(&ui.theme_preset, ui.theme_mode, cx);
    #[allow(unused_mut)]
    let mut options = WindowOptions {
        window_min_size: Some(size(px(960.), px(600.))),
        // Linux: the desktop entry has this name and this StartupWMClass.
        app_id: Some(APP_ID.into()),
        ..gpui_kit::component::TitleBar::window_options()
    };
    #[cfg(target_os = "linux")]
    {
        options.icon = icon;
    }
    if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
        // Linux without a package: add the app to the user's app menu, so that the dock shows
        // the icon. It runs in the background, after the first frame.
        #[cfg(target_os = "linux")]
        window.on_next_frame(|_, cx| {
            cx.background_executor()
                .spawn(async { desktop_entry::ensure_user_entry() })
                .detach();
        });
        cx.new(|cx| ui::Navigator::new(cli, load, window, cx))
    }) {
        eprintln!("Cannot open the window: {err:#}");
        cx.quit();
    }
}

const USAGE: &str = "Usage: mqtt-navigator [--host HOST] [--port PORT] [--tcp | --ws | --wss] \
[--insecure] [--ca FILE] [--user NAME] [--password-stdin] [--sub FILTER[:QOS]]...

TLS on port 8883 is the default. --tcp connects without encryption.
--password-stdin reads the password from the first line of the standard input.
The environment variable MQTT_NAVIGATOR_PASSWORD also sets it (less safe).";

/// The environment variable for the password. Other programs of the same user can read the
/// environment of a process, so `--password-stdin` is the safer way.
const PASSWORD_ENV: &str = "MQTT_NAVIGATOR_PASSWORD";

const PASSWORD_ARG_TEXT: &str = "--password is not supported: the password would show in the process list and in the shell history. Use --password-stdin, or set MQTT_NAVIGATOR_PASSWORD.";

/// The name of the session profile that the command line makes.
const SESSION_NAME: &str = "Command line";

/// With arguments, the command line makes an unsaved session profile "Command line". Its
/// password stays in memory (`ConnectConfig`). The app never writes the profile or the password
/// to the settings file or the keyring. Without arguments there is no session profile.
///
/// The password comes from the first line of `stdin` (`--password-stdin`; no line ending, an
/// empty line means no password) or else from `env_password`. Never from an argument.
fn cli_session(
    args: impl Iterator<Item = String>,
    env_password: Option<String>,
    stdin: &mut dyn std::io::BufRead,
) -> Result<Option<ConnectConfig>, String> {
    let args: Vec<String> = args.collect();
    if args.is_empty() {
        return Ok(None);
    }
    let (mut config, from_stdin) = parse_args(args.into_iter())?;
    config.password = if from_stdin {
        read_password(stdin)?
    } else {
        env_password.filter(|p| !p.is_empty())
    };
    config.profile.name = SESSION_NAME.into();
    // A value that is too long makes the MQTT library panic at the connect.
    config.validate()?;
    Ok(Some(config))
}

/// One line from `stdin`. A password never has a line ending. The read stops at a size that
/// `ConnectConfig::validate` rejects.
fn read_password(stdin: &mut dyn std::io::BufRead) -> Result<Option<String>, String> {
    use std::io::{BufRead as _, Read as _};
    let mut line = String::new();
    stdin
        .take(70_000)
        .read_line(&mut line)
        .map_err(|e| format!("Cannot read the password from the standard input: {e}."))?;
    let line = line.trim_end_matches(['\n', '\r']);
    Ok((!line.is_empty()).then(|| line.to_owned()))
}

/// The config and whether `--password-stdin` is set.
fn parse_args(args: impl Iterator<Item = String>) -> Result<(ConnectConfig, bool), String> {
    let mut c = ConnectConfig::from(navigator_core::net::Profile::default());
    let p = &mut c.profile;
    let mut port = None;
    let mut subs = Vec::new();
    let mut password_stdin = false;
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
            "--password-stdin" => password_stdin = true,
            // Also `--password=secret`: say nothing about what follows the name.
            a if a.starts_with("--password") => return Err(PASSWORD_ARG_TEXT.into()),
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
            // Print only the name: a value after `=` can be a secret.
            other => {
                let name = other.split_once('=').map_or(other, |(n, _)| n);
                return Err(format!("Unknown option: {name}"));
            }
        }
    }
    p.port = port.unwrap_or(p.transport.default_port());
    if !subs.is_empty() {
        p.subscriptions = subs;
    }
    Ok((c, password_stdin))
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro.
    use super::{ConnectConfig, Qos, Transport, cli_session, parse_args};

    fn parse(s: &str) -> Result<navigator_core::net::Profile, String> {
        parse_args(s.split_whitespace().map(String::from))
            .map(|(c, _): (ConnectConfig, bool)| c.profile)
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
        session_with(s, None, "")
    }

    fn session_with(
        s: &str,
        env: Option<&str>,
        stdin: &str,
    ) -> Result<Option<ConnectConfig>, String> {
        cli_session(
            s.split_whitespace().map(String::from),
            env.map(String::from),
            &mut stdin.as_bytes(),
        )
    }

    #[test]
    fn no_arguments_make_no_session_profile() {
        assert!(session("").expect("valid").is_none());
    }

    #[test]
    fn arguments_make_an_unsaved_command_line_profile() {
        let c = session_with(
            "--host b.example --tcp --user u --password-stdin",
            None,
            "secret\n",
        )
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
        let pass = session_with("--password-stdin", None, &long).expect_err("password");
        assert!(user.contains("user name"), "{user}");
        assert!(
            pass.contains("password") && !pass.contains("aaaa"),
            "{pass}"
        );
        assert!(session(&format!("--user {}", "a".repeat(65_535))).is_ok());
    }

    fn password(s: &str, env: Option<&str>, stdin: &str) -> Option<String> {
        session_with(s, env, stdin)
            .expect("valid")
            .expect("a session")
            .password
    }

    #[test]
    fn password_argument_is_refused_and_not_echoed() {
        let e = session("--tcp --password hunter2").expect_err("refused");
        assert!(
            e.contains("--password-stdin") && !e.contains("hunter2"),
            "{e}"
        );
        let e = parse("--password").expect_err("refused without a value too");
        assert!(e.contains("--password-stdin"), "{e}");
        for arg in [
            "--password=hunter2",
            "--passwordhunter2",
            "--password-stdin=hunter2",
        ] {
            let e = parse(arg).expect_err("refused");
            assert!(
                e.contains("--password-stdin") && !e.contains("hunter2"),
                "{arg}: {e}"
            );
        }
    }

    #[test]
    fn unknown_option_error_never_prints_a_value() {
        let e = parse("--bogus=secret").expect_err("unknown");
        assert!(
            e.starts_with("Unknown option: --bogus") && !e.contains("secret"),
            "{e}"
        );
        let e = parse("--host=secret").expect_err("unknown");
        assert!(e.contains("--host") && !e.contains("secret"), "{e}");
    }

    #[test]
    fn password_from_stdin_loses_the_line_ending_and_keeps_spaces() {
        let p = |stdin| password("--tcp --password-stdin", None, stdin);
        assert_eq!(p("s3cret\n").as_deref(), Some("s3cret"));
        assert_eq!(p("s3cret\r\n").as_deref(), Some("s3cret"));
        assert_eq!(p("s3cret").as_deref(), Some("s3cret"));
        assert_eq!(p(" a b \nsecond line\n").as_deref(), Some(" a b "));
        assert_eq!(p("\n"), None);
        assert_eq!(p(""), None);
    }

    #[test]
    fn password_from_the_environment_and_stdin_wins() {
        assert_eq!(
            password("--tcp", Some("fromenv"), "").as_deref(),
            Some("fromenv")
        );
        assert_eq!(password("--tcp", Some(""), ""), None);
        assert_eq!(password("--tcp", None, "ignored\n"), None);
        assert_eq!(
            password("--tcp --password-stdin", Some("fromenv"), "fromstdin\n").as_deref(),
            Some("fromstdin")
        );
        // An empty line on stdin means no password, also when the environment has one.
        assert_eq!(
            password("--tcp --password-stdin", Some("fromenv"), "\n"),
            None
        );
    }

    #[test]
    fn the_environment_alone_makes_no_session() {
        assert!(session_with("", Some("x"), "").expect("valid").is_none());
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
