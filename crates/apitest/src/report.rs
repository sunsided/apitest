//! Run results, console output and standard report formats (`JUnit` XML, JSON).
//!
//! Everything here is plain text: [`SuiteReport::render`], [`junit_xml`] and the
//! JSON form (via `serde`) are read by CI systems, IDEs and `cargo test`, so they
//! carry no colours, box drawing or miette glyphs. Terminal output with miette
//! diagnostics is `SuiteReport::render_terminal` (feature `diagnostics`).

use std::fmt::Write as _;

use serde::Serialize;

use crate::contract::{DriftReport, Severity};

/// How a test ended. Serialized as `{"status": "passed" | "failed" | "skipped", ...}`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Outcome {
    /// The body (and its setup) succeeded. Cleanup failures are tracked separately.
    Passed,
    /// Setup, the body or a `before_each` hook failed.
    Failed {
        /// The error with its causes, joined by `": "` (anyhow's `{:#}`), possibly
        /// spanning several lines.
        message: String,
        /// The error and its causes, outermost first, for terminal rendering.
        ///
        /// Not serialized. Empty when the failure didn't come from an error value,
        /// and ignored unless it joins (with `": "`) to `message`; build both with
        /// [`Outcome::failed`].
        #[serde(skip)]
        chain: Vec<String>,
    },
    /// The test never ran, e.g. because suite setup or the drift check failed.
    Skipped {
        /// Why the test was skipped.
        reason: String,
    },
}

impl Outcome {
    /// A failure from an error, keeping its cause chain for terminal rendering.
    #[must_use]
    pub fn failed(e: &anyhow::Error) -> Self {
        Outcome::Failed {
            message: format!("{e:#}"),
            chain: error_chain(e),
        }
    }
}

/// The messages of `e` and its causes, outermost first.
fn error_chain(e: &anyhow::Error) -> Vec<String> {
    e.chain().map(ToString::to_string).collect()
}

/// A named step inside a test (see [`crate::dsl::step`]).
#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    /// Step name as given in the test.
    pub name: String,
    /// Wall-clock duration of the step in milliseconds.
    pub duration_ms: f64,
    /// `None` if the step succeeded.
    pub error: Option<String>,
}

/// Result of one functional test.
#[derive(Debug, Clone, Serialize)]
pub struct TestReport {
    /// Test name inside its suite.
    pub name: String,
    /// Harness id (see [`crate::suite::TestCase::id`]).
    pub id: String,
    /// How the test ended.
    pub outcome: Outcome,
    /// Wall-clock duration in milliseconds, including setup and cleanup.
    pub duration_ms: f64,
    /// Cleanup failures. A test with failed cleanups counts as failed.
    pub teardown_errors: Vec<String>,
    /// Steps in execution order (outer steps after their inner ones).
    pub steps: Vec<StepReport>,
}

