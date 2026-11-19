//! Checks for `apitest call --as <actor>`.
//!
//! The actor must exist and belong to the service the request goes to. A `-H`
//! may not replace a header the actor sets, and an actor that logs in needs an
//! explicit login flow with a relative `path`: there is no spec to find a
//! `login` operation in, and no default endpoint is assumed.

use anyhow::{Result, anyhow, bail};

use apitest::auth::{ActorConfig, Auth, LoginFlow};
use apitest::runner::RunConfig;

/// Checks that actor `name` may send to `service` with the given `-H` headers.
/// `source` names the config file in messages.
///
/// # Errors
///
/// Fails when the actor is unknown, belongs to another service, conflicts with
/// a `-H`, or logs in without a usable login flow.
pub fn check(
    cfg: &RunConfig,
    name: &str,
    service: &str,
    headers: &[(String, String)],
    source: &str,
) -> Result<()> {
    let actor = find(cfg, name, source)?;
    check_service(actor, name, service)?;
    check_headers(cfg, actor, name, headers)?;
    if let Auth::Login { username, .. } = &actor.auth {
        let Some(login_service) = login_service(actor) else {
            bail!(
                "actor '{name}' logs in as '{username}', but names no service to log in on\n\
                 help: set `service: {service}` on the actor (or under its `auth:`) in {source}"
            );
        };
        check_login(cfg, name, username, login_service, source)?;
    }

    Ok(())
}

/// The service a login actor logs in on: its `auth.service`, else its own
/// `service`. This is the library's resolution when it sends, so a check that
/// passes here can't fail there; there is no fallback to the target service.
fn login_service(actor: &ActorConfig) -> Option<&str> {
    match &actor.auth {
        Auth::Login { service, .. } => service.as_deref().or(actor.service.as_deref()),
        _ => None,
    }
}

/// The headers an actor's credentials end up in: `Authorization`, an API-key
/// header, or the header its service's login flow names. Empty for an actor
/// without credentials, which leaves `Authorization` to the caller.
pub fn credential_headers(cfg: &RunConfig, actor: &ActorConfig) -> Vec<String> {
    if actor.auth == Auth::None {
        return vec![];
    }

    // An explicit `Authorization` drops the actor's credentials, so it always counts.
    let mut h = vec!["Authorization".to_string()];
    match &actor.auth {
        Auth::Header { name, .. } => h.push(name.clone()),
        Auth::Login { .. } => {
            if let Some(flow) = login_service(actor).and_then(|s| cfg.auth.get(s)) {
                h.push(flow.header.clone());
            }
        }
        _ => {}
    }

    h
}

/// Looks up actor `name` under `actors:` of `source`.
///
/// # Errors
///
/// Fails when there is no such actor, listing the configured ones.
fn find<'a>(cfg: &'a RunConfig, name: &str, source: &str) -> Result<&'a ActorConfig> {
    cfg.actors.get(name).ok_or_else(|| {
        let known: Vec<&str> = cfg.actors.keys().map(String::as_str).collect();
        anyhow!(
            "no actor named '{name}' is configured (known: {})\n\
             help: add it under `actors:` in {source}",
            if known.is_empty() {
                "none".into()
            } else {
                known.join(", ")
            }
        )
    })
}

/// Refuses an actor bound to a service other than `service`.
///
/// # Errors
///
/// Fails when the actor's `service` is set and differs.
fn check_service(actor: &ActorConfig, name: &str, service: &str) -> Result<()> {
    match &actor.service {
        Some(s) if s != service => bail!(
            "actor '{name}' belongs to service '{s}', not '{service}'\n\
             help: run `apitest call {s} ...`, or add an actor for '{service}'"
        ),
        _ => Ok(()),
    }
}

/// Refuses a `-H` for a header the actor sets (its credentials, `headers:` or
/// `user_agent:`): the two would conflict, and depending on the header one of
/// them would be dropped without a word (an `Authorization` replaces the
/// actor's credentials, while an API-key or login-flow header is overwritten
/// by them).
///
/// # Errors
///
/// Fails on the first conflicting `-H`.
fn check_headers(
    cfg: &RunConfig,
    actor: &ActorConfig,
    name: &str,
    headers: &[(String, String)],
) -> Result<()> {
    let mut taken = credential_headers(cfg, actor);
    taken.extend(actor.headers.keys().cloned());
    if actor.user_agent.is_some() {
        taken.push("User-Agent".into());
    }

    match headers
        .iter()
        .find(|(k, _)| taken.iter().any(|h| k.eq_ignore_ascii_case(h)))
    {
        Some((given, _)) => bail!(
            "-H '{given}: ...' conflicts with a header actor '{name}' sets\n\
             help: drop --as to send your own '{given}' header, or drop the -H"
        ),
        None => Ok(()),
    }
}

