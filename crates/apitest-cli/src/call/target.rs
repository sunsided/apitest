//! Where `apitest call` may send: a plain method, a path on a configured service.
//!
//! The path must start with `/` and is appended to the service's base URL, so a
//! request can never reach a host that is not listed under `services:`.

use anyhow::{Result, bail};

use apitest::runner::RunConfig;

/// Refuses a method that is not a plain token of letters and a path that does
/// not start with `/` (an absolute URL would bypass `services:`).
///
/// # Errors
///
/// Fails with a message naming the bad method or path.
pub fn check_request(method: &str, path: &str) -> Result<()> {
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic()) {
        bail!("invalid HTTP method '{method}'");
    }

    if !path.starts_with('/') {
        bail!(
            "path '{path}' must start with '/'\n\
             help: `apitest call` only sends to configured services, never to arbitrary URLs, \
             so an actor's credentials can't leak to another host"
        );
    }

    Ok(())
}

/// Refuses a service that is not listed under `services:`; `base_url` is never
/// used as a fallback. `source` names the config file in the message.
///
/// # Errors
///
/// Fails when `service` is not configured, listing the services that are.
pub fn check_service(cfg: &RunConfig, service: &str, source: &str) -> Result<()> {
    if cfg.services.contains_key(service) {
        return Ok(());
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
    fn only_plain_methods_and_relative_paths() {
        check_request("get", "/x?y=1").unwrap();
        assert!(check_request("GE T", "/x").is_err());
        assert!(check_request("", "/x").is_err());
        assert!(check_request("GET", "https://elsewhere.test/x").is_err());
        assert!(check_request("GET", "x").is_err());
    }
}
