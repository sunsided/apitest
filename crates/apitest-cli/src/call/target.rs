//! Where `apitest call` may send: a token method, a path on a configured service.
//!
//! The path must start with `/` and is appended to the service's base URL, so a
//! request can never reach a host that is not listed under `services:`.

use anyhow::{Result, bail};

use apitest::runner::RunConfig;

/// Refuses a method that is not an HTTP token and a path that would not be
/// sent as typed (see [`check_path`]).
///
/// # Errors
///
/// Fails with a message naming the bad method or path.
pub fn check_request(method: &str, path: &str) -> Result<()> {
    if reqwest::Method::from_bytes(method.as_bytes()).is_err() {
        bail!(
            "invalid HTTP method '{method}'\n\
             help: a method is a token such as GET, POST or M-SEARCH, without spaces"
        );
    }

    check_path(path)
}

/// Requires a path that starts with a single `/` (an absolute URL would bypass
/// `services:`) and is sent exactly as typed: no fragment, which the URL
/// parser would drop, no control characters, and no `{`/`}`, which the request
/// builder reads as path parameters.
///
/// # Errors
///
/// Fails with a message naming the path and what to write instead.
fn check_path(path: &str) -> Result<()> {
    if !path.starts_with('/') {
        bail!(
            "path '{path}' must start with '/'\n\
             help: `apitest call` only sends to configured services, never to arbitrary URLs, \
             so an actor's credentials can't leak to another host"
        );
    }

    if path.starts_with("//") {
        bail!(
            "path '{path}' starts with '//'\nhelp: the path is appended to the service's URL; start it with a single '/'"
        );
    }

    if path.contains('#') {
        bail!("path '{path}' contains '#', and a fragment is never sent\nhelp: write '#' as %23");
    }

    if path.contains(['{', '}']) {
        bail!(
            "path '{path}' contains '{{' or '}}'\n\
             help: `apitest call` has no path parameters; fill in the value, or write them as %7B and %7D"
        );
    }

    if path.chars().any(char::is_control) {
        bail!("path {path:?} contains a control character\nhelp: percent-encode it");
    }

    Ok(())
}

/// Refuses a service that is not listed under `services:`, or whose URL is not
/// an absolute `http(s)://` URL; `base_url` is never used as a fallback.
/// `source` names the config file in the message.
///
/// # Errors
///
/// Fails when `service` is not configured, listing the services that are, or
/// when its URL is unusable.
pub fn check_service(cfg: &RunConfig, service: &str, source: &str) -> Result<()> {
    if let Some(url) = cfg.services.get(service) {
        return check_service_url(service, url, source);
    }

    let known: Vec<&str> = cfg.services.keys().map(String::as_str).collect();
    if known.is_empty() {
        bail!(
            "unknown service '{service}': {source} has no `services:`\n\
             help: add it, e.g.\n  services:\n    {service}: http://127.0.0.1:8080"
        );
    }

    bail!(
        "unknown service '{service}' (configured in {source}: {})",
        known.join(", ")
    )
}

/// Requires the base URL of `service` to be an absolute `http(s)://` URL with a
/// host, so a typo fails here and not as an opaque error after the body was read.
///
/// # Errors
///
/// Fails naming the service, its URL and `source`.
pub fn check_service_url(service: &str, url: &str, source: &str) -> Result<()> {
    // A query or fragment would swallow the appended path (`/api?t=1` + `/users`).
    let usable = reqwest::Url::parse(url).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && u.has_host()
            && u.query().is_none()
            && u.fragment().is_none()
    });
    if !usable {
        bail!(
            "service '{service}' in {source} has the URL '{url}', which is not an absolute \
             http:// or https:// URL without a query or fragment\n\
             help: e.g.\n  services:\n    {service}: http://127.0.0.1:8080"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use apitest::runner::RunConfig;

    use super::{check_request, check_service};

    #[test]
    fn services_are_never_inferred() {
        let empty = RunConfig::default();
        let err = check_service(&empty, "hub", "apitest.nova")
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no `services:`"), "{err}");

        let cfg = RunConfig::default().service("hub", "http://127.0.0.1:1");
        let err = check_service(&cfg, "app", "apitest.nova")
            .unwrap_err()
            .to_string();
        assert!(err.contains("configured in apitest.nova: hub"), "{err}");
        check_service(&cfg, "hub", "apitest.nova").unwrap();
    }

    #[test]
    fn only_token_methods_and_relative_paths() {
        check_request("get", "/x?y=1").unwrap();
        check_request("M-SEARCH", "/x").unwrap();
        assert!(check_request("GE T", "/x").is_err());
        assert!(check_request("", "/x").is_err());
        assert!(check_request("GET", "https://elsewhere.test/x").is_err());
        assert!(check_request("GET", "x").is_err());
    }

    #[test]
    fn paths_are_sent_as_typed_or_refused() {
        check_request("GET", "/a/b%7Bc%7D?q=%23").unwrap();
        for (path, why) in [
            ("//elsewhere.test/x", "starts with '//'"),
            ("/x#frag", "a fragment is never sent"),
            ("/users/{id}", "has no path parameters"),
            ("/x\r\nHost: y", "control character"),
        ] {
            let e = check_request("GET", path).unwrap_err().to_string();
            assert!(e.contains(why), "{path}: {e}");
        }
    }

    #[test]
    fn service_urls_must_be_absolute_http() {
        for bad in [
            "localhost:8080",
            "",
            "/api",
            "ftp://hub.test",
            "http://",
            "http://hub.test/api?tenant=1",
            "http://hub.test/api#x",
        ] {
            let cfg = RunConfig::default().service("hub", bad);
            let e = check_service(&cfg, "hub", "x.nova")
                .unwrap_err()
                .to_string();
            assert!(
                e.contains(&format!("service 'hub' in x.nova has the URL '{bad}'")),
                "{e}"
            );
        }

        let cfg = RunConfig::default().service("hub", "https://hub.test/api");
        check_service(&cfg, "hub", "x.nova").unwrap();
    }
}