impl TestReport {
    /// `✓ step [12 ms]` lines, for failure messages.
    #[must_use]
    pub fn step_trail(&self) -> String {
        self.steps
            .iter()
            .map(|st| {
                format!(
                    "  {} {} [{:.0} ms]",
                    if st.error.is_none() { "✓" } else { "✗" },
                    st.name,
                    st.duration_ms
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `true` if the test passed and all its cleanups succeeded.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.outcome == Outcome::Passed && self.teardown_errors.is_empty()
    }
}

/// Result of one suite run.
#[derive(Debug, Clone, Serialize)]
pub struct SuiteReport {
    /// Suite name.
    pub suite: String,
    /// One report per selected test, in declaration order.
    pub tests: Vec<TestReport>,
    /// Suite-level setup failure (drift, suite fixture, `before_all`).
    pub setup_error: Option<String>,
    /// Cause chain of `setup_error`, outermost first, for terminal rendering.
    ///
    /// Not serialized; empty when the failure didn't come from an error value.
    /// Set both with [`SuiteReport::set_setup_error`].
    #[serde(skip)]
    pub setup_chain: Vec<String>,
    /// Suite-level teardown failures (`after_all`, suite/vu cleanups).
    pub teardown_errors: Vec<String>,
    /// Drift-check results, one per contract and selected drift target it has a spec for.
    pub drift: Vec<DriftReport>,
    /// Wall-clock duration of the whole suite in milliseconds.
    pub duration_ms: f64,
}

impl SuiteReport {
    /// Records a suite-level setup failure together with its cause chain.
    pub fn set_setup_error(&mut self, e: &anyhow::Error) {
        self.setup_error = Some(format!("{e:#}"));
        self.setup_chain = error_chain(e);
    }

    /// `true` if suite setup and teardown succeeded and every test passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.setup_error.is_none()
            && self.teardown_errors.is_empty()
            && self.tests.iter().all(TestReport::passed)
    }

    /// Panics with a readable summary unless everything passed. Handy in `#[tokio::test]`.
    ///
    /// # Panics
    ///
    /// Panics with [`SuiteReport::render`] as the message if [`SuiteReport::passed`]
    /// is `false`.
    #[track_caller]
    pub fn assert_passed(&self) {
        assert!(
            self.passed(),
            "suite '{}' failed:\n{}",
            self.suite,
            self.render()
        );
    }

    /// `(passed, failed, skipped)` test counts. Passed tests with failed cleanups
    /// count as failed.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut c = (0, 0, 0);
        for t in &self.tests {
            match (&t.outcome, t.teardown_errors.is_empty()) {
                (Outcome::Passed, true) => c.0 += 1,
                (Outcome::Skipped { .. }, _) => c.2 += 1,
                _ => c.1 += 1,
            }
        }

        c
    }

    /// Plain-text summary, for logs, panics and anything that isn't a terminal.
    #[must_use]
    pub fn render(&self) -> String {
        self.render_with(&|message, _chain| message.to_string())
    }

    /// Summary for a terminal: like [`render`](Self::render), but failures are
    /// miette diagnostics with their cause chains.
    ///
    /// `color` adds ANSI colours; see [`crate::diagnostic::use_color`].
    #[cfg(feature = "diagnostics")]
    #[must_use]
    pub fn render_terminal(&self, color: bool) -> String {
        self.render_with(&|message, chain| {
            // A chain that disagrees with the message would show different text
            // than render() and JUnit, so fall back to the message alone.
            let chain = if chain.join(": ") == message {
                chain
            } else {
                &[]
            };
            crate::diagnostic::render_chain(message, chain, color)
                .trim_end()
                .to_string()
        })
    }

    /// The summary behind [`render`](Self::render) and `render_terminal`.
    ///
    /// `error(message, chain)` formats one failure; its lines get indented here.
    fn render_with(&self, error: &dyn Fn(&str, &[String]) -> String) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "suite {}", self.suite);
        for d in &self.drift {
            render_drift(&mut s, d);
        }

        if let Some(e) = &self.setup_error {
            render_setup_error(&mut s, e, &error(e, &self.setup_chain));
        }

        for t in &self.tests {
            render_test(&mut s, t, error);
        }

        for e in &self.teardown_errors {
            let _ = writeln!(s, "  ✗ teardown: {e}");
        }

        let (p, f, sk) = self.counts();
        let _ = writeln!(
            s,
            "  {} — {p} passed, {f} failed, {sk} skipped in {:.0} ms",
            if self.passed() { "PASSED" } else { "FAILED" },
            self.duration_ms
        );
        s
    }
}

/// Appends a contract's drift verdict and all its findings to `s`.
fn render_drift(s: &mut String, d: &DriftReport) {
    let verdict = if d.is_breaking() {
        "BREAKING"
    } else {
        "compatible"
    };
    let _ = writeln!(
        s,
        "  contract {}: pinned v{} vs {} v{} ({}) → {verdict}  [{}]",
        d.contract, d.pinned_version, d.target, d.live_version, d.source, d.scope
    );
    for f in &d.findings {
        let tag = match f.severity {
            Severity::Breaking => "✗ breaking",
            Severity::Warning => "! warning ",
            Severity::Info => "· info    ",
        };
        let _ = writeln!(
            s,
            "    {tag} {} @ {}: {}",
            f.operation, f.location, f.message
        );
    }
}

/// Appends the suite setup failure `e`, shown as `rendered`.
///
/// Plain text stays on the status line; a diagnostic goes indented below it.
fn render_setup_error(s: &mut String, e: &str, rendered: &str) {
    if rendered == e {
        let _ = writeln!(s, "  ✗ setup failed: {e}");
    } else {
        let _ = writeln!(s, "  ✗ setup failed\n{}", indent(rendered, "      "));
    }
}

