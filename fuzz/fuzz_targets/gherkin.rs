//! Loads arbitrary text as a `.feature` file, with its first line as the
//! `--tags` expression: loading may fail, but never panic.

#![no_main]

use std::path::Path;
use std::sync::{Arc, OnceLock};

use apitest::cucumber::{FeatureOptions, Steps, load_str};
use libfuzzer_sys::fuzz_target;

/// The built-in step library, built once.
fn steps() -> Arc<Steps> {
    static STEPS: OnceLock<Arc<Steps>> = OnceLock::new();
    STEPS.get_or_init(|| Arc::new(Steps::builtin())).clone()
}

fuzz_target!(|text: &str| {
    // `@contract:` reads the named file, and a device such as /dev/stdin would block.
    if text.contains("contract:") {
        return;
    }

    let (tags, source) = text.split_once('\n').unwrap_or(("", text));
    let opts = FeatureOptions {
        tags: (!tags.is_empty()).then(|| tags.to_string()),
    };
    let _ = load_str(source, Path::new("fuzz.feature"), &opts, steps());
});
