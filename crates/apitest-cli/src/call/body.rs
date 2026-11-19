//! The request body of `apitest call`: `--json`, `--data` or `--form`.
//!
//! Bodies are UTF-8 text. `--json` and `--data` take the text inline, from
//! `@file`, or from stdin with `@-`; `@@` starts a literal `@`.

use std::io::Read as _;

use anyhow::{Context as _, Result};
use serde_json::Value;

use apitest::http::RequestOpts;

/// Body flags; at most one of them may be given.
#[derive(clap::Args)]
#[group(multiple = false)]
pub struct Body {
    /// JSON body: inline, @file, or @- for stdin (`@@` starts a literal `@`).
    #[arg(long, value_name = "JSON")]
    json: Option<String>,
    /// UTF-8 text body: inline, @file, or @- for stdin (`@@` starts a literal `@`).
    /// Set Content-Type with -H.
    #[arg(long, value_name = "TEXT")]
    data: Option<String>,
    /// Form field, sent as application/x-www-form-urlencoded (repeatable).
    #[arg(long, value_name = "KEY=VALUE", value_parser = crate::parse_kv)]
    form: Vec<(String, String)>,
}

impl Body {
    /// Puts the body, if any, into `opts`.
    ///
    /// # Errors
    ///
    /// Fails when a `@file` or stdin cannot be read as UTF-8, or `--json` is not
    /// valid JSON.
    pub fn apply(&self, opts: &mut RequestOpts) -> Result<()> {
        if let Some(j) = &self.json {
            let text = read_arg(j)?;
            opts.json = Some(serde_json::from_str(&text).context("--json is not valid JSON")?);
        } else if let Some(d) = &self.data {
            opts.body = Some(read_arg(d)?);
        } else if !self.form.is_empty() {
            opts.form = Some(
                self.form
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            );
        }

        Ok(())
    }
}

/// Resolves a body argument: `@file`, `@-` (stdin), `@@…` (literal text that
/// starts with `@`) or literal text.
///
/// # Errors
///
/// Fails when the file or stdin cannot be read as UTF-8.
fn read_arg(v: &str) -> Result<String> {
    match v.strip_prefix('@') {
        Some(rest) if rest.starts_with('@') => Ok(rest.to_string()),
        Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("reading the body from stdin")?;
            Ok(s)
        }
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {path}")),
        None => Ok(v.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::read_arg;

    #[test]
    fn at_at_is_a_literal_at() {
        assert_eq!(read_arg("@@handle").unwrap(), "@handle");
        assert_eq!(read_arg("plain").unwrap(), "plain");
    }
}
