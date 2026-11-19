//! Diffs two arbitrary `OpenAPI` documents (split at the first NUL byte) in both directions.
//! A document must never drift from itself.

#![no_main]

use apitest::Contract;
use apitest::contract::Spec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (pinned, live) = match data.iter().position(|&b| b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, data),
    };
    let (Ok(pinned), Ok(live)) = (Spec::from_bytes(pinned, None), Spec::from_bytes(live, None))
    else {
        return;
    };
    let pinned_contract = Contract::from_spec("pinned", pinned.clone());
    let live_contract = Contract::from_spec("live", live.clone());

    let _ = pinned_contract.diff(&live, None, "fuzz");
    let _ = live_contract.diff(&pinned, None, "fuzz");
    let own = pinned_contract.diff(&pinned, None, "self");
    assert!(
        own.findings.is_empty(),
        "self-diff found {:?}",
        own.findings
    );
});
