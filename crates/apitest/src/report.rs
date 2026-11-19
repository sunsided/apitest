//! Run results, console output and standard report formats (JUnit XML, JSON).

use std::fmt::Write as _;

use serde::Serialize;

use crate::contract::{DriftReport, Severity};

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Outcome {
    Passed,
    Failed { message: String },
    Skipped { reason: String },
}

/// A named step inside a test (see [`crate::dsl::step`]).
#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub name: String,
    pub duration_ms: f64,
    /// `None` if the step succeeded.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestReport {
    pub name: String,
    /// Harness id (see [`crate::suite::TestCase::id`]).
    pub id: String,
    pub outcome: Outcome,
    pub duration_ms: f64,
    /// Cleanup failures. A test with failed cleanups counts as failed.
    pub teardown_errors: Vec<String>,
    /// Steps in execution order (outer steps after their inner ones).
    pub steps: Vec<StepReport>,
}

impl TestReport {
    /// `✓ step [12 ms]` lines, for failure messages.
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

    pub fn passed(&self) -> bool {
        self.outcome == Outcome::Passed && self.teardown_errors.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SuiteReport {
    pub suite: String,
    pub tests: Vec<TestReport>,
    /// Suite-level setup failure (drift, suite fixture, before_all).
    pub setup_error: Option<String>,
    /// Suite-level teardown failures (after_all, suite/vu cleanups).
    pub teardown_errors: Vec<String>,
    pub drift: Vec<DriftReport>,
    pub duration_ms: f64,
}

impl SuiteReport {
    pub fn passed(&self) -> bool {
        self.setup_error.is_none()
            && self.teardown_errors.is_empty()
            && self.tests.iter().all(TestReport::passed)
    }

    /// Panics with a readable summary unless everything passed. Handy in `#[tokio::test]`.
    #[track_caller]
    pub fn assert_passed(&self) {
        if !self.passed() {
            panic!("suite '{}' failed:\n{}", self.suite, self.render());
        }
    }

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

    /// Human-readable summary.
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "suite {}", self.suite);
        for d in &self.drift {
            let verdict = if d.is_breaking() {
                "BREAKING"
            } else {
                "compatible"
            };
            let _ = writeln!(
                s,
                "  contract {}: pinned v{} vs live v{} ({}) → {verdict}  [{}]",
                d.contract, d.pinned_version, d.live_version, d.source, d.scope
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
        if let Some(e) = &self.setup_error {
            let _ = writeln!(s, "  ✗ setup failed: {e}");
        }
        for t in &self.tests {
            let (mark, inline, failure) = match &t.outcome {
                Outcome::Passed if t.teardown_errors.is_empty() => ("✓", String::new(), None),
                Outcome::Passed => ("✗", " (cleanup failed)".into(), None),
                Outcome::Failed { message } => ("✗", String::new(), Some(message)),
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
            if let Some(msg) = failure {
                let _ = writeln!(s, "      {}", msg.replace('\n', "\n      "));
            }
            for e in &t.teardown_errors {
                let _ = writeln!(s, "      cleanup: {e}");
            }
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

/// Renders suites as a JUnit XML document (`<testsuites>`), the de-facto standard
/// understood by GitHub Actions, GitLab, Jenkins, Azure DevOps, IDEs, ...
///
/// Contract drift checks appear as `[contract] <name>` test cases, suite-level
/// setup and teardown failures as `[setup]` / `[teardown]` cases, so they show
/// up in CI dashboards like any other failure.
pub fn junit_xml(suites: &[SuiteReport]) -> String {
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let mut all_cases = vec![];
    for s in suites {
        let mut cases: Vec<String> = vec![];
        for d in &s.drift {
            let mut c = format!(
                "    <testcase classname=\"{}\" name=\"[contract] {}\" time=\"0\">\n",
                esc(&s.suite),
                esc(&d.contract)
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
                    "      <failure type=\"ContractDrift\" message=\"live spec v{} is incompatible with pinned v{}\">{}</failure>",
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
            cases.push(c);
        }
        if let Some(e) = &s.setup_error {
            cases.push(format!(
                "    <testcase classname=\"{0}\" name=\"[setup]\" time=\"0\">\n      <error type=\"SetupError\" message=\"suite setup failed\">{1}</error>\n    </testcase>\n",
                esc(&s.suite),
                esc(e)
            ));
        }
        for t in &s.tests {
            let mut c = format!(
                "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\">\n",
                esc(&s.suite),
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
                Outcome::Failed { message } => {
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
                if t.outcome == Outcome::Passed {
                    let _ = writeln!(
                        c,
                        "      <error type=\"TeardownError\" message=\"cleanup failed\">{}</error>",
                        esc(&t.teardown_errors.join("\n"))
                    );
                } else {
                    let _ = writeln!(
                        c,
                        "      <system-err>{}</system-err>",
                        esc(&t.teardown_errors.join("\n"))
                    );
                }
            }
            c.push_str("    </testcase>\n");
            cases.push(c);
        }
        if !s.teardown_errors.is_empty() {
            cases.push(format!(
                "    <testcase classname=\"{0}\" name=\"[teardown]\" time=\"0\">\n      <error type=\"TeardownError\" message=\"suite teardown failed\">{1}</error>\n    </testcase>\n",
                esc(&s.suite),
                esc(&s.teardown_errors.join("\n"))
            ));
        }
        let failures = cases.iter().filter(|c| c.contains("<failure")).count();
        let errors = cases.iter().filter(|c| c.contains("<error")).count();
        let skipped = cases.iter().filter(|c| c.contains("<skipped")).count();
        all_cases.push((s, cases, failures, errors, skipped));
    }
    type Row<'a> = (&'a SuiteReport, Vec<String>, usize, usize, usize);
    let tot = |f: fn(&Row) -> usize| all_cases.iter().map(f).sum::<usize>();
    let _ = writeln!(
        x,
        "<testsuites name=\"apitest\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">",
        tot(|c| c.1.len()),
        tot(|c| c.2),
        tot(|c| c.3),
        tot(|c| c.4),
        suites.iter().map(|s| s.duration_ms).sum::<f64>() / 1000.0
    );
    for (s, cases, failures, errors, skipped) in &all_cases {
        let _ = writeln!(
            x,
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\" time=\"{:.3}\">",
            esc(&s.suite),
            cases.len(),
            s.duration_ms / 1000.0
        );
        for c in cases {
            x.push_str(c);
        }
        x.push_str("  </testsuite>\n");
    }
    x.push_str("</testsuites>\n");
    x
}

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
