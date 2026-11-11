//! Payload decoders for the details pane, and the one-line preview. Pure functions: no state,
//! no gpui, no I/O. The UI runs [`render`] on a background thread and sets `cancel` when the
//! user selects another message.

use std::fmt::{Display, Write as _};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::model::MsgProps;
use crate::text::{escape, is_bidi_control};

/// Default input limit of the UI: it renders the first 256 KiB. "Show all" passes the length of
/// the stored payload.
pub const DEFAULT_LIMIT: usize = 256 * 1024;
/// Largest output of the JSON decoder, in bytes. Pretty printing can make a deeply nested
/// document much larger than its input.
pub const JSON_OUT_MAX: usize = 8 * 1024 * 1024;

/// Width of a number for [`Decoder::Number`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberKind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
}

impl NumberKind {
    /// Size of one value, in bytes.
    pub fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }
}

/// Byte order of a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Endian {
    #[default]
    Little,
    Big,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoder {
    /// Strict UTF-8.
    Text,
    /// JSON, pretty printed. Keys keep their order.
    Json,
    /// Offset, 16 bytes and an ASCII column on each line (like `hexdump -C`).
    Hex,
    /// Standard Base64 with padding, on one line.
    Base64,
    /// A list of numbers, one for each line.
    Number(NumberKind, Endian),
}

/// The result of [`render`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    /// The decoder stopped before the end of the input. The value is the number of input bytes
    /// that it used (always a whole character, value or Base64 group).
    pub truncated_at: Option<usize>,
    /// Why the decoder could not show the payload, as text for the user. The `text` can still
    /// hold a partial result (JSON output over [`JSON_OUT_MAX`], or a cancelled run).
    pub error: Option<String>,
}

impl Rendered {
    fn ok(text: String) -> Self {
        Self {
            text,
            ..Self::default()
        }
    }

    fn err(msg: impl Into<String>) -> Self {
        Self {
            error: Some(msg.into()),
            ..Self::default()
        }
    }

    fn cancelled(text: String, used: usize) -> Self {
        Self {
            text,
            truncated_at: Some(used),
            error: Some("Cancelled.".into()),
        }
    }
}

/// The decoder to use when the user has not chosen one.
///
/// - JSON: the content type is `application/json` or ends with `+json`, or the payload (after
///   white space) is an object or array: `{`..`}` or `[`..`]`.
/// - Text: the payload format indicator says UTF-8, or the payload is valid UTF-8.
/// - Hex: all other payloads. This never guesses numbers.
///
/// This scans the whole payload for valid UTF-8 (a fast check) unless the indicator is set.
pub fn auto(payload: &[u8], props: &MsgProps) -> Decoder {
    if props.content_type.as_deref().is_some_and(json_content_type) || sniff_json(payload) {
        Decoder::Json
    } else if props.payload_utf8 || std::str::from_utf8(payload).is_ok() {
        Decoder::Text
    } else {
        Decoder::Hex
    }
}

fn json_content_type(ct: &str) -> bool {
    let essence = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    essence == "application/json" || essence.ends_with("+json")
}

fn sniff_json(payload: &[u8]) -> bool {
    let b = payload.trim_ascii();
    matches!(
        (b.first(), b.last()),
        (Some(b'{'), Some(b'}')) | (Some(b'['), Some(b']'))
    )
}

fn is_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// Decodes the first `limit` bytes of `payload`. Checks `cancel` while it works. For JSON, a
/// payload longer than `limit` is an error (a cut document is not valid JSON): pass the full
/// length to format it. The JSON parser itself cannot be cancelled.
pub fn render(decoder: Decoder, payload: &[u8], limit: usize, cancel: &AtomicBool) -> Rendered {
    let cut = payload.len() > limit;
    let head = &payload[..payload.len().min(limit)];
    if is_cancelled(cancel) {
        return Rendered::cancelled(String::new(), 0);
    }
    match decoder {
        Decoder::Text => text(head, cut),
        Decoder::Json if cut => Rendered::err(format!(
            "The payload is larger than {limit} bytes. Show all to format it as JSON."
        )),
        Decoder::Json => json(head, cancel),
        Decoder::Hex => hex(head, cut, cancel),
        Decoder::Base64 => base64(head, cut, cancel),
        Decoder::Number(kind, endian) => number(kind, endian, head, cut, cancel),
    }
}

