//! The step registry: [`Steps`], what a step receives ([`StepArgs`]) and how a
//! step text is matched to exactly one definition.
//!
//! The generic library [`Steps::builtin`] fills it with lives in
//! [`super::builtin`].

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use regex::Regex;

use super::world::World;
use crate::suite::BoxFut;

/// What a step definition receives besides the [`World`].
#[derive(Debug, Clone, Default)]
pub struct StepArgs {
    /// The pattern's capture groups (group 1 first), `None` for groups that did
    /// not participate. Raw: `${..}` and `{{..}}` are not resolved yet, see
    /// [`World::interpolate`].
    pub captures: Vec<Option<String>>,
    /// The step's docstring (`"""`), raw, without its media type line.
    pub docstring: Option<String>,
    /// The docstring's media type (`"""json` → `json`), if given.
    pub docstring_type: Option<String>,
    /// The step's data table, raw.
    pub table: Option<Vec<Vec<String>>>,
}

impl StepArgs {
    /// Capture group `n` (1-based, like the regex), which must have matched.
    ///
    /// # Errors
    ///
    /// Fails when the pattern has no group `n` or it did not participate.
    pub fn get(&self, n: usize) -> Result<&str> {
        self.opt(n)
            .ok_or_else(|| anyhow!("step has no capture group {n}"))
    }

    /// Capture group `n` (1-based), or `None` when it did not participate.
    #[must_use]
    pub fn opt(&self, n: usize) -> Option<&str> {
        self.captures
            .get(n.checked_sub(1)?)
            .and_then(|c| c.as_deref())
    }

    /// The docstring, which this step requires.
    ///
    /// # Errors
    ///
    /// Fails when the step has no docstring.
    pub fn docstring(&self) -> Result<&str> {
        self.docstring
            .as_deref()
            .ok_or_else(|| anyhow!("this step needs a docstring (\"\"\" … \"\"\")"))
    }

    /// The data table, which this step requires.
    ///
    /// # Errors
    ///
    /// Fails when the step has no data table.
    pub fn table(&self) -> Result<&[Vec<String>]> {
        self.table
            .as_deref()
            .ok_or_else(|| anyhow!("this step needs a data table (| … |)"))
    }
}

/// A type-erased step implementation.
pub type StepFn = Arc<dyn Fn(World, StepArgs) -> BoxFut<()> + Send + Sync>;

/// One registered step definition.
struct StepDef {
    /// The pattern as given, for ambiguity messages.
    source: String,
    /// The pattern, anchored at both ends.
    re: Regex,
    /// The implementation.
    f: StepFn,
    /// Sends a request and sets the response, so `within …,` retries the step
    /// itself instead of re-sending the scenario's latest request.
    sends: bool,
}

/// A step library. Patterns are regular expressions matched against the whole
/// step text (they are anchored for you), regardless of the Gherkin keyword.
#[derive(Default)]
pub struct Steps {
    /// Definitions in registration order; indexes are stable.
    defs: Vec<StepDef>,
}

impl Steps {
    /// No steps at all.
    #[must_use]
    pub fn empty() -> Self {
        Steps::default()
    }

    /// Adds a step definition. A step text matching more than one definition is
    /// reported as ambiguous when the feature is loaded.
    ///
    /// ```no_run
    /// use apitest::cucumber::Steps;
    ///
    /// # fn demo() -> anyhow::Result<()> {
    /// let steps = Steps::builtin().step(r#"the invoice "([^"]+)" is paid"#, |w, a| async move {
    ///     let id = w.interpolate(a.get(1)?)?;
    ///     let user = w.actor("user")?;
    ///     user.call("payInvoice").path("id", &id).expect(200).await?;
    ///     Ok(())
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Fails when `pattern` is not a valid regular expression.
    pub fn step<F, Fut>(mut self, pattern: &str, f: F) -> Result<Self>
    where
        F: Fn(World, StepArgs) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let re = Regex::new(&format!("^(?:{pattern})$"))
            .with_context(|| format!("invalid step pattern: {pattern}"))?;
        self.defs.push(StepDef {
            source: pattern.to_string(),
            re,
            f: Arc::new(move |w, a| Box::pin(f(w, a))),
            sends: false,
        });

        Ok(self)
    }

    /// Marks the last added step as one that sends a request: `within …, <it>`
    /// then retries the step itself instead of re-sending the previous request.
    #[must_use]
    pub(super) fn sends(mut self) -> Self {
        if let Some(d) = self.defs.last_mut() {
            d.sends = true;
        }

        self
    }

    /// Whether definition `i` sends a request (see [`Steps::sends`]).
    pub(super) fn is_request(&self, i: usize) -> bool {
        self.defs[i].sends
    }

    /// The definition index and captures for a step text.
    ///
    /// # Errors
    ///
    /// Fails when no definition or more than one matches.
    pub(super) fn find(&self, text: &str) -> Result<(usize, Vec<Option<String>>)> {
        let mut hits = self
            .defs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.re.captures(text).map(|c| (i, c)));
        let Some((i, caps)) = hits.next() else {
            bail!("undefined step: {text}");
        };

        let others: Vec<&str> = hits.map(|(j, _)| self.defs[j].source.as_str()).collect();
        if !others.is_empty() {
            bail!(
                "ambiguous step: {text} (matches /{}/ and /{}/)",
                self.defs[i].source,
                others.join("/, /")
            );
        }

        let captures = caps
            .iter()
            .skip(1)
            .map(|m| m.map(|m| m.as_str().to_string()))
            .collect();
        Ok((i, captures))
    }

    /// The implementation of definition `i`.
    pub(super) fn def(&self, i: usize) -> &StepFn {
        &self.defs[i].f
    }

    /// The generic library: actors, contract and raw requests, cleanups,
    /// remembered values and response checks (see `docs/design/cucumber.md`).
    ///
    /// # Panics
    ///
    /// Never in practice: it panics only if a built-in pattern were an invalid
    /// regular expression, which the unit tests rule out.
    #[must_use]
    pub fn builtin() -> Self {
        super::builtin::library(Steps::empty()).expect("built-in step patterns are valid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_are_one_based() {
        let a = StepArgs {
            captures: vec![Some("x".into()), None],
            ..Default::default()
        };
        assert_eq!(a.get(1).unwrap(), "x");
        assert!(a.get(2).is_err());
        assert!(a.get(0).is_err());
        assert_eq!(a.opt(3), None);
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        assert!(Steps::empty().step("(", |_w, _a| async { Ok(()) }).is_err());
    }
}
