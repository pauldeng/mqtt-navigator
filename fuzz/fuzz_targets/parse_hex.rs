#![no_main]
use libfuzzer_sys::fuzz_target;
use navigator_core::publish::{parse_hex, to_hex};

fuzz_target!(|s: &str| {
    if let Ok(bytes) = parse_hex(s) {
        assert_eq!(parse_hex(&to_hex(&bytes)), Ok(bytes));
    }
});
