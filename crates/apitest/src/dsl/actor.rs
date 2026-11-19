//! [`Actor`]: a service plus the credentials and headers used to call it.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::auth::{ActorConfig, Auth, ClientCredentials};
use crate::context::{Ctx, current};
use crate::http::Request;

/// Someone (or something) calling an API: a service plus credentials.
///
/// Actors are plain data (serializable), so fixtures can return them and tests
/// receive them as parameters. Requests made through an actor go to its service's
/// base URL ([`crate::RunConfig::service`]) and, for `call`, to the contract of the
/// same name (override with [`Actor::contract`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    /// Display name for logs ("hub admin", an e-mail address, ...).
    #[serde(default)]
    pub name: Option<String>,
    /// Service whose base URL raw requests go to, and the default contract for
    /// [`Actor::call`]. `None` means the actor can only reach absolute URLs.
    #[serde(default)]
    pub service: Option<String>,
    /// Contract for [`Actor::call`] when it is not named like [`Actor::service`].
    #[serde(default)]
    pub contract: Option<String>,
    /// Headers added to every request of this actor. Headers set on the request
    /// itself afterwards replace them.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Credentials for every request of this actor. [`Auth::None`] leaves the
    /// request to the scope's auth ([`Ctx::auth`]).
    #[serde(default)]
    pub auth: Auth,
}

impl Actor {
    /// An anonymous actor on a service.
    #[must_use]
    pub fn on(service: &str) -> Actor {
        Actor {
            service: Some(service.to_string()),
            ..Default::default()
        }
    }

    /// An actor with no service: only absolute URLs (see also [`super::visit`]).
    #[must_use]
    pub fn anywhere() -> Actor {
        Actor::default()
    }

    /// Sets the display name used in logs and step labels.
    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Actor {
        self.name = Some(name.into());
        self
    }

    /// Uses a different contract than the service name for `call`.
    #[must_use]
    pub fn contract(mut self, name: &str) -> Actor {
        self.contract = Some(name.to_string());
        self
    }

    /// `Authorization: Bearer <token>`.
    #[must_use]
    pub fn bearer(self, token: impl Into<String>) -> Actor {
        self.auth(Auth::bearer(token))
    }

    /// Alias of [`Actor::bearer`].
    #[must_use]
    pub fn with_bearer(self, token: impl AsRef<str>) -> Actor {
        self.bearer(token.as_ref())
    }

    /// HTTP Basic auth.
    #[must_use]
    pub fn basic(self, username: impl Into<String>, password: impl Into<String>) -> Actor {
        self.auth(Auth::basic(username, password))
    }

    /// An API key (or any credential) in a header.
    #[must_use]
    pub fn api_key(self, header: impl Into<String>, key: impl Into<String>) -> Actor {
        self.auth(Auth::header(header, key))
    }

    /// Logs in lazily through this service's [`crate::auth::LoginFlow`]: no request is
    /// made until the actor is used, the token is cached for the run, and a `401`
    /// triggers one re-login. The actor's display name becomes the username.
    #[must_use]
    pub fn login(self, username: impl Into<String>, password: impl Into<String>) -> Actor {
        let username = username.into();
        let name = self.name.clone().unwrap_or_else(|| username.clone());
        let service = self.service.clone();

        Actor {
            name: Some(name),
            ..self
        }
        .auth(Auth::Login {
            service,
            username,
            password: password.into(),
        })
    }

    /// OAuth 2.0 client credentials, cached until shortly before they expire.
    #[must_use]
    pub fn client_credentials(self, creds: ClientCredentials) -> Actor {
        self.auth(Auth::ClientCredentials(creds))
    }

    /// Replaces the actor's credentials.
    #[must_use]
    pub fn auth(mut self, auth: Auth) -> Actor {
        self.auth = auth;
        self
    }

    /// A named actor from the configuration (`actors:` in `apitest.nova`, `RunConfig::actor`).
    ///
    /// # Errors
    ///
    /// Fails outside a running fixture, hook, test or cleanup, when no actor of
    /// that name is configured, or when a `${VAR}` in one of its headers is not set.
    pub fn from_config(name: &str) -> Result<Actor> {
        Actor::from_config_in(&current()?, name)
    }

