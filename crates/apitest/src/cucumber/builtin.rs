//! The built-in, generic step library ([`Steps::builtin`]).
//!
//! The steps are listed with examples in `docs/design/cucumber.md`. In short:
//!
//! | Kind | Step |
//! |---|---|
//! | actors | `"a" is an anonymous actor on "svc"` · `"a" is the configured actor "name"` · `"a" logs in on "svc" as "user" with password "pw"` · `"a" is an actor on "svc" with the bearer token "t"` · `"a" is an actor on "svc" with basic auth "user" and password "pw"` · `"a" sends the header "H" with "v"` |
//! | requests | `"a" calls "operationId"` · `"a" sends GET "/path"`, each optionally followed by ` expecting 201` and one of ` with:` (table), ` with the JSON body:` (docstring), ` with options:` (docstring) |
//! | cleanups | the same request steps prefixed with `afterwards`: registered as a cleanup of the scenario (LIFO), sent when it ends, pass or fail |
//! | values | `I remember the response at "/ptr" as "name"` · `I remember the response header "H" as "name"` · `a unique value with prefix "p" as "name"` · `a unique email with prefix "p" as "name"` |
//! | checks | `the status is 201` · `the response contains:` (docstring, partial JSON match) · `the response at "/ptr" is <JSON>` · `the response at "/ptr" has 2 items` · `the response header "H" is "v"` |

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde_json::{Map, Value};

use super::steps::{StepArgs, Steps};
use super::world::World;
use crate::auth::Auth;
use crate::dsl::{Actor, json_includes, unique, unique_email};
use crate::http::{Request, RequestOpts, scalar, truncate};

/// A quoted step argument: `"…"`, captured without the quotes.
const Q: &str = r#""([^"]*)""#;

/// Adds every built-in step to `s`.
///
/// # Errors
///
/// Fails only if a built-in pattern is not a valid regular expression.
pub(super) fn library(s: Steps) -> Result<Steps> {
    let s = actor_steps(s)?;
    let s = s.step(&request_pattern(), request_step)?.sends();
    let s = value_steps(s)?;
    check_steps(s)
}

/// Steps that declare actors and their headers.
fn actor_steps(s: Steps) -> Result<Steps> {
    s.step(
        &format!("{Q} is an anonymous actor on {Q}"),
        anonymous_actor,
    )?
    .step(
        &format!("{Q} is the configured actor {Q}"),
        configured_actor,
    )?
    .step(
        &format!("{Q} logs in on {Q} as {Q} with password {Q}"),
        login_actor,
    )?
    .step(
        &format!("{Q} is an actor on {Q} with the bearer token {Q}"),
        bearer_actor,
    )?
    .step(
        &format!("{Q} is an actor on {Q} with basic auth {Q} and password {Q}"),
        basic_actor,
    )?
    .step(&format!("{Q} sends the header {Q} with {Q}"), actor_header)
}

/// Steps that remember values for `{{name}}`.
fn value_steps(s: Steps) -> Result<Steps> {
    s.step(
        &format!("I remember the response at {Q} as {Q}"),
        remember_at,
    )?
    .step(
        &format!("I remember the response header {Q} as {Q}"),
        remember_header,
    )?
    .step(
        &format!("a unique (value|email) with prefix {Q} as {Q}"),
        remember_unique,
    )
}

/// Steps that check the current response.
fn check_steps(s: Steps) -> Result<Steps> {
    s.step(r"the (?:response )?status is (\d{3})", check_status)?
        .step("the response contains:", check_contains)?
        .step(&format!("the response at {Q} is (.+)"), check_at)?
        .step(
            &format!(r"the response at {Q} has (\d+) (?:items|elements|entries)"),
            check_count,
        )?
        .step(&format!("the response header {Q} is {Q}"), check_header)
}

/// `[afterwards ]"a" (calls "op" | sends METHOD "/path")[ expecting NNN][ with…:]`.
///
/// Captures: 1 `afterwards`, 2 actor, 3 operationId, 4 method, 5 path,
/// 6 expected status, 7 the `with` form.
fn request_pattern() -> String {
    format!(
        r"(afterwards )?{Q} (?:calls {Q}|sends (GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS) {Q})(?: expecting (\d{{3}}))?( with:| with the JSON body:| with options:)?"
    )
}

