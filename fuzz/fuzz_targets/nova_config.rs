//! Parses arbitrary text as `apitest.nova` and as `apitest.lock.nova`, both schema-validated.

#![no_main]

use apitest::config::ProjectConfig;
use apitest::contract::Lockfile;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let _ = ProjectConfig::parse(text, "fuzz");
    let _ = Lockfile::parse(text, "fuzz");
});
