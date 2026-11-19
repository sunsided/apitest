//! miette diagnostics for terminal output (feature `diagnostics`).
//!
//! Only terminal output goes through here: the CLI and
//! [`SuiteReport::render_terminal`](crate::SuiteReport::render_terminal).
//! `JUnit` XML, the JSON report and the libtest harness messages stay plain text
//! because CI and IDE tooling parses them.

use std::fmt;
use std::io::IsTerminal;

use miette::{Diagnostic, GraphicalReportHandler, GraphicalTheme};

/// Whether output to `stream` should be coloured.
///
/// `CLICOLOR_FORCE` (non-empty, not `0`) forces colour. Otherwise colour needs a
/// terminal that isn't `TERM=dumb`, with [`NO_COLOR`](https://no-color.org)
/// unset or empty and `CLICOLOR` not `0`.
#[must_use]
pub fn use_color(stream: &impl IsTerminal) -> bool {
    color_from_env(stream.is_terminal(), |k| {
        std::env::var_os(k).map(|v| v.to_string_lossy().into_owned())
    })
}

/// The decision behind [`use_color`], with the environment lookup `var` injected
/// so it can be tested without touching the process environment.
fn color_from_env(is_terminal: bool, var: impl Fn(&str) -> Option<String>) -> bool {
    let set = |k: &str| var(k).is_some_and(|v| !v.is_empty());
    let off = |k: &str| var(k).as_deref() == Some("0");
    if set("CLICOLOR_FORCE") && !off("CLICOLOR_FORCE") {
        return true;
    }

    is_terminal && !set("NO_COLOR") && !off("CLICOLOR") && var("TERM").as_deref() != Some("dumb")
}

/// Renders an error and its causes as a miette diagnostic.
///
/// `color` adds ANSI colours; see [`use_color`].
#[must_use]
pub fn render_error(e: &anyhow::Error, color: bool) -> String {
    let chain: Vec<String> = e.chain().map(ToString::to_string).collect();
    render_chain(&format!("{e:#}"), &chain, color)
}

/// Renders a failure from its cause chain, outermost first.
///
/// Without a chain, `message` is the whole error. If miette fails to render,
/// `message` is returned unchanged.
#[must_use]
pub fn render_chain(message: &str, chain: &[String], color: bool) -> String {
    let failure = match chain {
        [] => Failure::new(message, &[]),
        [head, rest @ ..] => Failure::new(head, rest),
    };
    let theme = if color {
        GraphicalTheme::unicode()
    } else {
        GraphicalTheme::unicode_nocolor()
    };
    // Response bodies and diffs in messages read better unwrapped.
    let handler = GraphicalReportHandler::new_themed(theme).with_wrap_lines(false);

    let mut out = String::new();
    if handler.render_report(&mut out, &failure).is_err() {
        return message.to_string();
    }

    out
}

/// One link of a cause chain; [`std::error::Error::source`] points at the next one.
#[derive(Debug)]
struct Failure {
    /// This link's own message, without its causes.
    message: String,
    /// The next, inner cause. `None` for the root cause.
    cause: Option<Box<Failure>>,
}

impl Failure {
    /// Builds the chain `message` → `causes[0]` → `causes[1]` → ...
    fn new(message: &str, causes: &[String]) -> Self {
        Failure {
            message: message.to_string(),
            cause: causes
                .split_first()
                .map(|(head, rest)| Box::new(Failure::new(head, rest))),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_deref()
            .map(|c| c as &(dyn std::error::Error + 'static))
    }
}

impl Diagnostic for Failure {}

#[cfg(test)]
mod tests {
    use anyhow::{Context as _, anyhow};

    use super::{color_from_env, render_chain, render_error};

    /// A three-level chain: step → login → HTTP status.
    fn login_error() -> anyhow::Error {
        Err::<(), _>(anyhow!("HTTP 401: bad credentials"))
            .context("login as 'mallory' on 'svc' failed")
            .context("step 'sign in' failed")
            .unwrap_err()
    }

    /// An environment lookup over fixed `(name, value)` pairs.
    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn renders_the_cause_chain() {
        let out = render_error(&login_error(), false);
        assert!(out.contains("× step 'sign in' failed"), "{out}");
        assert!(
            out.contains("├─▶ login as 'mallory' on 'svc' failed"),
            "{out}"
        );
        assert!(out.contains("╰─▶ HTTP 401: bad credentials"), "{out}");
        assert!(!out.contains('\x1b'), "{out}");
    }

    #[test]
    fn colour_only_on_request() {
        assert!(render_error(&login_error(), true).contains("\x1b["));
    }

    #[test]
    fn colour_follows_the_usual_variables() {
        assert!(color_from_env(true, env(&[])));
        assert!(!color_from_env(false, env(&[])));
        assert!(!color_from_env(true, env(&[("NO_COLOR", "1")])));
        assert!(color_from_env(true, env(&[("NO_COLOR", "")])));
        assert!(!color_from_env(true, env(&[("CLICOLOR", "0")])));
        assert!(!color_from_env(true, env(&[("TERM", "dumb")])));
        assert!(color_from_env(false, env(&[("CLICOLOR_FORCE", "1")])));
        assert!(!color_from_env(false, env(&[("CLICOLOR_FORCE", "0")])));
    }

    #[test]
    fn without_a_chain_the_message_is_the_error() {
        let out = render_chain("suite setup: boom", &[], false);
        assert!(out.contains("× suite setup: boom"), "{out}");
        assert!(!out.contains("─▶"), "{out}");
    }
}