/// `"a" is an anonymous actor on "svc"`.
async fn anonymous_actor(w: World, a: StepArgs) -> Result<()> {
    w.set_actor(a.get(1)?, Actor::on(&w.interpolate(a.get(2)?)?));
    Ok(())
}

/// `"a" is the configured actor "name"`.
async fn configured_actor(w: World, a: StepArgs) -> Result<()> {
    let actor = Actor::from_config_in(w.cx(), &w.interpolate(a.get(2)?)?)?;
    w.set_actor(a.get(1)?, actor);
    Ok(())
}

/// `"a" logs in on "svc" as "user" with password "pw"`. Credentials keep their
/// `${NAME}` templates: the actor's `Auth` resolves and masks them when the
/// first request is sent, as for actors from `apitest.nova`.
async fn login_actor(w: World, a: StepArgs) -> Result<()> {
    let actor = Actor::on(&w.interpolate(a.get(2)?)?).login(
        w.interpolate_values(a.get(3)?)?,
        w.interpolate_values(a.get(4)?)?,
    );
    w.set_actor(a.get(1)?, actor);
    Ok(())
}

/// `"a" is an actor on "svc" with the bearer token "t"`. The token is masked
/// from here on, whether it is written in the step, remembered, taken from a
/// response or a `${NAME}`.
async fn bearer_actor(w: World, a: StepArgs) -> Result<()> {
    let token = w.interpolate_values(a.get(3)?)?;
    // An unresolved `${NAME}` is reported when the actor first sends.
    if let Ok(resolved) = crate::auth::interpolate(&token, w.cx()) {
        w.cx().config().redactor.secret(&resolved);
    }
    let actor = Actor::on(&w.interpolate(a.get(2)?)?).bearer(token);
    w.set_actor(a.get(1)?, actor);
    Ok(())
}

/// `"a" is an actor on "svc" with basic auth "user" and password "pw"`.
async fn basic_actor(w: World, a: StepArgs) -> Result<()> {
    let actor = Actor::on(&w.interpolate(a.get(2)?)?).basic(
        w.interpolate_values(a.get(3)?)?,
        w.interpolate_values(a.get(4)?)?,
    );
    w.set_actor(a.get(1)?, actor);
    Ok(())
}

/// `"a" sends the header "H" with "v"`, masked as [`mask_header`] decides.
async fn actor_header(w: World, a: StepArgs) -> Result<()> {
    let (name, raw) = (a.get(2)?, a.get(3)?);
    let value = w.interpolate(raw)?;
    mask_header(&w, name, raw, &value);
    let actor = w.actor(a.get(1)?)?.with_header(name, value);
    w.set_actor(a.get(1)?, actor);
    Ok(())
}

/// Registers a header value with the run's redactor when it is a credential:
/// the header's name is a credential's (`Authorization`, `X-Api-Key`), or the
/// value came from a variable, a remembered value or a response rather than
/// being written out (`raw` is the text as written). A literal
/// `application/json` stays readable.
fn mask_header(w: &World, name: &str, raw: &str, value: &str) {
    if value != raw || crate::redact::is_secret_name(name) {
        w.cx().config().redactor.header_value(value);
    }
}

/// A request step: sends the request, or with `afterwards` registers it as a
/// cleanup of the scenario.
async fn request_step(w: World, a: StepArgs) -> Result<()> {
    let req = build_request(&w, &a)?;
    if a.opt(1).is_some() {
        w.cx().undo(req);
        return Ok(());
    }

    w.set_last_request(a.clone());
    w.set_response(req.await?);
    Ok(())
}

/// `I remember the response at "/ptr" as "name"`.
async fn remember_at(w: World, a: StepArgs) -> Result<()> {
    let v = w.response_at(a.get(1)?)?;
    w.remember(a.get(2)?, v);
    Ok(())
}

