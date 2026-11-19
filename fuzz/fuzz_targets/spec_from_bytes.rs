//! Loads arbitrary bytes as an OpenAPI document, as JSON, as YAML and by sniffing.
//! Exercises `$ref` inlining on hostile documents (cycles, deep nesting).

#![no_main]

use apitest::contract::Spec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for hint in [Some("json"), Some("yaml"), None] {
        let _ = Spec::from_bytes(data, hint);
    }
});