    /// [`Actor::from_config`] with an explicit context, for callers that already
    /// hold one (e.g. requests sent `as` a named actor).
    pub(crate) fn from_config_in(cx: &Ctx, name: &str) -> Result<Actor> {
        let c = configured(cx, name)?;

        // A login without an explicit service logs in on the actor's own service.
        let mut auth = c.auth.clone();
        if let Auth::Login {
            service: s @ None, ..
        } = &mut auth
        {
            s.clone_from(&c.service);
        }

        let mut a = Actor {
            name: Some(name.to_string()),
            service: c.service.clone(),
            auth,
            ..Default::default()
        };
        for (k, v) in &c.headers {
            // Configured header values usually carry keys or tokens.
            let value = crate::auth::interpolate(v, cx)?;
            cx.config().redactor.secret(&value);
            a = a.with_header(k.clone(), value);
        }

        if let Some(ua) = &c.user_agent {
            a = a.with_header("User-Agent", ua.clone());
        }

        Ok(a)
    }

    /// `User-Agent` for every request of this actor.
    #[must_use]
    pub fn user_agent(self, ua: impl Into<String>) -> Actor {
        self.with_header("User-Agent", ua)
    }

    /// Resolves dynamic credentials now (logs in / fetches a token) and returns an
    /// actor with the resulting static header, e.g. to report the login as a step.
    ///
    /// # Errors
    ///
    /// Fails outside a running fixture, hook, test or cleanup, when a `${VAR}` in
    /// the credentials is not set, or when the login / token request fails or its
    /// response carries no token.
    pub async fn signed_in(&self) -> Result<Actor> {
        let cx = current()?;
        let mut a = self.clone();
        if let Some((name, value)) = crate::auth::resolve(&cx, &self.auth, false).await? {
            a.auth = Auth::header(name, value);
        }

        Ok(a)
    }

    /// Adds (or replaces) a header sent with every request of this actor.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Actor {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// The bearer token, if this actor has a static one.
    #[must_use]
    pub fn token(&self) -> Option<&str> {
        match &self.auth {
            Auth::Bearer { token } => Some(token),
            Auth::Header { name, value } if name.eq_ignore_ascii_case("authorization") => {
                value.strip_prefix("Bearer ")
            }
            _ => self
                .headers
                .get("Authorization")
                .and_then(|h| h.strip_prefix("Bearer ")),
        }
    }

    /// Applies the actor's headers and (unless [`Auth::None`]) its credentials to `req`.
    fn decorate(&self, mut req: Request<'static>) -> Request<'static> {
        for (k, v) in &self.headers {
            req = req.header(k, v.clone());
        }

        if self.auth != Auth::None {
            req = req.auth(self.auth.clone());
        }

        req
    }

    /// A contract call by operationId.
    pub fn call(&self, operation: &str) -> Request<'static> {
        let contract = self.contract.clone().or_else(|| self.service.clone());
        self.decorate(Request::op(None, contract, operation))
    }

    /// A raw request to a path on this actor's service (or an absolute URL).
    pub fn request(&self, method: &str, path: &str) -> Request<'static> {
        let mut req = Request::raw(None, method, path);
        if let Some(s) = &self.service {
            req = req.service(s);
        }

        self.decorate(req)
    }

    /// A raw `GET` request; see [`Actor::request`].
    pub fn get(&self, path: &str) -> Request<'static> {
        self.request("GET", path)
    }

    /// A raw `POST` request; see [`Actor::request`].
    pub fn post(&self, path: &str) -> Request<'static> {
        self.request("POST", path)
    }

    /// A raw `PUT` request; see [`Actor::request`].
    pub fn put(&self, path: &str) -> Request<'static> {
        self.request("PUT", path)
    }

    /// A raw `PATCH` request; see [`Actor::request`].
    pub fn patch(&self, path: &str) -> Request<'static> {
        self.request("PATCH", path)
    }

    /// A raw `DELETE` request; see [`Actor::request`].
    pub fn delete(&self, path: &str) -> Request<'static> {
        self.request("DELETE", path)
    }
}

/// The configuration entry of actor `name`, or an error listing the known actors.
fn configured<'c>(cx: &'c Ctx, name: &str) -> Result<&'c ActorConfig> {
    let actors = &cx.config().actors;
    actors.get(name).ok_or_else(|| {
        let known: Vec<&str> = actors.keys().map(String::as_str).collect();
        let known = if known.is_empty() {
            "none".to_string()
        } else {
            known.join(", ")
        };

        anyhow!("no actor named '{name}' is configured (known: {known})")
    })
}
