//! Checks for an outgoing message, and the hex helpers of the composer. Pure functions.

use crate::model::Qos;
use crate::net::Outgoing;
use crate::topic::MAX_LEN;

/// Largest MQTT "remaining length": 4 bytes of variable length integer.
const MAX_REMAINING: usize = 268_435_455;

/// Checks a message before it goes to the broker. Returns text for the user on the first error.
///
/// - Topic name: 1 to 65 535 bytes, no wildcard (`+`, `#`), no NUL.
/// - Content type, response topic (also a valid topic name), user property keys and values:
///   MQTT strings (at most 65 535 bytes, no NUL).
/// - Correlation data: at most 65 535 bytes.
/// - Payload format "UTF-8": the payload must be valid UTF-8.
/// - The encoded PUBLISH packet ([`encoded_size`]) must fit the MQTT limit (256 MiB) and
///   `max_packet`, the broker's Maximum Packet Size from the CONNACK, when it is known.
///
/// The control characters U+0001 to U+001F and U+007F to U+009F are allowed: MQTT only
/// recommends against them.
pub fn validate_outgoing(o: &Outgoing, max_packet: Option<u32>) -> Result<(), String> {
    mqtt_str(&o.topic, "The topic")?;
    if o.topic.is_empty() {
        return Err("The topic is empty.".into());
    }
    if o.topic.contains(['+', '#']) {
        return Err("The topic has a wildcard (+ or #). Publish to a topic name.".into());
    }
    let p = &o.props;
    if let Some(ct) = &p.content_type {
        mqtt_str(ct, "The content type")?;
    }
    if let Some(rt) = &p.response_topic {
        mqtt_str(rt, "The response topic")?;
        if rt.is_empty() {
            return Err("The response topic is empty.".into());
        }
        if rt.contains(['+', '#']) {
            return Err("The response topic has a wildcard (+ or #).".into());
        }
    }
    if let Some(c) = &p.correlation_data
        && c.len() > MAX_LEN
    {
        return Err(format!(
            "The correlation data has {} bytes. The limit is {MAX_LEN}.",
            c.len()
        ));
    }
    for (k, v) in &p.user {
        mqtt_str(k, "A user property name")?;
        mqtt_str(v, "A user property value")?;
    }
    if p.payload_utf8
        && let Err(e) = std::str::from_utf8(&o.payload)
    {
        return Err(format!(
            "The payload format is UTF-8, but the payload is not (first error at byte {}).",
            e.valid_up_to()
        ));
    }
    let size = encoded_size(o);
    if size > MAX_REMAINING + 5 {
        return Err("The message is larger than the MQTT limit (256 MiB).".into());
    }
    if let Some(max) = max_packet
        && size > max as usize
    {
        return Err(format!(
            "The message needs {size} bytes. The broker accepts {max} bytes at most."
        ));
    }
    Ok(())
}

fn mqtt_str(s: &str, what: &str) -> Result<(), String> {
    if s.len() > MAX_LEN {
        return Err(format!(
            "{what} has {} bytes. The limit is {MAX_LEN}.",
            s.len()
        ));
    }
    if s.contains('\0') {
        return Err(format!("{what} has a NUL character."));
    }
    Ok(())
}

/// Size of a variable length integer, in bytes.
fn varint(n: usize) -> usize {
    match n {
        0..=127 => 1,
        128..=16_383 => 2,
        16_384..=2_097_151 => 3,
        _ => 4,
    }
}

/// Size of the MQTT 5 PUBLISH packet for `o` on the wire, in bytes. It uses no topic alias.
/// The estimate is exact for valid input.
pub fn encoded_size(o: &Outgoing) -> usize {
    let p = &o.props;
    let s = |v: &Option<String>| v.as_ref().map_or(0, |v| 3 + v.len());
    let props = usize::from(p.payload_utf8) * 2
        + usize::from(p.message_expiry.is_some()) * 5
        + s(&p.content_type)
        + s(&p.response_topic)
        + p.correlation_data.as_ref().map_or(0, |c| 3 + c.len())
        + p.user
            .iter()
            .map(|(k, v)| 5 + k.len() + v.len())
            .sum::<usize>();
    let id = if o.qos == Qos::AtLeastOnce { 2 } else { 0 };
    let remaining = 2 + o.topic.len() + id + varint(props) + props + o.payload.len();
    1 + varint(remaining) + remaining
}

/// Reads hex bytes: pairs of hex digits, with white space (spaces, tabs, new lines) anywhere
/// (white space is ignored, so `a b` is the byte `ab`).
/// An empty text gives no bytes.
pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut high: Option<u8> = None;
    for (i, c) in s.char_indices() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let d = c
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
            .filter(|_| c.is_ascii())
            .ok_or_else(|| format!("'{c}' at position {i} is not a hex digit."))?;
        match high.take() {
            Some(h) => out.push(h << 4 | d),
            None => high = Some(d),
        }
    }
    if high.is_some() {
        return Err("The number of hex digits is odd. Each byte needs 2 digits.".into());
    }
    Ok(out)
}

