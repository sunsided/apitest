//! Load time: scenarios and outline rows become test names, and their steps
//! are matched against the step library.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use gherkin::{Scenario, Step};

use super::steps::{StepArgs, Steps};

/// One Gherkin step, ready to run: matched against a definition at load time.
#[derive(Clone)]
pub(super) struct PlannedStep {
    /// `Given "user" calls ...`, the text as written: placeholders of an
    /// outline filled in, `${..}` and `{{..}}` not yet. Those are resolved when
    /// the step runs, so secrets never end up in step names.
    pub label: String,
    /// `file:line`, added to the step's error.
    pub location: String,
    /// Index of the matched definition in the [`Steps`].
    pub def: usize,
    /// Captures, docstring and table.
    pub args: StepArgs,
    /// `within 10 seconds, …`: retry until it passes or the time is up.
    pub within: Option<Within>,
}

/// How a `within …,` step is retried.
#[derive(Clone, Copy)]
pub(super) struct Within {
    /// Total time for all attempts.
    pub timeout: Duration,
    /// The inner step does not send a request itself, so it is a check: re-send
    /// the scenario's latest request before each new attempt. Otherwise just
    /// the step is retried.
    pub resend: bool,
}

/// One test a scenario expands to: the whole scenario, or one outline row.
pub(super) struct Instance {
    /// The test name; outline rows add their values, e.g. `get 1 (id=1)`.
    pub name: String,
    /// `<placeholder>` values of an outline row.
    pub values: Vec<(String, String)>,
    /// Tags of the row's `Examples` block.
    pub tags: Vec<String>,
}

/// The tests a scenario expands to: itself, or one per row of each `Examples`
/// table of an outline.
///
/// # Errors
///
/// Fails when a row has a different number of cells than the table header.
pub(super) fn expand(sc: &Scenario) -> Result<Vec<Instance>> {
    let name = sc.name.trim().to_string();
    if sc.examples.is_empty() {
        return Ok(vec![Instance {
            name,
            values: vec![],
            tags: vec![],
        }]);
    }

    let mut out = vec![];
    for ex in &sc.examples {
        let Some((header, rows)) = ex.table.as_ref().and_then(|t| t.rows.split_first()) else {
            continue;
        };

        for row in rows {
            if row.len() != header.len() {
                bail!(
                    "examples of '{name}' (line {}): a row has {} cells, the header {}",
                    ex.position.line,
                    row.len(),
                    header.len()
                );
            }

            let values: Vec<(String, String)> =
                header.iter().cloned().zip(row.iter().cloned()).collect();
            let shown: Vec<String> = values.iter().map(|(k, v)| format!("{k}={v}")).collect();
            out.push(Instance {
                name: format!("{} ({})", fill(&name, &values), shown.join(", ")),
                values,
                tags: ex.tags.clone(),
            });
        }
    }

    Ok(out)
}

/// Matches `steps` (background first) of one instance against the library.
/// Each problem (undefined, ambiguous, a malformed `within`) is pushed to
/// `problems` as `file:line: message`, so the caller can report all of them.
pub(super) fn plan<'a>(
    steps: impl Iterator<Item = &'a Step>,
    values: &[(String, String)],
    file: &str,
    library: &Steps,
    problems: &mut Vec<String>,
) -> Vec<PlannedStep> {
    let mut planned = vec![];
    for s in steps {
        let s = substitute(s, values);
        let location = format!("{file}:{}", s.position.line);
        match plan_step(&s, location.clone(), library) {
            Ok(p) => planned.push(p),
            Err(e) => problems.push(format!("{location}: {e}")),
        }
    }

    planned
}

/// Matches one (substituted) step against the library.
fn plan_step(s: &Step, location: String, library: &Steps) -> Result<PlannedStep> {
    let (text, timeout) = match within(&s.value)? {
        Some((d, inner)) => (inner, Some(d)),
        None => (s.value.as_str(), None),
    };
    let (def, captures) = library.find(text)?;
    let (docstring_type, docstring) = match s.docstring.as_deref().map(docstring) {
        Some((media, content)) => (media, Some(content)),
        None => (None, None),
    };

    Ok(PlannedStep {
        label: format!("{} {}", s.keyword.trim(), s.value),
        location,
        def,
        args: StepArgs {
            captures,
            docstring,
            docstring_type,
            table: s.table.as_ref().map(|t| t.rows.clone()),
        },
        within: timeout.map(|timeout| Within {
            timeout,
            resend: !library.is_request(def),
        }),
    })
}

/// `within 10 seconds, <step>` → (10 s, `<step>`).
///
/// # Errors
///
/// Fails when a step starting with `within ` does not continue with an
/// amount, a unit and `, `.
fn within(text: &str) -> Result<Option<(Duration, &str)>> {
    let Some(rest) = text.strip_prefix("within ") else {
        return Ok(None);
    };

    let usage = || anyhow!("expected `within <n> seconds, <step>`");
    let (amount, inner) = rest.split_once(", ").ok_or_else(usage)?;
    let (n, unit) = amount.split_once(' ').ok_or_else(usage)?;
    let d = super::tags::duration(&format!("{n}{unit}"))
        .map_err(|e| anyhow!("`within {amount}`: {e} (milliseconds, seconds, minutes)"))?;
    Ok(Some((d, inner)))
}

/// `<placeholder>`s of an outline row filled into `s`.
fn fill(s: &str, values: &[(String, String)]) -> String {
    values
        .iter()
        .fold(s.to_string(), |s, (k, v)| s.replace(&format!("<{k}>"), v))
}

/// Fills `<placeholder>`s of an outline into a step's text, docstring and table.
fn substitute(step: &Step, values: &[(String, String)]) -> Step {
    let mut out = step.clone();
    if values.is_empty() {
        return out;
    }

    out.value = fill(&step.value, values);
    out.docstring = step.docstring.as_deref().map(|d| fill(d, values));
    if let Some(t) = &mut out.table {
        for cell in t.rows.iter_mut().flatten() {
            *cell = fill(cell, values);
        }
    }

    out
}

/// Splits a docstring as the parser keeps it, everything between the
/// delimiters, into the media type (`"""json`) on the first line and the
/// content, without the closing delimiter's indentation at the end.
fn docstring(raw: &str) -> (Option<String>, String) {
    let (first, rest) = raw.split_once('\n').unwrap_or((raw, ""));
    let media = first.trim();
    let mut content = rest.to_string();
    if let Some(i) = content.rfind('\n')
        && content[i + 1..].trim().is_empty()
    {
        content.truncate(i);
    }

    ((!media.is_empty()).then(|| media.to_string()), content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docstrings_lose_media_type_and_delimiter_lines() {
        assert_eq!(
            docstring("json\n{ \"a\": 1 }\n  "),
            (Some("json".into()), "{ \"a\": 1 }".into())
        );
        assert_eq!(
            docstring("\nline 1\nline 2\n"),
            (None, "line 1\nline 2".into())
        );
    }

    #[test]
    fn within_parses_amount_and_unit() {
        let (d, inner) = within("within 10 seconds, the status is 200")
            .unwrap()
            .unwrap();
        assert_eq!((d, inner), (Duration::from_secs(10), "the status is 200"));
        assert_eq!(
            within("within 1 minute, x").unwrap().unwrap().0,
            Duration::from_secs(60)
        );
        assert!(within("within 10 parsecs, x").is_err());
        assert!(within("within ten seconds, x").is_err());
        assert!(within("within 10 seconds").is_err());
        assert!(within("the status is 200").unwrap().is_none());
    }
}
