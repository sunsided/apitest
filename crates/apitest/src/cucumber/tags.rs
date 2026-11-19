//! Gherkin tags: tag expressions (`--tags`), contracts declared on the feature,
//! and scenario tags that become [`TestOpts`].

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use gherkin::tagexpr::TagOperation;

use crate::contract::{Compat, Contract};
use crate::suite::{Concurrency, TestOpts};

/// Longest tag expression [`TagFilter::parse`] accepts, in bytes. The parser
/// and the matcher recurse once per nesting level, so the length bounds the
/// stack they use.
pub const MAX_TAG_EXPRESSION_LEN: usize = 1024;

/// Feature-level tags that configure contracts rather than tests.
const CONTRACT_TAGS: [&str; 3] = ["contract:", "live_spec:", "mode:"];

/// A Cucumber tag expression such as `@smoke and not (@slow or @flaky)`.
///
/// `not` binds tighter than `and`, which binds tighter than `or`.
#[derive(Debug, Clone)]
pub struct TagFilter(TagOperation);

impl TagFilter {
    /// Parses a tag expression.
    ///
    /// # Errors
    ///
    /// Fails when the expression is longer than [`MAX_TAG_EXPRESSION_LEN`]
    /// bytes or is not a valid tag expression.
    pub fn parse(expr: &str) -> Result<TagFilter> {
        if expr.len() > MAX_TAG_EXPRESSION_LEN {
            bail!(
                "tag expression is {} bytes long (at most {MAX_TAG_EXPRESSION_LEN})",
                expr.len()
            );
        }

        expr.parse::<TagOperation>()
            .map(TagFilter)
            .map_err(|e| anyhow!("invalid tag expression '{expr}': {e}"))
    }

    /// Whether a scenario with `tags` is selected. Tags may be given with or
    /// without the leading `@`.
    #[must_use]
    pub fn matches<S: AsRef<str>>(&self, tags: &[S]) -> bool {
        holds(&self.0, tags)
    }
}

/// Evaluates `op` against `tags`.
fn holds<S: AsRef<str>>(op: &TagOperation, tags: &[S]) -> bool {
    match op {
        TagOperation::And(a, b) => holds(a, tags) && holds(b, tags),
        TagOperation::Or(a, b) => holds(a, tags) || holds(b, tags),
        TagOperation::Not(a) => !holds(a, tags),
        TagOperation::Tag(t) => {
            let t = t.trim_start_matches('@');
            tags.iter().any(|x| x.as_ref().trim_start_matches('@') == t)
        }
    }
}

/// The contracts the feature's tags declare: `@contract:NAME=PATH` (relative
/// to `base`), with optional `@live_spec:NAME=URL` and `@mode:NAME=MODE`.
///
/// # Errors
///
/// Fails on a tag without `NAME=VALUE`, an unknown mode, a `@live_spec` or
/// `@mode` without its `@contract`, or a contract that cannot be loaded.
pub(super) fn contracts(tags: &[String], base: &Path) -> Result<Vec<Contract>> {
    let mut pinned = vec![];
    let mut live = vec![];
    let mut modes = vec![];
    for tag in tags {
        if let Some(rest) = tag.strip_prefix("contract:") {
            pinned.push(kv(tag, rest)?);
        } else if let Some(rest) = tag.strip_prefix("live_spec:") {
            live.push(kv(tag, rest)?);
        } else if let Some(rest) = tag.strip_prefix("mode:") {
            let (name, mode) = kv(tag, rest)?;
            modes.push((name, mode.parse::<Compat>()?));
        }
    }

    let named = live
        .iter()
        .map(|(n, _)| *n)
        .chain(modes.iter().map(|(n, _)| *n));
    for name in named {
        if !pinned.iter().any(|(c, _)| *c == name) {
            bail!("@live_spec/@mode name contract '{name}', but there is no @contract:{name}=…");
        }
    }

    pinned
        .into_iter()
        .map(|(name, spec)| {
            let mut c = Contract::pinned(name, crate::script::resolve_relative(base, spec))
                .with_context(|| format!("@contract:{name}"))?;
            if let Some((_, url)) = live.iter().find(|(n, _)| *n == name) {
                c = c.live_spec(*url);
            }

            if let Some((_, m)) = modes.iter().find(|(n, _)| *n == name) {
                c = c.mode(*m);
            }

            Ok(c)
        })
        .collect()
}