/// Appends a test's status line, and for failures its steps, error and cleanup errors.
///
/// `error(message, chain)` formats the failure, as in [`SuiteReport::render_with`].
fn render_test(s: &mut String, t: &TestReport, error: &dyn Fn(&str, &[String]) -> String) {
    let (mark, inline, failure) = match &t.outcome {
        Outcome::Passed if t.teardown_errors.is_empty() => ("✓", String::new(), None),
        Outcome::Passed => ("✗", " (cleanup failed)".into(), None),
        Outcome::Failed { message, chain } => ("✗", String::new(), Some((message, chain))),
        Outcome::Skipped { reason } => ("-", format!(" (skipped: {reason})"), None),
    };
    let _ = writeln!(s, "  {mark} {} [{:.0} ms]{inline}", t.name, t.duration_ms);
    // For failures, the steps tell the story before the error.
    if !t.passed() {
        for st in &t.steps {
            let m = if st.error.is_none() { "✓" } else { "✗" };
            let _ = writeln!(s, "      {m} {} [{:.0} ms]", st.name, st.duration_ms);
        }
    }

    if let Some((message, chain)) = failure {
        let _ = writeln!(s, "{}", indent(&error(message, chain), "      "));
    }

    for e in &t.teardown_errors {
        let _ = writeln!(s, "      cleanup: {e}");
    }
}

/// The `<testcase>` elements of one suite plus their per-kind counts.
struct SuiteCases<'a> {
    /// The suite the cases belong to.
    suite: &'a SuiteReport,
    /// Rendered `<testcase>` elements, each ending in a newline.
    cases: Vec<String>,
    /// Cases containing a `<failure>`.
    failures: usize,
    /// Cases containing an `<error>`.
    errors: usize,
    /// Cases containing a `<skipped>`.
    skipped: usize,
}

impl<'a> SuiteCases<'a> {
    /// Renders all cases of `suite`: drift checks, setup, tests, teardown.
    fn new(suite: &'a SuiteReport) -> Self {
        let mut cases: Vec<String> = suite
            .drift
            .iter()
            .map(|d| contract_case(&suite.suite, d))
            .collect();
        if let Some(e) = &suite.setup_error {
            cases.push(format!(
                "    <testcase classname=\"{0}\" name=\"[setup]\" time=\"0\">\n      <error type=\"SetupError\" message=\"suite setup failed\">{1}</error>\n    </testcase>\n",
                esc(&suite.suite),
                esc(e)
            ));
        }

        cases.extend(suite.tests.iter().map(|t| test_case(&suite.suite, t)));
        if !suite.teardown_errors.is_empty() {
            cases.push(format!(
                "    <testcase classname=\"{0}\" name=\"[teardown]\" time=\"0\">\n      <error type=\"TeardownError\" message=\"suite teardown failed\">{1}</error>\n    </testcase>\n",
                esc(&suite.suite),
                esc(&suite.teardown_errors.join("\n"))
            ));
        }

        let count = |tag: &str| cases.iter().filter(|c| c.contains(tag)).count();
        let (failures, errors, skipped) = (count("<failure"), count("<error"), count("<skipped"));

        SuiteCases {
            suite,
            cases,
            failures,
            errors,
            skipped,
        }
    }
}

/// Renders suites as a `JUnit` XML document (`<testsuites>`), the de-facto standard
/// understood by GitHub Actions, GitLab, Jenkins, Azure DevOps, IDEs, ...
///
/// Contract drift checks appear as `[contract] <name>` test cases (`<name> @ <target>`
/// for named drift targets), suite-level setup and teardown failures as `[setup]` / `[teardown]` cases, so they show
/// up in CI dashboards like any other failure.
#[must_use]
pub fn junit_xml(suites: &[SuiteReport]) -> String {
    let all: Vec<SuiteCases> = suites.iter().map(SuiteCases::new).collect();
    let total = |f: fn(&SuiteCases) -> usize| all.iter().map(f).sum::<usize>();

    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        x,
        "<testsuites name=\"apitest\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">",
        total(|c| c.cases.len()),
        total(|c| c.failures),
        total(|c| c.errors),
        total(|c| c.skipped),
        suites.iter().map(|s| s.duration_ms).sum::<f64>() / 1000.0
    );
    for c in &all {
        let _ = writeln!(
            x,
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">",
            esc(&c.suite.suite),
            c.cases.len(),
            c.failures,
            c.errors,
            c.skipped,
            c.suite.duration_ms / 1000.0
        );
        for case in &c.cases {
            x.push_str(case);
        }

        x.push_str("  </testsuite>\n");
    }

    x.push_str("</testsuites>\n");
    x
}

