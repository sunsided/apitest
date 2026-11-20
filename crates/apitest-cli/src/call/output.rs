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

/// Registers credentials passed on the command line as secrets of
/// `redactor`, so they are masked wherever they show up (a body that echoes
/// them, an error), not only in the `-v` lines: the values of `-H` headers and
/// query parameters whose names [`is_secret_name`] accepts. `query` holds the
/// `--query` pairs and those written inline in the path.
///
/// Registration uses the word-based test rather than [`is_sensitive`]'s
/// substring match, because a registered value is masked everywhere: a
/// substring match would turn an `X-Idempotency-Key` or a `keyword` search term
/// in the body on stdout into `***`.
pub fn register_secrets(
    redactor: &Redactor,
    headers: &[(String, String)],
    query: &[(String, String)],
) {
    for (_, v) in headers
        .iter()
        .chain(query)
        .filter(|(k, _)| is_secret_name(k))
    {
        register_value(redactor, v);
    }
}

/// Registers `v` and, when it starts with an alphabetic scheme-like word
/// (`Bearer x`, `session abc123`), the part after that word too.
fn register_value(redactor: &Redactor, v: &str) {
    redactor.secret(v);
    if let Some((scheme, credential)) = v.split_once(' ')
        && scheme.chars().all(|c| c.is_ascii_alphabetic())
    {
        redactor.secret(credential.trim());
    }
}

/// Whether a header or query parameter name is a credential's, judged by its
/// words (split at `-`, `_`, `.` and camelCase, ignoring case): one of them is
/// `token`, `secret`, `password`, `passwd`, `credential(s)`, `signature`,
/// `sig`, `session`, `cookie`, `auth`, `authorization` or `apikey`, or the
/// last word is `key` and the name isn't an idempotency key. So `accessToken`,
/// `oauth_signature`, `X-Amz-Signature` and `X-Api-Key` count, while
/// `keyword`, `author`, `monkey`, `X-Key-Id` and `Idempotency-Key` don't.
fn is_secret_name(name: &str) -> bool {
    const WORDS: [&str; 13] = [
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "credentials",
        "signature",
        "sig",
        "session",
        "cookie",
        "auth",
        "authorization",
        "apikey",
    ];
    let words = name_words(name);

    words.iter().any(|w| WORDS.contains(&w.as_str()))
        || (words.last().is_some_and(|w| w == "key") && !words.iter().any(|w| w == "idempotency"))
}

/// The lowercase words of a name, split at non-alphanumeric characters and at
/// camelCase boundaries (`accessToken` → `access`, `token`).
fn name_words(name: &str) -> Vec<String> {
    let mut words = vec![];
    let mut current = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            words.extend((!current.is_empty()).then(|| std::mem::take(&mut current)));
            prev_lower = false;
            continue;
        }

        if c.is_ascii_uppercase() && prev_lower {
            words.push(std::mem::take(&mut current));
        }

        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        current.push(c.to_ascii_lowercase());
    }

    words.extend((!current.is_empty()).then_some(current));
    words
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
    use super::{is_secret_name, is_sensitive, redact, request_url};

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
    fn only_credential_query_names_are_secret_params() {
        for n in [
            "access_token",
            "refresh_token",
            "id_token",
            "token",
            "api_key",
            "apikey",
            "key",
            "password",
            "client_secret",
            "secret",
            "sig",
            "Signature",
            "X-Api-Key",
            "accessToken",
            "clientSecret",
            "authToken",
            "userPassword",
            "oauth_signature",
            "X-Amz-Signature",
            "session",
            "Cookie",
            "credential",
            "Authorization",
            "X-Auth-Token",
        ] {
            assert!(is_secret_name(n), "{n}");
        }

        for n in [
            "q",
            "keyword",
            "author",
            "monkey",
            "page",
            "tokens_left",
            "X-Idempotency-Key",
            "X-Key-Id",
            "oauth_nonce",
        ] {
            assert!(!is_secret_name(n), "{n}");
        }
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