/// `I remember the response header "H" as "name"`.
async fn remember_header(w: World, a: StepArgs) -> Result<()> {
    let resp = w.response()?;
    let name = a.get(1)?;
    let v = resp
        .header(name)
        .ok_or_else(|| anyhow!("the response has no header '{name}'"))?;
    w.remember(a.get(2)?, Value::String(v.to_string()));
    Ok(())
}

/// `a unique value|email with prefix "p" as "name"`.
async fn remember_unique(w: World, a: StepArgs) -> Result<()> {
    let prefix = w.interpolate(a.get(2)?)?;
    let v = if a.get(1)? == "email" {
        unique_email(&prefix)
    } else {
        unique(&prefix)
    };
    w.remember(a.get(3)?, Value::String(v));
    Ok(())
}

/// `the [response ]status is NNN`.
async fn check_status(w: World, a: StepArgs) -> Result<()> {
    let want: u16 = a.get(1)?.parse()?;
    let resp = w.response()?;
    ensure!(
        resp.status == want,
        "expected status {want}, got {}: {}",
        resp.status,
        truncate(&resp.text, 300)
    );
    Ok(())
}

/// `the response contains:` with a JSON docstring, matched partially.
async fn check_contains(w: World, a: StepArgs) -> Result<()> {
    let expected = w.interpolate_json(a.docstring()?)?;
    let resp = w.response()?;
    json_includes(&resp.body, &expected)
        .with_context(|| format!("response: {}", truncate(&resp.text, 300)))
}

/// `the response at "/ptr" is <JSON>`.
async fn check_at(w: World, a: StepArgs) -> Result<()> {
    let ptr = a.get(1)?;
    let expected = w.interpolate_json(a.get(2)?)?;
    let actual = w.response_at(ptr)?;
    ensure!(
        same(&actual, &expected),
        "at {ptr}: expected {expected}, got {actual}"
    );
    Ok(())
}

/// `the response at "/ptr" has N items`: the length of an array or object.
async fn check_count(w: World, a: StepArgs) -> Result<()> {
    let ptr = a.get(1)?;
    let want: usize = a.get(2)?.parse()?;
    let len = match w.response_at(ptr)? {
        Value::Array(v) => v.len(),
        Value::Object(m) => m.len(),
        other => bail!("at {ptr}: expected an array or object, got {other}"),
    };
    ensure!(len == want, "at {ptr}: expected {want} items, got {len}");
    Ok(())
}

/// `the response header "H" is "v"`.
async fn check_header(w: World, a: StepArgs) -> Result<()> {
    let name = a.get(1)?;
    let want = w.interpolate(a.get(2)?)?;
    let resp = w.response()?;
    let got = resp
        .header(name)
        .ok_or_else(|| anyhow!("the response has no header '{name}'"))?;
    ensure!(
        got == want,
        "header {name}: expected \"{want}\", got \"{got}\""
    );
    Ok(())
}

/// Sends the scenario's latest request again (built anew, so `{{..}}` and
/// `${..}` are resolved again) and makes its response the current one.
///
/// # Errors
///
/// Fails when no request was sent yet, or like the request step itself.
pub(super) async fn resend(w: &World) -> Result<()> {
    let a = w.last_request()?;
    let resp = build_request(w, &a)?.await?;
    w.set_response(resp);
    Ok(())
}

/// Builds the request of a request step (captures as in [`request_pattern`]).
fn build_request(w: &World, a: &StepArgs) -> Result<Request<'static>> {
    let actor = w.actor(a.get(2)?)?;
    let opts = request_opts(w, a)?;
    let mut req = match (a.opt(3), a.opt(4), a.opt(5)) {
        (Some(op), _, _) => actor.call(&w.interpolate(op)?).opts(opts),
        (None, Some(method), Some(path)) => raw_request(w, &actor, method, path, opts)?,
        _ => bail!("request step without an operation or a method and path"),
    };

    if let Some(status) = a.opt(6) {
        req = req.expect(status.parse()?);
    }

    Ok(req)
}

