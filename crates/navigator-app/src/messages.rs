//! Pure helpers for the message list: time text, colour slots, send status text.
//! These functions have no gpui types, so tests can run them.

use std::time::{SystemTime, UNIX_EPOCH};

use navigator_core::net::PublishResult;
use navigator_core::store::SendStatus;

/// Number of colours in the subscription palette (`chart_1`..`chart_5`, then five hues).
pub const PALETTE: usize = 10;

/// Palette slot of a subscription index. The slots repeat after `PALETTE` subscriptions.
pub fn colour_slot(index: usize) -> usize {
    index % PALETTE
}

/// Local time as `hh:mm:ss.mmm`, or `hh:mm:ss` without `millis`. `utc_offset` is in seconds.
pub fn format_time(at: SystemTime, utc_offset: i32, millis: bool) -> String {
    let d = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let day = (d.as_secs() as i64 + i64::from(utc_offset)).rem_euclid(86_400);
    let (h, m, s) = (day / 3600, day / 60 % 60, day % 60);
    if millis {
        format!("{h:02}:{m:02}:{s:02}.{:03}", d.subsec_millis())
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// A number with a space between each group of three digits, for example `1 240`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// The store status that belongs to a result of the net thread.
pub fn status_from_result(result: PublishResult) -> SendStatus {
    match result {
        PublishResult::Sent => SendStatus::Sent,
        PublishResult::Acknowledged => SendStatus::Acknowledged,
        PublishResult::Rejected(r) => SendStatus::Rejected(r),
        PublishResult::TimedOut => SendStatus::TimedOut,
        PublishResult::LostOnDisconnect => SendStatus::LostOnDisconnect,
    }
}

/// Text for a send status. The list row and the composer line use it.
pub fn status_text(status: &SendStatus) -> String {
    match status {
        SendStatus::Sending => "Sending…".into(),
        SendStatus::Sent => "Sent".into(),
        SendStatus::Acknowledged => "Acknowledged".into(),
        SendStatus::Rejected(r) => format!("Rejected: {r}"),
        SendStatus::TimedOut => "No acknowledgement after 10 s.".into(),
        SendStatus::LostOnDisconnect => "Unknown: the connection closed before the result.".into(),
    }
}

/// The result that a sent row shows in the list. A routine result ("Sent", "Acknowledged")
/// shows in the details and in the composer, not on the row.
pub fn row_result(status: &SendStatus) -> Option<String> {
    match status {
        SendStatus::Sent | SendStatus::Acknowledged => None,
        SendStatus::Sending => Some("Sending…".into()),
        SendStatus::Rejected(r) => Some(format!("Rejected: {r}")),
        SendStatus::TimedOut => Some("Timed out".into()),
        SendStatus::LostOnDisconnect => Some("Connection closed".into()),
    }
}

/// The status of a subscription that the broker accepted. `granted` is the QoS of the SUBACK.
pub fn granted_text(granted: navigator_core::Qos) -> String {
    format!("Granted QoS {}", granted.as_u8())
}

/// True for a status that the user must notice (shown in the danger colour, and with text).
pub fn status_failed(status: &SendStatus) -> bool {
    matches!(
        status,
        SendStatus::Rejected(_) | SendStatus::TimedOut | SendStatus::LostOnDisconnect
    )
}

/// The lines of "Connection details": a label and a value for each fact that the CONNACK
/// carries. MQTT 3.1.1 has the session flag only. A property that the broker did not send has
/// no line, except the keep alive, which always has one ("off" when it is 0).
pub fn details_lines(d: &navigator_core::net::ConnectionDetails) -> Vec<(&'static str, String)> {
    let yes_no = |b: bool| if b { "Yes" } else { "No" }.to_owned();
    let mut out = vec![("Session present", yes_no(d.session_present))];
    let Some(v) = &d.v5 else {
        return out;
    };
    if let Some(id) = &v.assigned_client_id {
        out.push(("Assigned client ID", id.clone()));
    }
    let keep_alive = match (v.keep_alive_secs, v.server_keep_alive_secs) {
        (0, _) => "off".to_owned(),
        (n, Some(_)) => format!("{n} s (set by the broker)"),
        (n, None) => format!("{n} s"),
    };
    out.push(("Keep alive", keep_alive));
    out.push(("Receive maximum", v.receive_max.to_string()));
    if let Some(n) = v.max_packet_size {
        out.push((
            "Maximum packet size",
            format!("{} bytes", group_digits(u64::from(n))),
        ));
    }
    out.push(("Maximum QoS", v.max_qos.to_string()));
    out.push(("Retained messages", yes_no(v.retain_available)));
    out.push(("Wildcard subscriptions", yes_no(v.wildcard_subscriptions)));
    out.push(("Shared subscriptions", yes_no(v.shared_subscriptions)));
    out.push(("Subscription identifiers", yes_no(v.subscription_ids)));
    out.push(("Topic alias maximum", v.topic_alias_max.to_string()));
    for (label, value) in [
        ("Server reference", &v.server_reference),
        ("Response information", &v.response_info),
        ("Reason string", &v.reason_string),
    ] {
        if let Some(text) = value {
            out.push((label, text.clone()));
        }
    }
    out
}

/// A short text for the tooltip of the "Options" button of a subscription.
pub fn options_text(o: &navigator_core::net::SubOptions) -> String {
    use navigator_core::net::RetainHandling;
    let handling = match o.retain_handling {
        RetainHandling::OnSubscribe => "Send retained messages at each subscribe",
        RetainHandling::OnNewSubscription => "Send retained messages only for a new subscription",
        RetainHandling::Never => "Do not send retained messages",
    };
    format!(
        "No Local: {}. Retain As Published: {}. {handling}.",
        if o.no_local { "on" } else { "off" },
        if o.retain_as_published { "on" } else { "off" },
    )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::{
        colour_slot, details_lines, format_time, granted_text, group_digits, options_text,
        row_result, status_from_result, status_text,
    };
    use navigator_core::net::PublishResult;
    use navigator_core::store::SendStatus;

    #[test]
    fn time_is_local_with_milliseconds() {
        // 2026-11-11 12:04:01.120 UTC.
        let at = UNIX_EPOCH + Duration::new(1_794_398_641, 120_000_000);
        assert_eq!(format_time(at, 0, true), "12:04:01.120");
        assert_eq!(format_time(at, 0, false), "12:04:01");
        // +13 h (New Zealand summer time) wraps past midnight.
        assert_eq!(format_time(at, 13 * 3600, true), "01:04:01.120");
        // A negative offset wraps back past midnight.
        let early = UNIX_EPOCH + Duration::from_secs(3600);
        assert_eq!(format_time(early, -2 * 3600, false), "23:00:00");
        assert_eq!(format_time(UNIX_EPOCH, 0, true), "00:00:00.000");
    }

    #[test]
    fn colours_repeat_after_the_palette() {
        assert_eq!(colour_slot(0), 0);
        assert_eq!(colour_slot(9), 9);
        assert_eq!(colour_slot(10), 0);
        assert_eq!(colour_slot(23), 3);
    }

    #[test]
    fn digits_group_by_three() {
        for (n, s) in [(0, "0"), (999, "999"), (1000, "1 000"), (1240, "1 240")] {
            assert_eq!(group_digits(n), s);
        }
        assert_eq!(group_digits(12_345_678), "12 345 678");
    }

    #[test]
    fn a_suback_shows_the_granted_qos() {
        assert_eq!(
            granted_text(navigator_core::Qos::AtMostOnce),
            "Granted QoS 0"
        );
        assert_eq!(
            granted_text(navigator_core::Qos::AtLeastOnce),
            "Granted QoS 1"
        );
    }

    #[test]
    fn a_row_shows_a_result_only_when_it_is_not_routine() {
        assert_eq!(row_result(&SendStatus::Sent), None);
        assert_eq!(row_result(&SendStatus::Acknowledged), None);
        assert_eq!(
            row_result(&SendStatus::Sending).as_deref(),
            Some("Sending…")
        );
        assert_eq!(
            row_result(&SendStatus::Rejected("NotAuthorized".into())).as_deref(),
            Some("Rejected: NotAuthorized")
        );
        assert_eq!(
            row_result(&SendStatus::TimedOut).as_deref(),
            Some("Timed out")
        );
        assert_eq!(
            row_result(&SendStatus::LostOnDisconnect).as_deref(),
            Some("Connection closed")
        );
    }

    #[test]
    fn status_text_follows_the_design() {
        assert_eq!(status_text(&SendStatus::Sending), "Sending…");
        assert_eq!(
            status_text(&status_from_result(PublishResult::Rejected(
                "not authorized (0x87)".into()
            ))),
            "Rejected: not authorized (0x87)"
        );
        assert_eq!(
            status_text(&status_from_result(PublishResult::LostOnDisconnect)),
            "Unknown: the connection closed before the result."
        );
    }

    fn v5_details() -> navigator_core::net::V5Details {
        navigator_core::net::V5Details {
            assigned_client_id: None,
            keep_alive_secs: 30,
            server_keep_alive_secs: None,
            receive_max: 20,
            max_packet_size: None,
            max_qos: 1,
            retain_available: true,
            wildcard_subscriptions: true,
            shared_subscriptions: false,
            subscription_ids: true,
            topic_alias_max: 0,
            server_reference: None,
            response_info: None,
            reason_string: None,
        }
    }

    fn details(
        v5: Option<navigator_core::net::V5Details>,
    ) -> navigator_core::net::ConnectionDetails {
        navigator_core::net::ConnectionDetails {
            protocol: if v5.is_some() {
                navigator_core::net::Protocol::V5
            } else {
                navigator_core::net::Protocol::V311
            },
            session_present: true,
            v5,
        }
    }

    #[test]
    fn mqtt_311_details_show_the_session_flag_only() {
        assert_eq!(
            details_lines(&details(None)),
            [("Session present", "Yes".to_owned())]
        );
    }

    #[test]
    fn details_show_keep_alive_off_when_it_is_0() {
        for (keep, server, want) in [
            (0, Some(0), "off"),
            (0, None, "off"),
            (30, None, "30 s"),
            (15, Some(15), "15 s (set by the broker)"),
        ] {
            let mut v = v5_details();
            (v.keep_alive_secs, v.server_keep_alive_secs) = (keep, server);
            let lines = details_lines(&details(Some(v)));
            let line = lines
                .iter()
                .find(|(l, _)| *l == "Keep alive")
                .expect("line");
            assert_eq!(line.1, want, "keep {keep} server {server:?}");
        }
    }

    #[test]
    fn details_list_each_connack_fact_once_and_skip_absent_ones() {
        let mut v = v5_details();
        assert_eq!(
            details_lines(&details(Some(v.clone()))),
            [
                ("Session present", "Yes".to_owned()),
                ("Keep alive", "30 s".to_owned()),
                ("Receive maximum", "20".to_owned()),
                ("Maximum QoS", "1".to_owned()),
                ("Retained messages", "Yes".to_owned()),
                ("Wildcard subscriptions", "Yes".to_owned()),
                ("Shared subscriptions", "No".to_owned()),
                ("Subscription identifiers", "Yes".to_owned()),
                ("Topic alias maximum", "0".to_owned()),
            ]
        );
        v.assigned_client_id = Some("srv-1".into());
        v.max_packet_size = Some(1_048_576);
        v.server_reference = Some("alt:1883".into());
        v.response_info = Some("resp/".into());
        v.reason_string = Some("hi".into());
        let lines = details_lines(&details(Some(v)));
        assert_eq!(lines[1], ("Assigned client ID", "srv-1".to_owned()));
        assert!(lines.contains(&("Maximum packet size", "1 048 576 bytes".to_owned())));
        assert_eq!(
            lines[lines.len() - 3..],
            [
                ("Server reference", "alt:1883".to_owned()),
                ("Response information", "resp/".to_owned()),
                ("Reason string", "hi".to_owned()),
            ]
        );
    }

    #[test]
    fn the_options_tooltip_names_each_option() {
        use navigator_core::net::{RetainHandling, SubOptions};
        assert_eq!(
            options_text(&SubOptions::default()),
            "No Local: off. Retain As Published: off. Send retained messages at each subscribe."
        );
        assert_eq!(
            options_text(&SubOptions {
                no_local: true,
                retain_as_published: true,
                retain_handling: RetainHandling::Never
            }),
            "No Local: on. Retain As Published: on. Do not send retained messages."
        );
    }
}
