//! What `apitest call` writes: the response on stdout, the `-v` summary on stderr.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{ErrorKind, Write as _};

use anyhow::{Context as _, Result};

use apitest::Redactor;
use apitest::http::Response;
use apitest::runner::RunConfig;

use super::actor::credential_headers;

/// Writes the body to stdout as received, after `HTTP <status>` and the
/// response headers with `-i`. A reader that stops early (`| head`) is fine.
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

    out.write_all(&masked_body(resp, redactor))?;
    out.flush()
}

/// The body to write: the bytes as received, so binary downloads stay intact,
/// unless a secret or redaction pattern matches it. Then it is the masked text,
/// and a binary body that holds a secret loses its exact bytes.
fn masked_body<'a>(resp: &'a Response, redactor: &Redactor) -> Cow<'a, [u8]> {
    let masked = redactor.redact(&resp.text);
    if masked == resp.text {
        Cow::Borrowed(&resp.bytes)
    } else {
        Cow::Owned(masked.into_bytes())
    }
}

/// Request and response lines for `--verbose`, with credentials redacted: any
/// header or query parameter [`is_sensitive`] by name, and with an actor also
/// its credential header and the headers from its `headers:` (they may hold
/// keys too). `query` is the `--query` pairs, which the sent URL lacks.
pub fn summary(
    resp: &Response,
    cfg: &RunConfig,
    actor: Option<&str>,
    query: &[(String, String)],
) -> String {
    let mut secret = vec![];
    if let Some(a) = actor.and_then(|a| cfg.actors.get(a)) {
        secret.extend(credential_headers(cfg, a));
        secret.extend(a.headers.keys().cloned());
    }

    let shown = |k: &str, v: &str| {
        if is_sensitive(k) || secret.iter().any(|s| s.eq_ignore_ascii_case(k)) {
            redact(k, v)
        } else {
            v.to_string()
        }
    };

    let mut s = format!(
        "> {} {}\n",
        resp.request.method,
        request_url(&resp.request.url, query)
    );
    for (k, v) in &resp.request.headers {
        let _ = writeln!(s, "> {k}: {}", shown(k, v));
    }

    let _ = writeln!(s, "< HTTP {} ({:.0} ms)", resp.status, resp.elapsed_ms);
    for (k, v) in &resp.headers {
        let _ = writeln!(s, "< {k}: {}", shown(k, v));
    }

    s
}

/// Registers the values of `-H` headers whose names are [`is_sensitive`] as
/// secrets of `redactor`, so they are masked wherever they show up (a body that
/// echoes them, an error), not only in the `-v` lines. An `Authorization`
/// value registers its credential without the scheme too.
///
/// `--query` values are only redacted in `-v`: the name test is a substring
/// match (`keyword` contains `key`), and masking a search term in the body on
/// stdout would garble the data.
pub fn register_secrets(redactor: &Redactor, headers: &[(String, String)]) {
    for (k, v) in headers.iter().filter(|(k, _)| is_sensitive(k)) {
        redactor.secret(v);
        if is_authorization(k)
            && let Some((_, credential)) = v.split_once(' ')
        {
            redactor.secret(credential.trim());
        }
    }
}

/// Whether `name` is `Authorization` or `Proxy-Authorization`, whose values
/// start with an auth scheme.
fn is_authorization(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("proxy-authorization")
}

/// Whether a header or query parameter name suggests a credential: it contains
/// `auth`, `cookie`, `token`, `key`, `secret`, `session`, `password` or
/// `credential`, ignoring case (`Authorization`, `Set-Cookie`, `X-Api-Key`,
/// `access_token`, ...).
fn is_sensitive(name: &str) -> bool {
    const PARTS: [&str; 8] = [
        "auth",
        "cookie",
        "token",
        "key",
        "secret",
        "session",
        "password",
        "credential",
    ];
    let name = name.to_ascii_lowercase();

    PARTS.iter().any(|p| name.contains(p))
}

/// The URL as sent, with the `--query` pairs appended, sensitive parameters
/// redacted and any `user:password@` removed.
fn request_url(sent: &str, query: &[(String, String)]) -> String {
    let Ok(mut url) = reqwest::Url::parse(sent) else {
        return sent.to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);

    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .into_owned()
        .chain(query.iter().cloned())
        .collect();
    if !pairs.is_empty() {
        url.query_pairs_mut().clear().extend_pairs(
            pairs
                .iter()
                .map(|(k, v)| (k, if is_sensitive(k) { "***" } else { v.as_str() })),
        );
    }

    url.into()
}

/// Hides a credential. Only an `Authorization` or `Proxy-Authorization` value
/// keeps its auth scheme (`Bearer ***`); anything else becomes `***`, since a
/// first word there may be part of the secret.
fn redact(name: &str, v: &str) -> String {
    match v.split_once(' ') {
        Some((scheme, _))
            if is_authorization(name) && scheme.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            format!("{scheme} ***")
        }
        _ => "***".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_sensitive, redact, request_url};

    #[test]
    fn only_authorization_keeps_its_scheme() {
        assert_eq!(redact("Authorization", "Bearer abc.def"), "Bearer ***");
        assert_eq!(
            redact("proxy-authorization", "Basic dXNlcjpwdw=="),
            "Basic ***"
        );
        assert_eq!(redact("Authorization", "raw-key"), "***");
        assert_eq!(redact("X-Session", "session abc"), "***");
        assert_eq!(redact("Cookie", "sid=1; Path=/"), "***");
    }

    #[test]
    fn credential_like_names_are_sensitive() {
        for n in [
            "Authorization",
            "Proxy-Authorization",
            "Cookie",
            "set-cookie",
            "X-Api-Key",
            "X-Auth-Token",
            "X-Session",
            "client_secret",
            "password",
            "X-Credentials",
        ] {
            assert!(is_sensitive(n), "{n}");
        }

        for n in ["Content-Type", "X-Tenant", "Accept", "page"] {
            assert!(!is_sensitive(n), "{n}");
        }
    }

    #[test]
    fn the_request_url_shows_the_query_without_secrets() {
        let q = [
            ("page".to_string(), "2".to_string()),
            ("api_key".to_string(), "k".to_string()),
        ];
        assert_eq!(
            request_url("http://user:pw@hub.test/x?a=1&token=t", &q),
            "http://hub.test/x?a=1&token=***&page=2&api_key=***"
        );
        assert_eq!(request_url("http://hub.test/x", &[]), "http://hub.test/x");
    }
}
