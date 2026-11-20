//! Loads arbitrary text as a `.feature` file: loading may fail, but never
//! panic. A first line that starts like a tag expression (`@`, `not`, `(`) is
//! taken as `--tags` instead, so most inputs still reach the feature parser.

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

    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let first = first.trim_end_matches('\r');
    let is_tags = ["@", "not", "("].iter().any(|p| first.starts_with(p));
    let (tags, source) = if is_tags {
        (Some(first), rest)
    } else {
        (None, text)
    };
    let opts = FeatureOptions {
        tags: tags.map(str::to_string),
    };
    let _ = load_str(source, Path::new("fuzz.feature"), &opts, steps());
});
