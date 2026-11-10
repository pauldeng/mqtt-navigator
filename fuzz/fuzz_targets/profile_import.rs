#![no_main]
use libfuzzer_sys::fuzz_target;
use navigator_core::profile::import_profiles;

fuzz_target!(|json: &str| {
    let _ = import_profiles(json, &[]);
});