/// Checks that actor `name` (logging in as `username`) can log in on
/// `login_service`: a listed service with a usable URL and a login flow that
/// has a relative `path`. `source` names the config file in messages.
///
/// # Errors
///
/// Fails when the flow is missing, has no `path` or an absolute one, or the
/// login service is not listed under `services:` or has an unusable URL.
fn check_login(
    cfg: &RunConfig,
    name: &str,
    username: &str,
    login_service: &str,
    source: &str,
) -> Result<()> {
    let flow = cfg.auth.get(login_service).ok_or_else(|| {
        anyhow!(
            "actor '{name}' logs in as '{username}' on '{login_service}', but there is no \
             login flow for '{login_service}'\n\
             help: `apitest call` has no OpenAPI spec and doesn't guess a login endpoint; \
             configure it in {source}, e.g.\n  auth:\n    {login_service}:\n      \
             path: /login\n      body:\n        username: \"{{username}}\"\n        \
             password: \"{{password}}\"\n      token: /token"
        )
    })?;
    check_login_path(flow, login_service)?;
    let Some(url) = cfg.services.get(login_service) else {
        bail!(
            "actor '{name}' logs in on '{login_service}', which is not listed under `services:` \
             in {source}"
        );
    };

    super::target::check_service_url(login_service, url, source)
}

