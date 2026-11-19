//! Parses arbitrary text as a `.env` file, then masks the text with every parsed
//! value registered as a secret: no registered value may survive.

#![no_main]

use apitest::Redactor;
use apitest::dotenv::parse;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let Ok(vars) = parse(text) else { return };
    let redactor = Redactor::default();
    for v in vars.values() {
        redactor.secret(v);
    }

    let masked = redactor.redact(text);
    for v in vars
        .values()
        .filter(|v| v.chars().count() >= apitest::redact::MIN_SECRET_LEN)
    {
        assert!(!masked.contains(v.as_str()), "{v:?} survived");
    }
});
