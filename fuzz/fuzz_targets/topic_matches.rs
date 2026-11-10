#![no_main]
use libfuzzer_sys::fuzz_target;
use navigator_core::topic::{is_valid_filter, matches};

fuzz_target!(|input: (&str, &str)| {
    let (filter, topic) = input;
    if matches(filter, topic) {
        assert!(is_valid_filter(filter));
    }
    // A topic always matches itself as a filter if it is a valid filter.
    if is_valid_filter(topic) && !topic.contains(['+', '#']) && !topic.starts_with("$share/") {
        assert!(matches(topic, topic));
    }
});
