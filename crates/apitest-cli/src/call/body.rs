//! The request body of `apitest call`: `--json`, `--data` or `--form`.
//!
//! Bodies are UTF-8 text of at most 16 MiB. `--json` and `--data` take the
//! text inline, from `@file`, or from stdin with `@-`; `@@` starts a literal `@`.
//! The body is read before the request is sent, so `--timeout` does not cover
//! waiting for stdin.

use std::fs::File;
use std::io::{IsTerminal as _, Read};

use anyhow::{Context as _, Result, anyhow, bail};
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
    /// Fails when a `@file` or stdin cannot be read as UTF-8 or is larger than
    /// [`MAX_BODY`], or `--json` is not valid JSON.
    pub fn apply(&self, opts: &mut RequestOpts) -> Result<()> {
        if let Some(j) = &self.json {
            let text = read_arg(j, stdin(j))?;
            opts.json = Some(serde_json::from_str(&text).context("--json is not valid JSON")?);
        } else if let Some(d) = &self.data {
            opts.body = Some(read_arg(d, stdin(d))?);
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

/// The largest body read from a file or stdin: 16 MiB.
const MAX_BODY: u64 = 16 * 1024 * 1024;

/// Stdin for the body argument `arg`. When `arg` is `@-` and stdin is a
/// terminal, says how to end the input, and that `--timeout` only starts once
/// it is read.
fn stdin(arg: &str) -> impl Read {
    let stdin = std::io::stdin();
    if arg == "@-" && stdin.is_terminal() {
        eprintln!(
            "reading the body from the terminal; end it with Ctrl-D \
             (--timeout does not cover this)"
        );
    }

    stdin
}

/// Resolves a body argument: `@file`, `@-` (read from `stdin`), `@@…` (literal
/// text that starts with `@`) or literal text. `stdin` is only read for `@-`.
///
/// # Errors
///
/// Fails on a bare `@`, or when the file or stdin cannot be read, is not UTF-8,
/// or is larger than [`MAX_BODY`].
fn read_arg(v: &str, stdin: impl Read) -> Result<String> {
    match v.strip_prefix('@') {
        None => Ok(v.to_string()),
        Some(rest) if rest.starts_with('@') => Ok(rest.to_string()),
        Some("") => bail!(
            "'@' needs a file name\n\
             help: use @file, @- for stdin, or @@ for text that starts with '@'"
        ),
        Some("-") => read_capped(stdin, "stdin"),
        Some(path) => {
            let file = File::open(path).with_context(|| format!("reading {path}"))?;
            read_capped(file, path)
        }
    }
}

/// Reads all of `r` (named `what` in errors) as UTF-8, up to [`MAX_BODY`].
///
/// # Errors
///
/// Fails when reading fails, the input is larger than [`MAX_BODY`] or is not UTF-8.
fn read_capped(r: impl Read, what: &str) -> Result<String> {
    let mut buf = Vec::new();
    r.take(MAX_BODY + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("reading {what}"))?;
    if buf.len() as u64 > MAX_BODY {
        bail!("{what} is larger than 16 MiB, the limit for a request body");
    }

    String::from_utf8(buf).map_err(|_| anyhow!("{what} is not UTF-8 text; bodies are text"))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use super::{MAX_BODY, read_arg};

    /// A stdin that fails the test when it is read.
    struct Untouched;

    impl Read for Untouched {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("stdin was read");
        }
    }

    /// The error message of a `read_arg` that must fail.
    fn err(v: &str, stdin: impl Read) -> String {
        read_arg(v, stdin).unwrap_err().to_string()
    }

    #[test]
    fn literal_text_and_at_at() {
        assert_eq!(read_arg("@@handle", Untouched).unwrap(), "@handle");
        assert_eq!(read_arg("plain", Untouched).unwrap(), "plain");
        assert_eq!(read_arg("", Untouched).unwrap(), "");
    }

    #[test]
    fn a_bare_at_is_refused() {
        assert!(err("@", Untouched).contains("@@ for text"));
    }

    #[test]
    fn at_dash_reads_stdin() {
        assert_eq!(
            read_arg("@-", Cursor::new("{\"a\":1}")).unwrap(),
            "{\"a\":1}"
        );
        assert_eq!(read_arg("@-", Cursor::new("")).unwrap(), "");
        assert!(err("@-", Cursor::new(b"\xff\xfe")).contains("stdin is not UTF-8"));

        let huge = std::io::repeat(b'a').take(MAX_BODY + 1);
        assert!(err("@-", huge).contains("larger than 16 MiB"));
        let at_limit = std::io::repeat(b'a').take(MAX_BODY);
        assert_eq!(read_arg("@-", at_limit).unwrap().len(), 16 * 1024 * 1024);
    }

    #[test]
    fn at_file_reads_the_file() {
        let dir = std::env::temp_dir().join(format!("apitest-body-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (text, binary) = (dir.join("text.json"), dir.join("binary"));
        std::fs::write(&text, "{}").unwrap();
        std::fs::write(&binary, b"\x89PNG\xff").unwrap();

        assert_eq!(
            read_arg(&format!("@{}", text.display()), Untouched).unwrap(),
            "{}"
        );
        assert!(err(&format!("@{}", binary.display()), Untouched).contains("is not UTF-8"));
        assert!(err(&format!("@{}", dir.join("missing").display()), Untouched).contains("reading"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