/// A raw request of `actor` to `path` as written in the step.
///
/// Values inserted into the path are percent-encoded segments, and none may
/// stand in the scheme or host of an absolute URL, so only the step text
/// decides where the request goes: a path on the actor's service, or an
/// absolute URL written out in the step. The actor's credentials and headers
/// go only to its own service's origin; a request to any other origin is sent
/// without them. Headers set on the scope (by a fixture or hook) are sent
/// either way, and the HTTP client follows redirects to other origins.
///
/// # Errors
///
/// Fails when the path is neither `/…` nor an `http(s)://` URL whose origin is
/// written out in the step, or cannot be interpolated.
fn raw_request(
    w: &World,
    actor: &Actor,
    method: &str,
    path: &str,
    mut opts: RequestOpts,
) -> Result<Request<'static>> {
    let absolute = path.starts_with("http://") || path.starts_with("https://");
    ensure!(
        absolute || path.starts_with('/'),
        "the path of a raw request must start with `/` (or be an absolute http(s):// URL written in the step)"
    );
    if absolute {
        let rest = &path[path.find("://").map_or(0, |i| i + 3)..];
        let origin = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
        ensure!(
            !origin.contains("{{"),
            "the host of an absolute URL must be written out in the step; a {{{{..}}}} value may only stand in its path or query"
        );
    }

    let url = w.interpolate_path(path)?;
    if absolute && !same_origin(w, actor, &url) {
        return Ok(Actor::anywhere()
            .request(method, &url)
            .opts(opts)
            .auth(Auth::None));
    }

    // `.opts(..)` replaces the options, including the actor's service.
    opts.service.clone_from(&actor.service);

    Ok(actor.request(method, &url).opts(opts))
}

