//! Parses arbitrary `traceparent` headers; whatever parses must print back to an equal value.

#![no_main]

use apitest::trace::Traceparent;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    if let Ok(tp) = Traceparent::parse(text) {
        assert_eq!(Traceparent::parse(&tp.to_string()).unwrap(), tp);
    }
});