/// Requires a relative `path` on the login flow: an `operation` can't be
/// resolved without a spec, and an absolute URL would send the password elsewhere.
///
/// # Errors
///
/// Fails when the flow has no `path`, or one that does not start with `/`.
fn check_login_path(flow: &LoginFlow, login_service: &str) -> Result<()> {
    match &flow.path {
        None => bail!(
            "the login flow for '{login_service}' has no `path`\n\
             help: `apitest call` has no OpenAPI spec, so `operation: {}` can't be resolved; \
             set `path` (e.g. `path: /login`) under `auth.{login_service}`",
            flow.operation.as_deref().unwrap_or("login")
        ),
        Some(path) if !path.starts_with('/') => bail!(
            "the login flow for '{login_service}' has `path: {path}`, which must start with '/'\n\
             help: the login is sent to the '{login_service}' service; an absolute URL there \
             would send the actor's password to another host"
        ),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use apitest::auth::{ActorConfig, Auth, LoginFlow};
    use apitest::runner::RunConfig;

    use super::{check, credential_headers};

    /// One service `hub` and a `hub_admin` actor that logs in on it.
    fn cfg() -> RunConfig {
        RunConfig::default()
            .service("hub", "http://127.0.0.1:1")
            .actor(
                "hub_admin",
                "hub",
                Auth::login("hub", "${HUB_USER}", "${HUB_PASSWORD}"),
            )
    }

    /// `-H name: value` as parsed from the command line.
    fn h(name: &str, value: &str) -> [(String, String); 1] {
        [(name.to_string(), value.to_string())]
    }

    /// The error message of a check that must fail.
    fn err(cfg: &RunConfig, actor: &str, service: &str, headers: &[(String, String)]) -> String {
        check(cfg, actor, service, headers, "apitest.nova")
            .unwrap_err()
            .to_string()
    }

    /// Whether the check passes.
    fn passes(cfg: &RunConfig, actor: &str, service: &str, headers: &[(String, String)]) -> bool {
        check(cfg, actor, service, headers, "apitest.nova").is_ok()
    }

    #[test]
    fn a_login_actor_without_any_service_is_refused_not_guessed() {
        let mut c = cfg().login_flow("hub", LoginFlow::path("/login"));
        c.actors.insert(
            "admin".into(),
            ActorConfig {
                auth: Auth::Login {
                    service: None,
                    username: "u".into(),
                    password: "p".into(),
                },
                ..Default::default()
            },
        );
        let e = err(&c, "admin", "hub", &[]);
        assert!(e.contains("names no service to log in on"), "{e}");
        assert!(e.contains("set `service: hub`"), "{e}");
        assert_eq!(
            credential_headers(&c, &c.actors["admin"]),
            ["Authorization"]
        );
    }

    #[test]
    fn messages_name_the_config_file_in_use() {
        let e = check(&cfg(), "nobody", "hub", &[], "other.nova")
            .unwrap_err()
            .to_string();
        assert!(e.contains("under `actors:` in other.nova"), "{e}");
    }

    #[test]
    fn login_actor_needs_an_explicit_flow() {
        let e = err(&cfg(), "hub_admin", "hub", &[]);
        assert!(e.contains("no login flow for 'hub'"), "{e}");
        assert!(e.contains("doesn't guess a login endpoint"), "{e}");
    }

    #[test]
    fn login_flow_needs_a_path_without_a_spec() {
        let c = cfg().login_flow("hub", LoginFlow::operation("signIn"));
        let e = err(&c, "hub_admin", "hub", &[]);
        assert!(e.contains("has no `path`"), "{e}");
        assert!(e.contains("operation: signIn"), "{e}");

        let c = cfg().login_flow("hub", LoginFlow::path("/login"));
        assert!(passes(&c, "hub_admin", "hub", &[]));
    }

    #[test]
    fn login_flow_path_must_be_relative() {
        let c = cfg().login_flow("hub", LoginFlow::path("https://elsewhere.test/login"));
        let e = err(&c, "hub_admin", "hub", &[]);
        assert!(e.contains("must start with '/'"), "{e}");
    }

    #[test]
    fn actor_must_exist_and_match_the_service() {
        let e = err(&cfg(), "nobody", "hub", &[]);
        assert!(e.contains("known: hub_admin"), "{e}");

        let c = cfg()
            .service("app", "http://127.0.0.1:2")
            .login_flow("hub", LoginFlow::path("/login"));
        let e = err(&c, "hub_admin", "app", &[]);
        assert!(e.contains("belongs to service 'hub', not 'app'"), "{e}");
    }

    #[test]
    fn headers_the_actor_sets_cannot_be_overridden() {
        let mut c = cfg().login_flow("hub", LoginFlow::path("/login"));
        let e = err(&c, "hub_admin", "hub", &h("authorization", "Bearer x"));
        assert!(e.contains("conflicts with a header actor"), "{e}");

        c.actors.insert(
            "monitor".into(),
            ActorConfig {
                service: Some("hub".into()),
                auth: Auth::header("X-Api-Key", "k"),
                ..Default::default()
            },
        );
        assert!(!passes(&c, "monitor", "hub", &h("x-api-key", "other")));
        assert!(!passes(
            &c,
            "monitor",
            "hub",
            &h("Authorization", "Bearer x")
        ));
        assert!(passes(&c, "monitor", "hub", &h("X-Tenant", "42")));

        // The actor's `user_agent` counts as a header it sets.
        assert!(passes(&c, "monitor", "hub", &h("User-Agent", "mine")));
        c.actors.get_mut("monitor").unwrap().user_agent = Some("synthetic/1.0".into());
        assert!(!passes(&c, "monitor", "hub", &h("user-agent", "mine")));
    }

    #[test]
    fn login_flow_header_counts_as_a_credential() {
        let mut flow = LoginFlow::path("/login");
        flow.header = "X-Session".into();
        flow.scheme = None;
        let c = cfg().login_flow("hub", flow);
        assert!(!passes(&c, "hub_admin", "hub", &h("x-session", "mine")));
        assert!(credential_headers(&c, &c.actors["hub_admin"]).contains(&"X-Session".to_string()));
    }

    #[test]
    fn actors_without_credentials_allow_an_explicit_authorization() {
        let mut c = cfg();
        c.actors.insert(
            "tenant42".into(),
            ActorConfig {
                service: Some("hub".into()),
                ..Default::default()
            },
        );
        assert!(passes(
            &c,
            "tenant42",
            "hub",
            &h("Authorization", "Bearer x")
        ));

        // ... unless the actor sets it through `headers:`.
        c.actors
            .get_mut("tenant42")
            .unwrap()
            .headers
            .insert("Authorization".into(), "Bearer ${TOKEN}".into());
        let e = err(&c, "tenant42", "hub", &h("Authorization", "Bearer x"));
        assert!(
            e.contains("conflicts with a header actor 'tenant42'"),
            "{e}"
        );
    }
}
