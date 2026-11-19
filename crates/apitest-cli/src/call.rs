//! `apitest call`: one request to a configured service, without an `OpenAPI` spec.
//!
//! ```text
//! apitest call hub GET /users/me --as hub_admin
//! apitest call hub POST /users --as hub_admin --json '{"email":"a@b.test"}'
//! apitest call mail GET /messages --query to=a@b.test -i
//! ```
//!
//! Nothing is guessed. The service must be listed under `services:`, the actor
//! under `actors:`, and an actor that logs in needs an explicit login flow with
//! a `path` under `auth:`. Without `--as` the request carries no credentials.
//!
//! ```mermaid
//! flowchart LR
//!     A[parse flags] --> B[load apitest.nova]
//!     B --> C[check method, path, service, actor]
//!     C --> D[build request]
//!     D --> E[send with timeout]
//!     E --> F[stdout: body, stderr: -v summary]
//!     F --> G{status as expected?}
//! ```

mod actor;
mod body;
mod output;
mod target;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;

use apitest::config::ProjectConfig;
use apitest::http::{RequestOpts, Response};
use apitest::runner::RunConfig;
use apitest::{Ctx, Redactor, load};

use crate::RequestFlags;

/// Arguments of `apitest call`.
#[derive(clap::Args)]
pub struct Args {
    /// Service name from `services:` in apitest.nova.
    service: String,
    /// HTTP method, e.g. GET, POST, DELETE.
    method: String,
    /// Path on the service, starting with `/` (query strings are fine).
    path: String,
    /// Send as a named actor from `actors:` (its service, headers and credentials).
    #[arg(long = "as", value_name = "ACTOR")]
    actor: Option<String>,
    /// Request header, e.g. -H 'X-Tenant: 42' (repeatable).
    #[arg(long, short = 'H', value_name = "NAME: VALUE", value_parser = parse_header)]
    header: Vec<(String, String)>,
    /// Query parameter, e.g. --query page=2 (repeatable).
    #[arg(long, value_name = "KEY=VALUE", value_parser = crate::parse_kv)]
    query: Vec<(String, String)>,
    /// The request body, if any.
    #[command(flatten)]
    body: body::Body,
    /// Write the status code and response headers to stdout before the body.
    /// Repeated response headers show their last value.
    #[arg(long, short = 'i')]
    include: bool,
    /// Write the request and response summary to stderr (credentials redacted).
    #[arg(long, short = 'v')]
    verbose: bool,
    /// Exit 0 only for this status (default: any 2xx).
    #[arg(long, value_name = "STATUS")]
    expect: Option<u16>,
    /// Give up after this long, e.g. 500ms, 10s, 2m (includes a login, if any).
    #[arg(long, value_name = "DURATION", default_value = "30s", value_parser = parse_timeout)]
    timeout: Duration,
    /// Project config (default: apitest.nova in the current directory or a parent).
    #[arg(long, short = 'c')]
    config: Option<PathBuf>,
    /// Variables, redaction, User-Agent and tracing.
    #[command(flatten)]
    request: RequestFlags,
}

/// Parses `--timeout`; zero is refused because it could never succeed.
fn parse_timeout(s: &str) -> Result<Duration, String> {
    match load::parse_duration(s) {
        Ok(d) if d.is_zero() => Err("the timeout must be greater than zero".into()),
        Ok(d) => Ok(d),
        Err(e) => Err(e.to_string()),
    }
}

/// Parses `-H 'Name: value'`; the value is trimmed and may contain `:`.
fn parse_header(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once(':')
        .ok_or_else(|| format!("expected 'Name: value', got '{s}'"))?;
    let k = k.trim();
    if k.is_empty() {
        return Err(format!("empty header name in '{s}'"));
    }

    Ok((k.to_string(), v.trim().to_string()))
}

/// Runs `apitest call`. `Ok(false)` means the status was not the expected one.
///
/// # Errors
///
/// Fails on a missing or broken config, a refused request (see [`target`] and
/// [`actor`]), an unreadable body, a failed login, a connection error, a
/// timeout, an unresolved variable of the `--as` actor, or when stdout cannot
/// be written.
///
/// Everything written is masked with `redactor`: the response, the `-v`
/// summary and the messages, like the reports of `apitest run`.
pub async fn call(a: Args, redactor: &Redactor) -> Result<bool> {
    let (mut cfg, source) = run_config(&a, redactor)?;
    target::check_request(&a.method, &a.path)?;
    target::check_service(&cfg, &a.service, &source)?;
    if let Some(name) = &a.actor {
        actor::check(&cfg, name, &a.service, &a.header)?;
    }

    // Only the actor this call uses must resolve: an unrelated actor's missing
    // secret doesn't block a one-off request.
    cfg.actors.retain(|name, _| a.actor.as_ref() == Some(name));
    let opts = request_opts(&a)?;
    let resp = send(&cfg, &a, opts).await?;
    if a.verbose {
        let summary = output::summary(&resp, &cfg, a.actor.as_deref());
        eprint!("{}", redactor.redact(&summary));
    }

    output::write_response(&resp, a.include, redactor)?;

    Ok(status_ok(&resp, a.expect, redactor))
}