/// The `[contract] <name>` case: a `<failure>` listing breaking findings if the
/// drift is breaking, other findings as `<system-out>`.
fn contract_case(suite: &str, d: &DriftReport) -> String {
    let mut c = format!(
        "    <testcase classname=\"{}\" name=\"[contract] {}\" time=\"0\">\n",
        esc(suite),
        esc(&d.label())
    );
    if d.is_breaking() {
        let body: Vec<String> = d
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Breaking)
            .map(|f| format!("{} @ {}: {}", f.operation, f.location, f.message))
            .collect();
        let _ = writeln!(
            c,
            "      <failure type=\"ContractDrift\" message=\"{} spec v{} is incompatible with pinned v{}\">{}</failure>",
            esc(&d.target),
            esc(&d.live_version),
            esc(&d.pinned_version),
            esc(&body.join("\n"))
        );
    }

    let other: Vec<String> = d
        .findings
        .iter()
        .filter(|f| f.severity != Severity::Breaking)
        .map(|f| {
            format!(
                "{:?} {} @ {}: {}",
                f.severity, f.operation, f.location, f.message
            )
        })
        .collect();
    if !other.is_empty() {
        let _ = writeln!(
            c,
            "      <system-out>{}</system-out>",
            esc(&other.join("\n"))
        );
    }

    c.push_str("    </testcase>\n");
    c
}

/// The case of one test: steps as `<system-out>`, then its outcome, then cleanup errors
/// (an `<error>` for otherwise passed tests, `<system-err>` otherwise).
fn test_case(suite: &str, t: &TestReport) -> String {
    let mut c = format!(
        "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\">\n",
        esc(suite),
        esc(&t.name),
        t.duration_ms / 1000.0
    );
    if !t.steps.is_empty() {
        let lines: Vec<String> = t
            .steps
            .iter()
            .map(|st| match &st.error {
                None => format!("✓ {} ({:.0} ms)", st.name, st.duration_ms),
                Some(e) => format!("✗ {} ({:.0} ms): {e}", st.name, st.duration_ms),
            })
            .collect();
        let _ = writeln!(
            c,
            "      <system-out>{}</system-out>",
            esc(&lines.join("\n"))
        );
    }

    match &t.outcome {
        Outcome::Passed => {}
        Outcome::Failed { message, .. } => {
            let first = message.lines().next().unwrap_or_default();
            let _ = writeln!(
                c,
                "      <failure type=\"TestFailure\" message=\"{}\">{}</failure>",
                esc(first),
                esc(message)
            );
        }
        Outcome::Skipped { reason } => {
            let _ = writeln!(c, "      <skipped message=\"{}\"/>", esc(reason));
        }
    }

    if !t.teardown_errors.is_empty() {
        let errors = esc(&t.teardown_errors.join("\n"));
        if t.outcome == Outcome::Passed {
            let _ = writeln!(
                c,
                "      <error type=\"TeardownError\" message=\"cleanup failed\">{errors}</error>"
            );
        } else {
            let _ = writeln!(c, "      <system-err>{errors}</system-err>");
        }
    }

    c.push_str("    </testcase>\n");
    c
}

/// Prefixes every non-empty line of `text` with `by`.
fn indent(text: &str, by: &str) -> String {
    text.lines()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{by}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Escapes `s` for XML text and attribute values, dropping control characters
/// XML 1.0 forbids (everything below U+0020 except tab, newline and carriage return).
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            // XML 1.0 forbids most control characters.
            c if (c as u32) < 0x20 && !matches!(c, '\n' | '\r' | '\t') => {}
            c => o.push(c),
        }
    }

    o
}
