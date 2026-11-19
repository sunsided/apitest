//! `apitest run` with variables: the real binary on a Rhai suite that sends no requests.

use std::path::{Path, PathBuf};
use std::process::Output;

/// A temporary project directory, removed when the test ends.
struct Project(PathBuf);

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A project with a `monitor` actor whose API key header is `${KEY}`, and a Rhai
/// suite that prints and logs that variable.
fn project(name: &str) -> Project {
    let dir = std::env::temp_dir().join(format!("apitest-run-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("apitest.nova"),
        "actors:\n  monitor:\n    headers:\n      X-Api-Key: \"${KEY}\"\nsuites:\n  - leak.rhai\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("leak.rhai"),
        r#"
        let s = suite("leak");
        s.test("prints the key", |cx| {
            print("printed " + cx.env("KEY"));
            cx.log("logged " + cx.env("KEY"));
        });
        s
        "#,
    )
    .unwrap();

    Project(dir)
}

/// Runs `apitest run <args>` in `dir`, without colours or `KEY` from the environment.
fn apitest_run(dir: &Path, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_apitest"))
        .current_dir(dir)
        .arg("run")
        .args(args)
        .env_remove("KEY")
        .env_remove("CLICOLOR_FORCE")
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

/// The process's stderr as text.
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn script_print_and_log_mask_actor_secrets() {
    let p = project("mask");
    let o = apitest_run(&p.0, &["--var", "KEY=k-secret-123"]);
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert!(err.contains("[rhai] printed ***"), "{err}");
    assert!(err.contains("logged ***"), "{err}");
    assert!(!err.contains("k-secret-123"), "{err}");
}

#[test]
fn unresolved_variables_stop_the_run_before_any_suite() {
    let p = project("missing");
    let o = apitest_run(&p.0, &[]);
    let err = stderr(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains("KEY (used by actors.monitor.headers.X-Api-Key)"),
        "{err}"
    );
    assert!(!err.contains("printed"), "{err}");
}