/// Whether `url` has the origin (scheme, host, port) of `actor`'s service.
fn same_origin(w: &World, actor: &Actor, url: &str) -> bool {
    let origin = |u: &str| reqwest::Url::parse(u).ok().map(|u| u.origin());
    let service = w.cx().config().service_url(actor.service.as_deref()).ok();
    match (origin(url), service.and_then(origin)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The request options a request step's ` with…:` suffix and its table or
/// docstring describe.
fn request_opts(w: &World, a: &StepArgs) -> Result<RequestOpts> {
    let mut opts = RequestOpts::default();
    match a.opt(7).map(str::trim) {
        None => ensure!(
            a.docstring.is_none() && a.table.is_none(),
            "this step has a docstring or table; end it with ` with:` (table), \
             ` with the JSON body:` or ` with options:` (docstring)"
        ),
        Some("with:") => fill_from_table(w, a.table()?, &mut opts)?,
        Some("with the JSON body:") => {
            opts.json = Some(w.interpolate_json(a.docstring()?)?);
        }
        Some("with options:") => {
            opts = serde_json::from_value(w.interpolate_json(a.docstring()?)?).context(
                "invalid request options (same shape as in Rhai/JS: path, query, headers, json, form, body, …)",
            )?;
            // The actor alone decides who sends and where to, so neither the
            // feature nor a value from a response can move its credentials.
            ensure!(
                opts.service.is_none()
                    && opts.as_actor.is_none()
                    && opts.auth.is_none()
                    && opts.bearer.is_none(),
                "`service`, `as`, `auth` and `bearer` can't be set in step options: the actor decides \
                 who sends the request and to which service; declare another actor instead"
            );
            let raw: Value = serde_json::from_str(a.docstring()?.trim()).unwrap_or(Value::Null);
            for (name, value) in &opts.headers {
                let written = raw
                    .get("headers")
                    .and_then(|h| h.get(name))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                mask_header(w, name, written, &scalar(value));
            }
        }
        Some(other) => bail!("unexpected '{other}'"),
    }

    Ok(opts)
}

/// `| in | name | value |` rows; `in` is path, query, header, form or json.
/// A first row reading `in | name | value` is a header and skipped.
fn fill_from_table(w: &World, rows: &[Vec<String>], opts: &mut RequestOpts) -> Result<()> {
    let mut form = Map::new();
    let mut json = Map::new();
    for (i, row) in rows.iter().enumerate() {
        let [place, name, value] = row.as_slice() else {
            bail!(
                "table row {} has {} cells; expected | in | name | value |",
                i + 1,
                row.len()
            );
        };
        if i == 0 && place == "in" && name == "name" && value == "value" {
            continue;
        }

        let target = match place.as_str() {
            "path" => &mut opts.path,
            "query" => &mut opts.query,
            "header" => &mut opts.headers,
            "form" => &mut form,
            "json" => {
                let v = w
                    .interpolate_json(value)
                    .with_context(|| format!("json field '{name}'"))?;
                json.insert(name.clone(), v);
                continue;
            }
            other => bail!(
                "table row {}: unknown location '{other}' (path, query, header, form, json)",
                i + 1
            ),
        };
        let resolved = w.interpolate(value)?;
        if place == "header" {
            mask_header(w, name, value, &resolved);
        }
        target.insert(name.clone(), Value::String(resolved));
    }

    opts.form = (!form.is_empty()).then_some(form);
    if !json.is_empty() {
        opts.json = Some(Value::Object(json));
    }

    Ok(())
}

/// JSON equality where `4200` and `4200.0` are the same number, at any depth.
/// Integers compare exactly; only a float on either side compares as `f64`.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i128(), y.as_i128()) {
            (Some(x), Some(y)) => x == y,
            _ => x.as_f64() == y.as_f64(),
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_steps_parse() {
        let s = Steps::builtin();
        let (_, c) = s
            .find(r#"afterwards "user" calls "deleteInvoice" expecting 204 with:"#)
            .unwrap();
        assert_eq!(c[0].as_deref(), Some("afterwards "));
        assert_eq!(c[1].as_deref(), Some("user"));
        assert_eq!(c[2].as_deref(), Some("deleteInvoice"));
        assert_eq!(c[5].as_deref(), Some("204"));
        assert_eq!(c[6].as_deref(), Some(" with:"));
        let (_, c) = s.find(r#""anon" sends GET "/openapi.json""#).unwrap();
        assert_eq!(c[3].as_deref(), Some("GET"));
        assert_eq!(c[4].as_deref(), Some("/openapi.json"));
        assert!(s.find(r#""anon" sends FETCH "/x""#).is_err());
    }

    #[test]
    fn builtin_steps_are_unambiguous_for_their_examples() {
        let s = Steps::builtin();
        for text in [
            r#""a" is an anonymous actor on "billing""#,
            r#""a" is the configured actor "hub_admin""#,
            r#""a" logs in on "hub" as "${USER}" with password "${PW}""#,
            r#""a" is an actor on "billing" with the bearer token "{{token}}""#,
            r#""a" is an actor on "billing" with basic auth "u" and password "p""#,
            r#""a" sends the header "X-Tenant" with "t1""#,
            r#""a" calls "createInvoice" with the JSON body:"#,
            r#""a" calls "createInvoice" with options:"#,
            r#""a" sends DELETE "/messages""#,
            r#"I remember the response at "/id" as "id""#,
            r#"I remember the response header "Location" as "loc""#,
            r#"a unique email with prefix "patient" as "email""#,
            "the status is 201",
            "the response status is 404",
            "the response contains:",
            r#"the response at "/currency" is "EUR""#,
            r#"the response at "/items" has 2 items"#,
            r#"the response header "Content-Type" is "application/json""#,
        ] {
            s.find(text).unwrap_or_else(|e| panic!("{e}"));
        }
    }

    #[test]
    fn numbers_compare_by_value() {
        assert!(same(&serde_json::json!(4200), &serde_json::json!(4200.0)));
        assert!(!same(&serde_json::json!("4200"), &serde_json::json!(4200)));
        assert!(same(
            &serde_json::json!({ "n": [4200] }),
            &serde_json::json!({ "n": [4200.0] })
        ));
        assert!(!same(
            &serde_json::json!(9_007_199_254_740_993_u64),
            &serde_json::json!(9_007_199_254_740_992_u64)
        ));
    }
}