fn text(head: &[u8], cut: bool) -> Rendered {
    match std::str::from_utf8(head) {
        Ok(s) => Rendered {
            text: s.into(),
            truncated_at: cut.then_some(head.len()),
            error: None,
        },
        // The limit cut a character in two: the rest is valid.
        Err(e) if cut && e.error_len().is_none() => {
            let n = e.valid_up_to();
            Rendered {
                text: String::from_utf8_lossy(&head[..n]).into_owned(),
                truncated_at: Some(n),
                error: None,
            }
        }
        Err(e) => Rendered::err(format!(
            "The payload is not valid UTF-8 (first error at byte {}). Use Hex.",
            e.valid_up_to()
        )),
    }
}

/// Writer that stops at `cap` bytes or when `cancel` is set.
struct Capped<'a> {
    buf: Vec<u8>,
    cap: usize,
    cancel: &'a AtomicBool,
    over: bool,
}

impl io::Write for Capped<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if is_cancelled(self.cancel) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.buf.len() + data.len() > self.cap {
            self.over = true;
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn json(head: &[u8], cancel: &AtomicBool) -> Rendered {
    // serde_json stops at nesting depth 128, so a hostile payload cannot overflow the stack.
    let value: serde_json::Value = match serde_json::from_slice(head) {
        Ok(v) => v,
        Err(e) if e.to_string().contains("recursion limit") => {
            return Rendered::err("The JSON is nested too deeply (the limit is 127 levels).");
        }
        Err(e) => return Rendered::err(format!("The payload is not valid JSON: {e}.")),
    };
    if is_cancelled(cancel) {
        return Rendered::cancelled(String::new(), 0);
    }
    let mut out = Capped {
        buf: Vec::new(),
        cap: JSON_OUT_MAX,
        cancel,
        over: false,
    };
    let result = serde_json::to_writer_pretty(&mut out, &value);
    let mut text = String::from_utf8_lossy(&out.buf).into_owned();
    if out.over {
        // Cut at a line end, so the last line is whole.
        text.truncate(text.rfind('\n').unwrap_or(0));
        return Rendered {
            text,
            truncated_at: None,
            error: Some("The formatted JSON is larger than 8 MiB. Showing the start.".into()),
        };
    }
    match result {
        Ok(()) => Rendered::ok(text),
        Err(_) => Rendered::cancelled(text, 0),
    }
}

fn hex(head: &[u8], cut: bool, cancel: &AtomicBool) -> Rendered {
    let mut out = String::with_capacity(head.len() / 16 * 77 + 80);
    for (row, chunk) in head.chunks(16).enumerate() {
        if is_cancelled(cancel) {
            return Rendered::cancelled(out, row * 16);
        }
        let _ = write!(out, "{:08x} ", row * 16);
        for i in 0..16 {
            if i == 8 {
                out.push(' ');
            }
            match chunk.get(i) {
                Some(b) => {
                    let _ = write!(out, " {b:02x}");
                }
                None => out.push_str("   "),
            }
        }
        out.push_str("  |");
        out.extend(chunk.iter().map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        }));
        out.push_str("|\n");
    }
    Rendered {
        text: out,
        truncated_at: cut.then_some(head.len()),
        error: None,
    }
}