/// Scenario tags (feature and rule tags first) → test options. The innermost
/// `@parallel`/`@serial` wins; tags that are no option become test tags.
///
/// # Errors
///
/// Fails on a `@timeout:` that is not a duration.
pub(super) fn test_opts(tags: &[String]) -> Result<TestOpts> {
    let mut opts = TestOpts::default();
    for tag in tags {
        if tag == "parallel" {
            opts.concurrency = Concurrency::Parallel;
        } else if tag == "serial" {
            opts.concurrency = Concurrency::Serial;
        } else if let Some(r) = tag.strip_prefix("resource:") {
            opts.resources.push(r.to_string());
        } else if let Some(t) = tag.strip_prefix("timeout:") {
            opts.timeout = Some(duration(t).with_context(|| format!("@{tag}"))?);
        } else if !CONTRACT_TAGS.iter().any(|p| tag.starts_with(p)) {
            opts.tags.push(tag.clone());
        }
    }

    Ok(opts)
}

/// `5s`, `250ms`, `2m` as a duration; huge values saturate.
///
/// # Errors
///
/// Fails without a unit, on a non-numeric amount or an unknown unit.
pub(super) fn duration(s: &str) -> Result<Duration> {
    let (n, unit) = s
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| s.split_at(i))
        .ok_or_else(|| anyhow!("'{s}' needs a unit (ms, s, m)"))?;
    let n: u64 = n.parse().map_err(|_| anyhow!("'{s}' is not a duration"))?;
    Ok(match unit {
        "ms" | "millisecond" | "milliseconds" => Duration::from_millis(n),
        "s" | "second" | "seconds" => Duration::from_secs(n),
        "m" | "minute" | "minutes" => Duration::from_secs(n.saturating_mul(60)),
        _ => bail!("'{s}': unknown unit '{unit}' (ms, s, m)"),
    })
}

/// `NAME=VALUE` of a tag, both non-empty.
fn kv<'a>(tag: &str, rest: &'a str) -> Result<(&'a str, &'a str)> {
    rest.split_once('=')
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .ok_or_else(|| anyhow!("@{tag}: expected NAME=VALUE"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owned tags from string literals.
    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn tag_expressions() {
        let f = TagFilter::parse("@smoke and not @slow").unwrap();
        assert!(f.matches(&["smoke"]));
        assert!(f.matches(&["@smoke"]));
        assert!(!f.matches(&["smoke", "slow"]));
        assert!(!f.matches(&["other"]));
        assert!(TagFilter::parse("@a not @b").is_err());
        assert!(TagFilter::parse(&"(".repeat(MAX_TAG_EXPRESSION_LEN + 1)).is_err());
    }

    #[test]
    fn tags_become_test_options() {
        let o = test_opts(&tags(&[
            "parallel",
            "resource:db",
            "timeout:5s",
            "smoke",
            "contract:a=b",
        ]))
        .unwrap();
        assert_eq!(o.concurrency, Concurrency::Parallel);
        assert_eq!(o.resources, vec!["db"]);
        assert_eq!(o.timeout, Some(Duration::from_secs(5)));
        assert_eq!(o.tags, vec!["smoke"]);
        assert!(test_opts(&tags(&["timeout:5"])).is_err());
    }

    #[test]
    fn durations_saturate() {
        assert_eq!(duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(
            duration("18446744073709551615m").unwrap(),
            Duration::from_secs(u64::MAX)
        );
        assert!(duration("99999999999999999999s").is_err());
        assert!(duration("5h").is_err());
    }

    #[test]
    fn live_spec_needs_its_contract() {
        let e = contracts(&tags(&["live_spec:x=/openapi.json"]), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(e.contains("no @contract:x"), "{e}");
        assert!(contracts(&tags(&["contract:x"]), Path::new(".")).is_err());
    }
}
