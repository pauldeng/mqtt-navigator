#![no_main]
use libfuzzer_sys::fuzz_target;
use navigator_core::topic::{check_filter, matches};

fuzz_target!(|input: (&str, &str)| {
    let (filter, topic) = input;
    if matches(filter, topic) {
        assert!(check_filter(filter).is_ok());
    }
    // A topic always matches itself as a filter if it is a valid filter.
    if check_filter(topic).is_ok() && !topic.contains(['+', '#']) && !topic.starts_with("$share/") {
        assert!(matches(topic, topic));
    }
});
