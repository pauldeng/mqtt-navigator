//! Pure parts of the details pane (the inspector): decoder choice, the meta line, the property
//! list and the generation counter of the background render job. No gpui types.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use navigator_core::decode::{self, Decoder, Endian, NumberKind};
use navigator_core::publish::to_hex;
use navigator_core::{MsgProps, Qos};

/// The choices of the decoder `Select`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderPick {
    Auto,
    Text,
    Json,
    Hex,
    Base64,
    Number,
}

impl DecoderPick {
    pub const ALL: [Self; 6] = [
        Self::Auto,
        Self::Text,
        Self::Json,
        Self::Hex,
        Self::Base64,
        Self::Number,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Text => "Text",
            Self::Json => "JSON",
            Self::Hex => "Hex",
            Self::Base64 => "Base64",
            Self::Number => "Number",
        }
    }
}

/// The widths of the Number decoder, with their labels.
pub const NUMBER_KINDS: [(NumberKind, &str); 10] = [
    (NumberKind::I8, "int8"),
    (NumberKind::U8, "uint8"),
    (NumberKind::I16, "int16"),
    (NumberKind::U16, "uint16"),
    (NumberKind::I32, "int32"),
    (NumberKind::U32, "uint32"),
    (NumberKind::I64, "int64"),
    (NumberKind::U64, "uint64"),
    (NumberKind::F32, "float32"),
    (NumberKind::F64, "float64"),
];

/// What the user chose for the payload view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecoderChoice {
    pub pick: DecoderPick,
    pub kind: NumberKind,
    pub endian: Endian,
}

impl Default for DecoderChoice {
    fn default() -> Self {
        Self {
            pick: DecoderPick::Auto,
            kind: NumberKind::I32,
            endian: Endian::Little,
        }
    }
}

impl DecoderChoice {
    /// The decoder for one message. Auto scans the payload (a fast UTF-8 check).
    pub fn resolve(self, payload: &[u8], props: &MsgProps) -> Decoder {
        match self.pick {
            DecoderPick::Auto => decode::auto(payload, props),
            DecoderPick::Text => Decoder::Text,
            DecoderPick::Json => Decoder::Json,
            DecoderPick::Hex => Decoder::Hex,
            DecoderPick::Base64 => Decoder::Base64,
            DecoderPick::Number => Decoder::Number(self.kind, self.endian),
        }
    }
}

/// "1.2 MiB", "340 KiB" or "20 B".
pub fn size_text(n: usize) -> String {
    const KIB: f64 = 1024.0;
    let x = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if x < KIB * KIB {
        format!("{:.0} KiB", x / KIB)
    } else {
        format!("{:.1} MiB", x / (KIB * KIB))
    }
}

/// "QoS 1 · Retained · 20 B", with "truncated: first X B stored" when the store cut the payload.
pub fn meta_line(qos: Qos, retain: bool, size: usize, stored: usize, truncated: bool) -> String {
    let mut out = format!("QoS {}", qos.as_u8());
    if retain {
        out.push_str(" · Retained");
    }
    out.push_str(&format!(" · {size} B"));
    if truncated {
        out.push_str(&format!(" · truncated: first {stored} B stored"));
    }
    out
}

/// The MQTT 5 properties as (name, value) rows. User properties keep their order, also with the
/// same name twice. A message with no properties gives no rows.
pub fn props_rows(p: &MsgProps) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    if let Some(v) = &p.content_type {
        rows.push(("Content type".to_string(), v.clone()));
    }
    if let Some(v) = &p.response_topic {
        rows.push(("Response topic".to_string(), v.clone()));
    }
    if let Some(v) = &p.correlation_data {
        rows.push(("Correlation data".to_string(), to_hex(v)));
    }
    if let Some(v) = p.message_expiry {
        rows.push(("Message expiry".to_string(), format!("{v} s")));
    }
    if p.payload_utf8 {
        rows.push(("Payload format".to_string(), "UTF-8".to_string()));
    }
    rows.extend(p.user.iter().cloned());
    rows
}