/// Bytes as lowercase hex pairs, separated by one space. [`parse_hex`] reads it back.
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::model::MsgProps;

    fn msg(topic: &str) -> Outgoing {
        Outgoing {
            op_id: 1,
            topic: topic.into(),
            payload: Bytes::from_static(b"hi"),
            qos: Qos::AtMostOnce,
            retain: false,
            props: MsgProps::default(),
        }
    }

    fn err(o: &Outgoing, max: Option<u32>) -> String {
        validate_outgoing(o, max).expect_err("should fail")
    }

    #[test]
    fn topic_rules() {
        assert!(validate_outgoing(&msg("a/b"), None).is_ok());
        assert!(validate_outgoing(&msg("$SYS/x"), None).is_ok());
        assert!(validate_outgoing(&msg("/"), None).is_ok());
        assert!(err(&msg(""), None).contains("empty"));
        assert!(err(&msg("a/+"), None).contains("wildcard"));
        assert!(err(&msg("a/#"), None).contains("wildcard"));
        assert!(err(&msg("a\0b"), None).contains("NUL"));
        assert!(validate_outgoing(&msg(&"a".repeat(MAX_LEN)), None).is_ok());
        assert!(err(&msg(&"a".repeat(MAX_LEN + 1)), None).contains("limit"));
    }

    #[test]
    fn property_rules() {
        let mut o = msg("a");
        o.props.content_type = Some("a".repeat(MAX_LEN + 1));
        assert!(err(&o, None).contains("content type"));
        o.props.content_type = Some("text/plain".into());
        o.props.response_topic = Some("r/+".into());
        assert!(err(&o, None).contains("response topic"));
        o.props.response_topic = Some(String::new());
        assert!(err(&o, None).contains("response topic"));
        o.props.response_topic = Some("r/1".into());
        o.props.correlation_data = Some(Bytes::from(vec![0; MAX_LEN]));
        assert!(validate_outgoing(&o, None).is_ok());
        o.props.correlation_data = Some(Bytes::from(vec![0; MAX_LEN + 1]));
        assert!(err(&o, None).contains("correlation"));
        o.props.correlation_data = None;
        o.props.user = vec![("k".into(), "v".into()), ("k\0".into(), "v".into())];
        assert!(err(&o, None).contains("name"));
        o.props.user = vec![("k".into(), "v".repeat(MAX_LEN + 1))];
        assert!(err(&o, None).contains("value"));
        o.props.user = vec![("k".into(), "v".into())];
        o.props.message_expiry = Some(u32::MAX);
        assert!(validate_outgoing(&o, None).is_ok());
    }

    #[test]
    fn utf8_flag_needs_utf8_payload() {
        let mut o = msg("a");
        o.props.payload_utf8 = true;
        assert!(validate_outgoing(&o, None).is_ok());
        o.payload = Bytes::from_static(&[b'a', 0xff]);
        assert!(err(&o, None).contains("byte 1"));
        o.props.payload_utf8 = false;
        assert!(validate_outgoing(&o, None).is_ok());
    }

    #[test]
    fn size_estimate_and_broker_limit() {
        // Fixed header 1, length 1, topic 2+1, no properties 1, payload 2.
        let mut o = msg("a");
        assert_eq!(encoded_size(&o), 1 + 1 + (2 + 1 + 1 + 2));
        o.qos = Qos::AtLeastOnce;
        assert_eq!(encoded_size(&o), 10);
        o.props.payload_utf8 = true;
        o.props.message_expiry = Some(5);
        o.props.content_type = Some("ab".into());
        o.props.user = vec![("k".into(), "vv".into())];
        // props: 2 + 5 + 5 + (5 + 1 + 2) = 20
        assert_eq!(encoded_size(&o), 1 + 1 + (2 + 1 + 2 + 1 + 20 + 2));
        assert!(validate_outgoing(&o, Some(29)).is_err());
        assert!(validate_outgoing(&o, Some(30)).is_ok());
        assert_eq!(encoded_size(&o), 30);
        let text = err(&o, Some(10));
        assert!(text.contains("30") && text.contains("10"), "{text}");
        // A large payload uses a 4-byte length.
        let mut big = msg("a");
        big.payload = Bytes::from(vec![0; 3_000_000]);
        assert_eq!(encoded_size(&big), 1 + 4 + 2 + 1 + 1 + 3_000_000);
        assert!(validate_outgoing(&big, Some(1024 * 1024)).is_err());
        assert!(validate_outgoing(&big, None).is_ok());
    }

    #[test]
    fn hex_parse_and_format() {
        assert_eq!(parse_hex("").as_deref(), Ok(&[][..]));
        assert_eq!(
            parse_hex("00 ff\n7B\t1a").as_deref(),
            Ok(&[0, 255, 0x7b, 0x1a][..])
        );
        assert_eq!(parse_hex("0aFF").as_deref(), Ok(&[0x0a, 0xff][..]));
        assert!(parse_hex("abc").expect_err("odd").contains("odd"));
        assert_eq!(
            parse_hex("a b").as_deref(),
            Ok(&[0xab][..]),
            "white space can split a byte"
        );
        assert!(parse_hex("0g").expect_err("g").contains("position 1"));
        assert!(parse_hex("0x10").is_err());
        assert!(parse_hex("\u{ff11}\u{ff11}").is_err(), "full-width digits");
        assert_eq!(to_hex(&[]), "");
        assert_eq!(to_hex(&[0, 255, 16]), "00 ff 10");
        let mut x = 0x1234_5678_9abc_def1u64;
        for len in 0..64 {
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect();
            assert_eq!(parse_hex(&to_hex(&bytes)), Ok(bytes));
        }
    }

    #[test]
    fn present_but_empty_properties_are_valid_and_counted() {
        let mut o = msg("a");
        let base = encoded_size(&o);
        o.props.content_type = Some(String::new());
        o.props.correlation_data = Some(Bytes::new());
        o.props.user = vec![(String::new(), String::new())];
        assert!(validate_outgoing(&o, None).is_ok());
        // Content type 3, correlation data 3, one user property 5 bytes.
        assert_eq!(encoded_size(&o), base + 3 + 3 + 5);
    }
}
