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
        SendStatus::LostOnDisconnect => "Not sent: the connection closed.".into(),
    }
}

/// True for a status that the user must notice (shown in the danger colour, and with text).
pub fn status_failed(status: &SendStatus) -> bool {
    matches!(
        status,
        SendStatus::Rejected(_) | SendStatus::TimedOut | SendStatus::LostOnDisconnect
    )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::{colour_slot, format_time, group_digits, status_from_result, status_text};
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
            "Not sent: the connection closed."
        );
    }
}