/// Loads the project config and layers the request flags over it, masking
/// with `redactor`. Also returns the config file's path for messages.
///
/// # Errors
///
/// Fails when no `apitest.nova` is found or given, it cannot be parsed, a
/// flag is invalid or a `.env` file is broken.
fn run_config(a: &Args, redactor: &Redactor) -> Result<(RunConfig, String)> {
    let project = match &a.config {
        Some(p) => ProjectConfig::load(p)?,
        None => ProjectConfig::discover(&std::env::current_dir()?)?.ok_or_else(|| {
            anyhow!(
                "no apitest.nova in this directory or its parents\n\
                 help: `apitest call` sends to services from `services:` in apitest.nova; \
                 pass --config <file>"
            )
        })?,
    };
    let source = a
        .config
        .clone()
        .unwrap_or_else(|| project.root.join("apitest.nova"));

    let base = RunConfig {
        redactor: redactor.clone(),
        ..RunConfig::default()
    };
    let mut cfg = project.apply(base)?;
    a.request.apply(&mut cfg, Some(&project))?;

    Ok((cfg, source.display().to_string()))
}

/// The request options: service, actor, `-H` headers, `--query` and the body.
///
/// # Errors
///
/// Fails when the body cannot be read or `--json` is not valid JSON.
fn request_opts(a: &Args) -> Result<RequestOpts> {
    let mut opts = RequestOpts {
        service: Some(a.service.clone()),
        as_actor: a.actor.clone(),
        ..Default::default()
    };
    for (k, v) in &a.header {
        opts.headers.insert(k.clone(), Value::String(v.clone()));
    }

    for (k, v) in &a.query {
        opts.query.insert(k.clone(), Value::String(v.clone()));
    }

    a.body.apply(&mut opts)?;

    Ok(opts)
}

/// Sends the request in a suite-less context, giving up after `--timeout`.
///
/// # Errors
///
/// Fails when the context cannot be built, the request (or the actor's login)
/// fails, or no response arrives in time.
async fn send(cfg: &RunConfig, a: &Args, opts: RequestOpts) -> Result<Response> {
    let cx = Ctx::standalone(cfg)?;
    let send = cx.request(&a.method, &a.path).opts(opts).send();

    tokio::time::timeout(a.timeout, send).await.map_err(|_| {
        anyhow!(
            "no response within {:?} (raise it with --timeout)",
            a.timeout
        )
    })?
}

/// Whether the status is the `--expect`ed one (default: any 2xx). Says on
/// stderr what was expected when it is not, and where an unfollowed redirect
/// pointed (masked with `redactor`).
fn status_ok(resp: &Response, expect: Option<u16>, redactor: &Redactor) -> bool {
    let ok = expect.map_or((200..300).contains(&resp.status), |s| resp.status == s);
    if !ok {
        let wanted = expect.map_or_else(|| "2xx".to_string(), |s| s.to_string());
        eprintln!("status {} (expected {wanted})", resp.status);
    }

    // Only redirects to another origin come back unfollowed (see `Ctx::standalone`).
    if let (301..=303 | 307 | 308, Some(location)) = (resp.status, resp.header("location")) {
        eprintln!(
            "redirect to {} not followed: it leaves the service's origin",
            redactor.redact(location)
        );
    }

    ok
}

#[cfg(test)]
mod tests {
    use super::{parse_header, parse_timeout};

    #[test]
    fn bad_timeouts_are_usage_errors_not_panics() {
        for t in ["-1s", "NaN", "infs", "1e300s", "0s"] {
            assert!(parse_timeout(t).is_err(), "{t}");
        }

        assert_eq!(parse_timeout("250ms").unwrap().as_millis(), 250);
    }

    #[test]
    fn parses_headers() {
        assert_eq!(
            parse_header("X-Tenant: 42").unwrap(),
            ("X-Tenant".into(), "42".into())
        );
        assert_eq!(
            parse_header("X-Url:http://a:b").unwrap(),
            ("X-Url".into(), "http://a:b".into())
        );
        assert!(parse_header("nope").is_err());
        assert!(parse_header(": v").is_err());
    }
}