/// The editor sets, wraps and lays out text on the UI thread. Measured cost (release, 360 px
/// pane): about 0.3 ms for each KiB of JSON and 3 to 4 ms for each KiB of a hex dump (long
/// lines wrap). So the text goes in small steps and the step size follows the measured time.
///
/// The first window of a new text.
pub const FIRST_BYTES: usize = 4 * 1024;
/// The text grows to this size by itself after the first window.
pub const AUTO_BYTES: usize = 32 * 1024;
/// One "Show more" adds this much text, in steps over the next frames.
pub const MORE_BYTES: usize = 64 * 1024;
const MIN_STEP: usize = 2 * 1024;
const MAX_STEP: usize = 64 * 1024;
/// The time that one step should take.
const STEP_MS: f32 = 6.0;

/// The size of the next step, from the last step: `bytes` took `took_ms`.
pub fn next_step(bytes: usize, took_ms: f32) -> usize {
    ((bytes as f32 * STEP_MS / took_ms.max(0.1)) as usize).clamp(MIN_STEP, MAX_STEP)
}

/// End of the next chunk of `text`, from the byte `from` (a character boundary). The chunk has
/// at most `chunk` bytes. It ends after a line break in its second half if there is one, so a
/// line is not cut. Else it ends at the last character boundary that fits. `text.len()` when
/// the rest fits. The result is always above `from` (a whole character at least) unless
/// `from` is the end of the text.
pub fn next_cut(text: &str, from: usize, chunk: usize) -> usize {
    let end = from.saturating_add(chunk).min(text.len());
    if end == text.len() {
        return end;
    }
    let mid = from + chunk / 2;
    if let Some(i) = text.as_bytes()[mid..end].iter().rposition(|&b| b == b'\n') {
        return mid + i + 1;
    }
    let mut e = end;
    while !text.is_char_boundary(e) {
        e -= 1;
    }
    // A character wider than the chunk would give no progress: take the whole character.
    if e <= from {
        e = from + text[from..].chars().next().map_or(0, char::len_utf8);
    }
    e
}

/// The generation counter of the background render job. A new job cancels the old one. A result
/// is used only if it has the current generation.
#[derive(Debug)]
pub struct Jobs {
    generation: u64,
    cancel: Arc<AtomicBool>,
}