fn base64(head: &[u8], cut: bool, cancel: &AtomicBool) -> Rendered {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    // A cut at a multiple of 3 bytes keeps the text a prefix of the full encoding.
    let head = if cut {
        &head[..head.len() / 3 * 3]
    } else {
        head
    };
    let mut out = String::with_capacity(head.len().div_ceil(3) * 4);
    for (i, c) in head.chunks(3).enumerate() {
        if i % 4096 == 0 && is_cancelled(cancel) {
            return Rendered::cancelled(out, i * 3);
        }
        let n = (u32::from(c[0]) << 16)
            | (u32::from(c.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(c.get(2).copied().unwrap_or(0));
        let sextet = |shift: u32| ALPHABET[(n >> shift) as usize & 63] as char;
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if c.len() > 1 { sextet(6) } else { '=' });
        out.push(if c.len() > 2 { sextet(0) } else { '=' });
    }
    Rendered {
        text: out,
        truncated_at: cut.then_some(head.len()),
        error: None,
    }
}

/// Writes one value for each line. Returns the number of input bytes used.
fn numbers<const N: usize, T: Display>(
    data: &[u8],
    out: &mut String,
    cancel: &AtomicBool,
    from: fn([u8; N]) -> T,
) -> Result<(), usize> {
    for (i, &bytes) in data.as_chunks::<N>().0.iter().enumerate() {
        if i % 4096 == 0 && is_cancelled(cancel) {
            return Err(i * N);
        }
        let _ = writeln!(out, "{}", from(bytes));
    }
    Ok(())
}

fn number(
    kind: NumberKind,
    endian: Endian,
    head: &[u8],
    cut: bool,
    cancel: &AtomicBool,
) -> Rendered {
    let size = kind.size();
    let whole = head.len() / size * size;
    if whole != head.len() && !cut {
        return Rendered::err(format!(
            "The payload has {} bytes. A {}-byte number needs a multiple of {size}.",
            head.len(),
            size
        ));
    }
    let data = &head[..whole];
    let mut out = String::with_capacity(whole / size * 6);
    macro_rules! run {
        ($n:literal, $t:ty) => {
            match endian {
                Endian::Little => numbers::<$n, $t>(data, &mut out, cancel, <$t>::from_le_bytes),
                Endian::Big => numbers::<$n, $t>(data, &mut out, cancel, <$t>::from_be_bytes),
            }
        };
    }
    let done = match kind {
        NumberKind::I8 => run!(1, i8),
        NumberKind::U8 => run!(1, u8),
        NumberKind::I16 => run!(2, i16),
        NumberKind::U16 => run!(2, u16),
        NumberKind::I32 => run!(4, i32),
        NumberKind::U32 => run!(4, u32),
        NumberKind::I64 => run!(8, i64),
        NumberKind::U64 => run!(8, u64),
        NumberKind::F32 => run!(4, f32),
        NumberKind::F64 => run!(8, f64),
    };
    match done {
        Ok(()) => Rendered {
            text: out,
            truncated_at: cut.then_some(whole),
            error: None,
        },
        Err(used) => Rendered::cancelled(out, used),
    }
}

/// One-line preview of a payload: white space collapsed to one space, control characters as
/// `·`, direction controls as `⟨U+202E⟩`, invalid UTF-8 as `bin 7b 22 …` (the first 16 bytes).
///
/// - Zero-width characters and joiners stay, so that emoji sequences and combining marks show
///   correctly. Topics get the stricter [`crate::text::display_safe`].
/// - It reads at most the first 512 bytes. The cost does not depend on the payload size.
/// - It keeps `max_chars` characters and then adds `…`. A cut inside a UTF-8 sequence at byte
///   512 is not an error.
/// - The cut is safe for display: it never separates a character from the combining marks,
///   variation selectors, skin-tone modifiers or zero-width joiner sequences that follow it.
// ponytail: not full grapheme clusters (a flag is two regional indicators and can be split);
// add unicode-segmentation if that ever matters.
pub fn preview(payload: &[u8], max_chars: usize) -> String {
    let head = &payload[..payload.len().min(512)];
    let text = match std::str::from_utf8(head) {
        Ok(s) => s,
        Err(e) if e.error_len().is_none() => {
            std::str::from_utf8(&head[..e.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => {
            let hex: Vec<String> = head.iter().take(16).map(|b| format!("{b:02x}")).collect();
            return format!("bin {}", hex.join(" "));
        }
    };
    let mut out = String::with_capacity(max_chars.min(512) + 3);
    let (mut count, mut extra) = (0usize, 0usize);
    let mut last_space = false;
    let mut join = false;
    for c in text.chars() {
        if count >= max_chars {
            // Keep the marks of the last character, but not for ever.
            if (join || is_extender(c)) && extra < 16 {
                extra += 1;
            } else {
                out.push('…');
                return out;
            }
        } else {
            count += 1;
        }
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            if is_bidi_control(c) {
                // A direction control can change how the text around it looks.
                out.push_str(&escape(c));
            } else {
                out.push(if c.is_control() { '·' } else { c });
            }
            last_space = false;
        }
        join = c == '\u{200d}';
    }
    if payload.len() > head.len() {
        out.push('…');
    }
    out
}

/// A character that must stay with the one before it.
fn is_extender(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036f}'   // combining marks
        | '\u{1ab0}'..='\u{1aff}'
        | '\u{1dc0}'..='\u{1dff}'
        | '\u{20d0}'..='\u{20ff}'
        | '\u{fe20}'..='\u{fe2f}'
        | '\u{200d}'              // zero-width joiner
        | '\u{fe00}'..='\u{fe0f}' // variation selectors
        | '\u{1f3fb}'..='\u{1f3ff}' // skin tones
        | '\u{e0020}'..='\u{e007f}' // tag characters
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    static NO: AtomicBool = AtomicBool::new(false);

    fn run(d: Decoder, p: &[u8]) -> Rendered {
        render(d, p, usize::MAX, &NO)
    }

    fn ok(d: Decoder, p: &[u8]) -> String {
        let r = run(d, p);
        assert_eq!(r.error, None);
        assert_eq!(r.truncated_at, None);
        r.text
    }

    /// Deterministic xorshift64.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn bytes(&mut self, max: usize) -> Vec<u8> {
            let n = self.next() as usize % (max + 1);
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    fn all_decoders() -> Vec<Decoder> {
        use NumberKind::*;
        let mut v = vec![Decoder::Text, Decoder::Json, Decoder::Hex, Decoder::Base64];
        for k in [I8, U8, I16, U16, I32, U32, I64, U64, F32, F64] {
            v.push(Decoder::Number(k, Endian::Little));
            v.push(Decoder::Number(k, Endian::Big));
        }
        v
    }

    #[test]
    fn no_panic_on_random_bytes_and_truncations() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let samples = [
            &b"{\"a\":[1,2,{\"b\":null}],\"c\":\"\xc3\xa9\"}"[..],
            b"[[[[",
            "h\u{e9}llo \u{1f44d}\u{1f3fd}".as_bytes(),
        ];
        #[allow(clippy::needless_range_loop)]
        for round in 0..300 {
            let p = if round < 3 {
                samples[round].to_vec()
            } else {
                rng.bytes(300)
            };
            for d in all_decoders() {
                for limit in [0, 1, 2, 3, 7, 16, 17, p.len() / 2, p.len(), usize::MAX] {
                    let r = render(d, &p, limit, &NO);
                    if let Some(n) = r.truncated_at {
                        assert!(n <= p.len().min(limit));
                    }
                }
                // Every prefix: the decoder must never panic.
                for n in 0..p.len().min(40) {
                    run(d, &p[..n]);
                }
            }
            let _ = preview(&p, (round % 20) + 1);
            let _ = auto(&p, &MsgProps::default());
        }
    }

    #[test]
    fn text_is_strict() {
        assert_eq!(ok(Decoder::Text, "h\u{e9}".as_bytes()), "h\u{e9}");
        let r = run(Decoder::Text, &[b'a', 0xff]);
        assert!(r.error.expect("error").contains("byte 1"));
        // A limit inside a character is not an error.
        let r = render(Decoder::Text, "a\u{e9}".as_bytes(), 2, &NO);
        assert_eq!(
            (r.text.as_str(), r.truncated_at, r.error),
            ("a", Some(1), None)
        );
        // An invalid byte inside the limit is an error.
        assert!(
            render(Decoder::Text, &[b'a', 0xff, b'b'], 2, &NO)
                .error
                .is_some()
        );
    }

    #[test]
    fn json_pretty_round_trip_and_order() {
        let src = br#"{"z":1,"a":[1,2.5,"x\n",null,true,{"k":{}}],"u":"\u00e9"}"#;
        let text = ok(Decoder::Json, src);
        assert!(text.contains("\n  \"z\": 1,\n  \"a\": ["), "{text}");
        assert!(text.find("\"z\"") < text.find("\"a\""), "key order kept");
        let a: serde_json::Value = serde_json::from_slice(src).expect("src");
        let b: serde_json::Value = serde_json::from_str(&text).expect("pretty");
        assert_eq!(a, b);
        assert_eq!(ok(Decoder::Json, b"42"), "42");
        assert!(
            run(Decoder::Json, b"{")
                .error
                .expect("e")
                .contains("not valid JSON")
        );
        assert!(run(Decoder::Json, b"").error.is_some());
    }

    #[test]
    fn json_depth_and_output_caps() {
        let deep = format!("{}{}", "[".repeat(128), "]".repeat(128));
        assert!(
            run(Decoder::Json, deep.as_bytes())
                .error
                .expect("e")
                .contains("nested too deeply")
        );
        let fine = format!("{}{}", "[".repeat(127), "]".repeat(127));
        assert_eq!(run(Decoder::Json, fine.as_bytes()).error, None);
        // 127 levels with many small items: the pretty output is far larger than the input.
        let items = "1,".repeat(300_000);
        let doc = format!(
            "{}{}0{}",
            "[".repeat(126),
            format_args!("[{items}"),
            "]".repeat(127)
        );
        let r = run(Decoder::Json, doc.as_bytes());
        assert!(r.error.expect("e").contains("8 MiB"));
        assert!(r.text.len() <= JSON_OUT_MAX && !r.text.is_empty());
        assert!(r.text.is_char_boundary(r.text.len()));
    }

    #[test]
    fn json_over_limit_asks_for_show_all() {
        let r = render(Decoder::Json, b"[1,2,3]", 3, &NO);
        assert!(r.error.expect("e").contains("Show all"));
        assert_eq!(render(Decoder::Json, b"[1,2,3]", 7, &NO).error, None);
    }

    #[test]
    fn hex_dump_format() {
        let p: Vec<u8> = (0u8..20)
            .map(|b| if b < 17 { b'A' + b } else { b })
            .collect();
        let t = ok(Decoder::Hex, &p);
        let mut lines = t.lines();
        assert_eq!(
            lines.next(),
            Some("00000000  41 42 43 44 45 46 47 48  49 4a 4b 4c 4d 4e 4f 50  |ABCDEFGHIJKLMNOP|")
        );
        assert_eq!(
            lines.next(),
            Some("00000010  51 11 12 13                                       |Q...|")
        );
        assert_eq!(lines.next(), None);
        assert_eq!(ok(Decoder::Hex, b""), "");
    }

    #[test]
    fn base64_vectors() {
        // RFC 4648 section 10.
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(ok(Decoder::Base64, i.as_bytes()), o);
        }
        assert_eq!(ok(Decoder::Base64, &[0xfb, 0xff, 0xfe]), "+//+");
        // A cut keeps whole groups, and the text is a prefix of the full encoding.
        let full = ok(Decoder::Base64, b"foobar");
        let r = render(Decoder::Base64, b"foobar", 4, &NO);
        assert_eq!((r.text.as_str(), r.truncated_at), ("Zm9v", Some(3)));
        assert!(full.starts_with(&r.text));
    }

    #[test]
    fn number_vectors() {
        use NumberKind::*;
        let n = |k, e, p: &[u8]| ok(Decoder::Number(k, e), p);
        let (l, b) = (Endian::Little, Endian::Big);
        assert_eq!(n(U8, l, &[1, 255]), "1\n255\n");
        assert_eq!(n(I8, l, &[1, 255]), "1\n-1\n");
        assert_eq!(n(U16, l, &[1, 0, 0, 1]), "1\n256\n");
        assert_eq!(n(U16, b, &[1, 0, 0, 1]), "256\n1\n");
        assert_eq!(n(I16, b, &[0xff, 0xfe]), "-2\n");
        assert_eq!(n(U32, l, &[1, 0, 0, 0]), "1\n");
        assert_eq!(n(I32, b, &[0xff, 0xff, 0xff, 0xfe]), "-2\n");
        assert_eq!(n(U64, b, &[0, 0, 0, 0, 0, 0, 1, 0]), "256\n");
        assert_eq!(n(I64, l, &[0xff; 8]), "-1\n");
        assert_eq!(n(F32, b, &1.5f32.to_be_bytes()), "1.5\n");
        assert_eq!(n(F32, l, &21.4f32.to_le_bytes()), "21.4\n");
        assert_eq!(n(F64, l, &(-0.25f64).to_le_bytes()), "-0.25\n");
        assert_eq!(n(F64, b, &f64::NAN.to_be_bytes()), "NaN\n");
        assert_eq!(n(U8, l, b""), "");
        let r = run(Decoder::Number(U32, l), &[1, 2, 3, 4, 5]);
        assert!(r.error.expect("e").contains("multiple of 4"));
    }

    #[test]
    fn limits() {
        let p = [1u8, 0, 2, 0, 3, 0, 4, 0];
        // A number limit cuts at a whole value.
        let r = render(Decoder::Number(NumberKind::U16, Endian::Little), &p, 5, &NO);
        assert_eq!(
            (r.text.as_str(), r.truncated_at, r.error),
            ("1\n2\n", Some(4), None)
        );
        let r = render(Decoder::Hex, &[0u8; 40], 20, &NO);
        assert_eq!(r.truncated_at, Some(20));
        assert_eq!(r.text.lines().count(), 2);
        let r = render(Decoder::Text, b"abcdef", 4, &NO);
        assert_eq!((r.text.as_str(), r.truncated_at), ("abcd", Some(4)));
        let r = render(Decoder::Text, b"abcdef", 6, &NO);
        assert_eq!(r.truncated_at, None);
    }

    #[test]
    fn cancel_stops_every_decoder() {
        let stop = AtomicBool::new(true);
        let big = vec![7u8; 1 << 20];
        for d in all_decoders() {
            let r = render(d, &big, usize::MAX, &stop);
            assert_eq!(r.error.as_deref(), Some("Cancelled."), "{d:?}");
        }
        // Set while the work runs: the loops see it.
        let flag = AtomicBool::new(false);
        let mid = std::thread::scope(|s| {
            let h = s.spawn(|| {
                let mut r = Rendered::default();
                while r.error.is_none() {
                    r = render(Decoder::Hex, &big, usize::MAX, &flag);
                }
                r
            });
            std::thread::sleep(std::time::Duration::from_millis(5));
            flag.store(true, Ordering::Relaxed);
            h.join().expect("thread")
        });
        assert_eq!(mid.error.as_deref(), Some("Cancelled."));
        assert!(mid.truncated_at.is_some());
    }

    #[test]
    fn auto_choice() {
        let props = |ct: Option<&str>, utf8| MsgProps {
            content_type: ct.map(Into::into),
            payload_utf8: utf8,
            ..MsgProps::default()
        };
        let none = MsgProps::default();
        assert_eq!(auto(b" \n{\"a\":1}\n", &none), Decoder::Json);
        assert_eq!(auto(b"[1,2]", &none), Decoder::Json);
        assert_eq!(auto(b"[INFO] hi", &none), Decoder::Text);
        assert_eq!(auto(b"{x", &none), Decoder::Text);
        assert_eq!(auto(b"21.4", &none), Decoder::Text);
        assert_eq!(
            auto(b"x", &props(Some("Application/JSON; charset=utf-8"), false)),
            Decoder::Json
        );
        assert_eq!(
            auto(b"x", &props(Some("application/ld+json"), false)),
            Decoder::Json
        );
        assert_eq!(auto(b"x", &props(Some("text/plain"), false)), Decoder::Text);
        assert_eq!(auto(&[0xff, 0x00], &none), Decoder::Hex);
        assert_eq!(auto(&[0xff, 0x00], &props(None, true)), Decoder::Text);
        assert_eq!(auto(b"", &none), Decoder::Text);
    }

    #[test]
    fn preview_collapses_whitespace_and_marks_binary() {
        assert_eq!(preview(b"{\n  \"v\": 1\n}", 80), "{ \"v\": 1 }");
        assert_eq!(preview(&[0xff, 0x00, 0x10], 80), "bin ff 00 10");
        assert_eq!(preview("abcdef".as_bytes(), 3), "abc\u{2026}");
        assert_eq!(preview(b"a\x01b", 80), "a\u{b7}b");
        // A cut inside a multi-byte character at byte 512 is not binary.
        let long = "\u{e9}".repeat(400);
        assert!(!preview(long.as_bytes(), 80).starts_with("bin"));
        assert_eq!(preview(b"", 5), "");
    }

    #[test]
    fn preview_shows_direction_controls() {
        assert_eq!(
            preview("pay \u{202e}fdp.exe \u{2066}x".as_bytes(), 80),
            "pay ⟨U+202E⟩fdp.exe ⟨U+2066⟩x"
        );
        assert_eq!(
            preview("a\u{200f}b\u{061c}".as_bytes(), 80),
            "a⟨U+200F⟩b⟨U+061C⟩"
        );
        // An escape counts as one character.
        assert_eq!(preview("\u{202e}abc".as_bytes(), 2), "⟨U+202E⟩a\u{2026}");
    }

    #[test]
    fn preview_keeps_combining_marks_and_joiners() {
        // e + acute accent: the mark stays with the e.
        assert_eq!(preview("ae\u{301}b".as_bytes(), 2), "ae\u{301}\u{2026}");
        // Thumbs up + skin tone.
        assert_eq!(
            preview("a\u{1f44d}\u{1f3fd}b".as_bytes(), 2),
            "a\u{1f44d}\u{1f3fd}\u{2026}"
        );
        // Family emoji: man ZWJ woman.
        let fam = "\u{1f468}\u{200d}\u{1f469}x";
        assert_eq!(
            preview(fam.as_bytes(), 1),
            "\u{1f468}\u{200d}\u{1f469}\u{2026}"
        );
        // Many marks: the extension stops.
        let many = format!("a{}b", "\u{301}".repeat(100));
        assert!(preview(many.as_bytes(), 1).chars().count() <= 1 + 16 + 1);
    }
}
