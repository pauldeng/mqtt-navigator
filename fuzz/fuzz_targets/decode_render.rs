#![no_main]
use std::sync::atomic::AtomicBool;

use libfuzzer_sys::fuzz_target;
use navigator_core::decode::{Decoder, Endian, NumberKind, auto, preview, render};
use navigator_core::model::MsgProps;

const KINDS: [NumberKind; 10] = [
    NumberKind::I8,
    NumberKind::U8,
    NumberKind::I16,
    NumberKind::U16,
    NumberKind::I32,
    NumberKind::U32,
    NumberKind::I64,
    NumberKind::U64,
    NumberKind::F32,
    NumberKind::F64,
];

// The first byte picks the decoder and the limit. The rest is the payload.
fuzz_target!(|data: &[u8]| {
    let Some((&sel, payload)) = data.split_first() else {
        return;
    };
    let cancel = AtomicBool::new(false);
    let endian = if sel & 0x80 == 0 { Endian::Little } else { Endian::Big };
    let decoder = match sel % 14 {
        0 => Decoder::Text,
        1 => Decoder::Json,
        2 => Decoder::Hex,
        3 => Decoder::Base64,
        n => Decoder::Number(KINDS[usize::from(n - 4)], endian),
    };
    let limit = if sel & 0x40 == 0 { usize::MAX } else { payload.len() / 2 };
    let r = render(decoder, payload, limit, &cancel);
    if let Some(n) = r.truncated_at {
        assert!(n <= payload.len().min(limit));
    }
    let _ = preview(payload, usize::from(sel) % 40);
    let _ = auto(payload, &MsgProps::default());
});
