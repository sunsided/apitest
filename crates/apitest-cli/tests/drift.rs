//! `apitest contract check --against`: the real binary in the demo project,
//! comparing against the drift targets from its `apitest.nova` without a service.

use std::path::PathBuf;
use std::process::Output;

/// The demo project, whose `apitest.nova` defines the targets `qa` (1.1) and `next` (2.0).
fn demo_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../demo")
}

/// Runs `apitest contract check <args>` in the demo project, without colours.
/// The base URL points where nothing listens, so any live fetch would fail.
fn contract_check(args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_apitest"))
        .current_dir(demo_dir())
        .args(["contract", "check", "-u", "http://127.0.0.1:9"])
        .args(args)
        .env_remove("CLICOLOR_FORCE")
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

/// stdout and stderr of `o` as text.
fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

/// `qa` is compatible and `next` breaking; the breaking target fails the check (exit 1).
#[test]
fn named_targets_are_checked_offline() {
    let o = contract_check(&["--against", "qa,next"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(
        out.contains("billing-rhai / billing: pinned v1.0.0 vs qa v1.1.0"),
        "{out}"
    );
    assert!(
        out.lines()
            .any(|l| l.contains("vs qa v1.1.0") && l.ends_with("→ compatible"))
    );
    assert!(
        out.lines()
            .any(|l| l.contains("vs next v2.0.0") && l.ends_with("→ BREAKING"))
    );
    assert!(!out.contains("vs live"), "{out}");
}

/// A compatible target alone passes (exit 0).
#[test]
fn compatible_target_passes() {
    let o = contract_check(&["--against", "qa"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
}

/// An unknown target name is a usage error (exit 2) that lists the known ones.
#[test]
fn unknown_target_is_an_error() {
    let o = contract_check(&["--against", "prod"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(
        err.contains("unknown drift target 'prod' (known: live, all, next, qa)"),
        "{err}"
    );
}

/// A one-off spec for a contract no suite has is caught before anything runs (exit 2).
#[test]
fn spec_for_unknown_contract_is_an_error() {
    let o = contract_check(&["--against-spec", "biling=specs/billing-1.1.yaml"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(
        err.contains("drift target 'cli' has a spec for unknown contract 'biling'"),
        "{err}"
    );
}

/// `--against-spec` checks only the one-off target `cli` by default.
#[test]
fn one_off_spec_is_checked_alone() {
    let o = contract_check(&["--against-spec", "billing=specs/billing-1.1.yaml"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    assert!(out.contains("vs cli v1.1.0"), "{out}");
}

/// Drift output goes through the same masking as reports.
#[test]
fn drift_output_is_masked() {
    let o = contract_check(&["--against", "qa", "--redact", r"1\.1\.0"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}");
    assert!(out.contains("pinned v1.0.0 vs qa v***"), "{out}");
    assert!(!out.contains("1.1.0"), "{out}");
}
