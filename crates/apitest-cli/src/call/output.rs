//! What `apitest call` writes: the response on stdout, the `-v` summary on stderr.

use std::fmt::Write as _;
use std::io::{ErrorKind, Write as _};

use anyhow::{Context as _, Result};

use apitest::Redactor;
use apitest::http::Response;
use apitest::runner::RunConfig;

use super::actor::credential_headers;

/// Writes the body to stdout, after `HTTP <status>` and the response headers
/// with `-i`. A reader that stops early (`| head`) is fine.
///
/// Secrets and redaction patterns are masked here too: a token endpoint's
/// answer must not end up in a terminal or CI log.
///
/// # Errors
///
/// Fails when stdout cannot be written for any reason but a closed pipe.
pub fn write_response(resp: &Response, include: bool, redactor: &Redactor) -> Result<()> {
    match write_to_stdout(resp, include, redactor) {
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => other.context("writing the response to stdout"),
    }
}

/// The unguarded write behind [`write_response`].
fn write_to_stdout(resp: &Response, include: bool, redactor: &Redactor) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    if include {
        write!(out, "HTTP {}\r\n", resp.status)?;
        for (k, v) in &resp.headers {
            write!(out, "{}: {}\r\n", redactor.redact(k), redactor.redact(v))?;
        }

        write!(out, "\r\n")?;
    }

    out.write_all(redactor.redact(&resp.text).as_bytes())?;
    out.flush()
}

/// Request and response lines for `--verbose`, with credential values redacted:
/// cookies, `Authorization`, and with an actor also its credential header and
/// the headers from its `headers:` (they may hold keys too).
pub fn summary(resp: &Response, cfg: &RunConfig, actor: Option<&str>) -> String {
    let mut secret = vec![
        "authorization".to_string(),
        "proxy-authorization".to_string(),
        "cookie".to_string(),
    ];
    if let Some(a) = actor.and_then(|a| cfg.actors.get(a)) {
        secret.extend(credential_headers(cfg, a));
        secret.extend(a.headers.keys().cloned());
    }

    let is_secret = |k: &str| secret.iter().any(|s| s.eq_ignore_ascii_case(k));

    let mut s = format!("> {} {}\n", resp.request.method, resp.request.url);
    for (k, v) in &resp.request.headers {
        let v = if is_secret(k) { redact(v) } else { v.clone() };
        let _ = writeln!(s, "> {k}: {v}");
    }

    let _ = writeln!(s, "< HTTP {} ({:.0} ms)", resp.status, resp.elapsed_ms);
    for (k, v) in &resp.headers {
        let v = if k.eq_ignore_ascii_case("set-cookie") {
            redact(v)
        } else {
            v.clone()
        };
        let _ = writeln!(s, "< {k}: {v}");
    }

    s
}

/// Hides a credential but keeps its auth scheme (`Bearer`, `Basic`).
fn redact(v: &str) -> String {
    match v.split_once(' ') {
        Some((scheme, _)) if scheme.chars().all(|c| c.is_ascii_alphabetic()) => {
            format!("{scheme} ***")
        }
        _ => "***".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redacts_credentials_but_keeps_the_scheme() {
        assert_eq!(redact("Bearer abc.def"), "Bearer ***");
        assert_eq!(redact("Basic dXNlcjpwdw=="), "Basic ***");
        assert_eq!(redact("raw-key"), "***");
        assert_eq!(redact("sid=1; Path=/"), "***");
    }
}