impl Default for Jobs {
    fn default() -> Self {
        Self {
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Jobs {
    /// Cancels the running job and starts a new generation. Returns the generation and the
    /// cancel flag for the new job.
    pub fn start(&mut self) -> (u64, Arc<AtomicBool>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.generation += 1;
        self.cancel = Arc::new(AtomicBool::new(false));
        (self.generation, self.cancel.clone())
    }

    /// Cancels the running job. Its result is stale from now on.
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.generation += 1;
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro in the parent.
    use super::{
        DecoderChoice, DecoderPick, Jobs, MAX_STEP, MIN_STEP, meta_line, next_cut, next_step,
        props_rows, size_text,
    };
    use bytes::Bytes;
    use navigator_core::decode::Decoder;
    use navigator_core::{MsgProps, Qos};
    use std::sync::atomic::Ordering;

    #[test]
    fn a_new_job_cancels_the_old_one_and_stale_results_are_dropped() {
        let mut jobs = Jobs::default();
        let (g1, c1) = jobs.start();
        assert!(jobs.is_current(g1) && !c1.load(Ordering::Relaxed));
        let (g2, c2) = jobs.start();
        assert!(c1.load(Ordering::Relaxed), "the old job is cancelled");
        assert!(!jobs.is_current(g1) && jobs.is_current(g2));
        jobs.cancel();
        assert!(c2.load(Ordering::Relaxed) && !jobs.is_current(g2));
    }

    #[test]
    fn decoder_choice() {
        let props = MsgProps::default();
        let c = DecoderChoice::default();
        assert_eq!(c.resolve(b"{\"a\":1}", &props), Decoder::Json);
        assert_eq!(c.resolve(b"hi", &props), Decoder::Text);
        assert_eq!(c.resolve(&[0xff, 0xfe], &props), Decoder::Hex);
        let n = DecoderChoice {
            pick: DecoderPick::Number,
            ..c
        };
        assert!(matches!(n.resolve(b"x", &props), Decoder::Number(..)));
    }

    #[test]
    fn meta_and_sizes() {
        assert_eq!(
            meta_line(Qos::AtLeastOnce, true, 20, 20, false),
            "QoS 1 · Retained · 20 B"
        );
        assert_eq!(
            meta_line(Qos::AtMostOnce, false, 900_000, 65_536, true),
            "QoS 0 · 900000 B · truncated: first 65536 B stored"
        );
        assert_eq!(size_text(20), "20 B");
        assert_eq!(size_text(340 * 1024), "340 KiB");
        assert_eq!(size_text(1_258_291), "1.2 MiB");
    }

    #[test]
    fn properties_keep_user_order_with_duplicates() {
        assert!(props_rows(&MsgProps::default()).is_empty());
        let p = MsgProps {
            content_type: Some("a/b".into()),
            response_topic: Some("r".into()),
            correlation_data: Some(Bytes::from_static(&[0xca, 0xfe])),
            message_expiry: Some(9),
            user: vec![("k".into(), "2".into()), ("k".into(), "1".into())],
            payload_utf8: true,
        };
        let rows = props_rows(&p);
        assert_eq!(rows[2], ("Correlation data".into(), "ca fe".into()));
        assert_eq!(rows[3], ("Message expiry".into(), "9 s".into()));
        assert_eq!(rows[5], ("k".into(), "2".into()));
        assert_eq!(rows[6], ("k".into(), "1".into()));
    }

    #[test]
    fn chunks_cut_at_line_ends_and_character_boundaries() {
        // The rest fits: one chunk.
        assert_eq!(next_cut("abc", 0, 10), 3);
        // A line break in the second half of the chunk is the cut (it stays in the chunk).
        let t = "aaaa\nbbbb\ncccc\ndddd";
        assert_eq!(next_cut(t, 0, 12), 10);
        // No line break in the second half: cut at the limit.
        assert_eq!(next_cut("aaaaaaaaaaaaaaaa", 0, 8), 8);
        // Never inside a character: "é" has two bytes.
        let t = "aaaaaaaé";
        assert_eq!(next_cut(t, 0, 8), 7);
        // All chunks together are the whole text, and none is empty.
        let t = "x\n".repeat(1000) + &"é".repeat(777) + &"y".repeat(5000);
        let (mut at, mut parts) = (0, 0);
        while at < t.len() {
            let to = next_cut(&t, at, 1000);
            assert!(to > at && t.is_char_boundary(to));
            at = to;
            parts += 1;
        }
        assert!(parts >= t.len() / 1000);
    }

    #[test]
    fn the_step_follows_the_measured_time() {
        // Fast text (1 ms for 8 KiB): the step grows. Slow text (90 ms for 32 KiB): it shrinks.
        assert!(next_step(8 * 1024, 1.0) > 8 * 1024);
        assert!(next_step(32 * 1024, 90.0) < 4 * 1024);
        // The bounds hold for a zero time and for a very slow step.
        assert_eq!(next_step(64 * 1024, 0.0), MAX_STEP);
        assert_eq!(next_step(1024, 1000.0), MIN_STEP);
    }

    #[test]
    fn growth_always_makes_progress_across_wide_characters() {
        // The target (32 KiB) falls inside "é": a step of 1 byte must still pass it.
        let t = "a".repeat(32767) + "é" + "tail";
        assert_eq!(next_cut(&t, 32767, 1), 32769);
        // Automatic growth in steps of 1 byte reaches the end, and every cut is a boundary.
        let t = "aé€😀".repeat(50);
        let (mut at, mut steps) = (0, 0);
        while at < t.len() {
            let to = next_cut(&t, at, 1);
            assert!(to > at && t.is_char_boundary(to));
            at = to;
            steps += 1;
        }
        assert_eq!(steps, 200);
        // A manual "Show more" target inside a character: the limit is the target, the cut
        // is the next boundary, and the loop stops (shown >= target).
        let t = "x".repeat(65535) + "😀" + "end";
        let target = 65536;
        let mut shown = 65535;
        shown = next_cut(&t, shown, target - shown);
        assert!(shown >= target && t.is_char_boundary(shown));
        // The end of the text stays the end.
        assert_eq!(next_cut("ab", 2, 5), 2);
    }
}
