//! Per-scenario state: actors, remembered values and the last response.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::StepArgs;
use crate::context::Ctx;
use crate::dsl::Actor;
use crate::http::{Response, scalar, truncate};

/// The state one scenario's steps share. A fresh `World` is created for every
/// scenario run; it is a cheap handle (clones share the state).
///
/// The lock is never held across an `.await`.
#[derive(Clone)]
pub struct World {
    /// The test's context.
    cx: Ctx,
    /// Actors, values and the latest response, shared by all clones.
    state: Arc<Mutex<State>>,
}

/// What the steps of one scenario have set up so far.
#[derive(Default)]
struct State {
    /// Actors declared by name.
    actors: BTreeMap<String, Actor>,
    /// Values remembered by name (`{{name}}`).
    values: BTreeMap<String, Value>,
    /// The response of the latest request step.
    response: Option<Arc<Response>>,
    /// The latest request step, for `within …,` checks that re-send it.
    last_request: Option<StepArgs>,
}

impl World {
    /// An empty world for one scenario run of `cx`'s test.
    #[must_use]
    pub fn new(cx: Ctx) -> Self {
        World {
            cx,
            state: Arc::default(),
        }
    }

    /// The test's context (variables, logging, cleanups, the redactor).
    #[must_use]
    pub fn cx(&self) -> &Ctx {
        &self.cx
    }

    /// The state, also after a panicking step: that test already failed, and
    /// its cleanups still need the actors.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Declares (or replaces) an actor of this scenario.
    pub fn set_actor(&self, name: &str, actor: Actor) {
        self.lock().actors.insert(name.to_string(), actor);
    }

    /// An actor declared earlier in this scenario (or its background). There is
    /// no implicit lookup in the configuration: configured actors are declared
    /// with `Given "x" is the configured actor "y"`.
    ///
    /// # Errors
    ///
    /// Fails when no actor of that name was declared; the error lists the
    /// declared ones.
    pub fn actor(&self, name: &str) -> Result<Actor> {
        let st = self.lock();
        st.actors.get(name).cloned().ok_or_else(|| {
            anyhow!(
                "no actor \"{name}\" in this scenario (declared: {}); declare it first, e.g. \
                 `Given \"{name}\" is an anonymous actor on \"<service>\"` or \
                 `Given \"{name}\" is the configured actor \"<name>\"`",
                known(st.actors.keys(), "none")
            )
        })
    }

    /// Remembers `value` as `name` for `{{name}}`.
    pub fn remember(&self, name: &str, value: Value) {
        self.lock().values.insert(name.to_string(), value);
    }

    /// A value remembered earlier in this scenario.
    ///
    /// # Errors
    ///
    /// Fails when nothing was remembered as `name`.
    pub fn recall(&self, name: &str) -> Result<Value> {
        let st = self.lock();
        st.values.get(name).cloned().ok_or_else(|| {
            anyhow!(
                "nothing remembered as \"{name}\" (remembered: {})",
                known(st.values.keys(), "nothing")
            )
        })
    }

    /// Makes `resp` the scenario's current response.
    pub fn set_response(&self, resp: Response) {
        self.lock().response = Some(Arc::new(resp));
    }

    /// Records the latest request step.
    pub(super) fn set_last_request(&self, args: StepArgs) {
        self.lock().last_request = Some(args);
    }

    /// The latest request step.
    ///
    /// # Errors
    ///
    /// Fails when no request step ran yet.
    pub(super) fn last_request(&self) -> Result<StepArgs> {
        self.lock().last_request.clone().ok_or_else(|| {
            anyhow!(
                "`within …,` on a check re-sends the scenario's latest request, but none was sent yet"
            )
        })
    }

    /// The response of the latest request a step sent in this scenario.
    ///
    /// # Errors
    ///
    /// Fails when no request was sent yet.
    pub fn response(&self) -> Result<Arc<Response>> {
        self.lock()
            .response
            .clone()
            .ok_or_else(|| anyhow!("no request has been sent in this scenario yet"))
    }

    /// The value at JSON pointer `ptr` of the current response's body.
    ///
    /// # Errors
    ///
    /// Fails when there is no response or the body has nothing at `ptr`; the
    /// error shows the start of the body.
    pub fn response_at(&self, ptr: &str) -> Result<Value> {
        let resp = self.response()?;
        resp.body
            .pointer(ptr)
            .cloned()
            .ok_or_else(|| anyhow!("the response has no '{ptr}': {}", truncate(&resp.text, 300)))
    }

    /// Resolves `${NAME}` (a run variable: `--var`, environment, `.env`,
    /// `vars:`) and `{{name}}` (a remembered value) / `{{response:/pointer}}`
    /// (the last response's body). Anything unresolved is an error; there are
    /// no defaults. Inserted values are not scanned again, and `$${` stands for
    /// a literal `${`.
    ///
    /// # Errors
    ///
    /// Fails on a malformed or unresolved `${..}` or `{{..}}`. The error gives
    /// a byte position, never the text: it may hold a secret.
    pub fn interpolate(&self, text: &str) -> Result<String> {
        self.substitute(
            text,
            |piece| crate::auth::interpolate(piece, &self.cx),
            |v| v,
        )
    }

    /// Only `{{..}}`. Auth steps use this and leave `${NAME}` to the actor's
    /// [`crate::auth::Auth`], which resolves it when the request is sent and
    /// masks the secret, the same as for actors from `apitest.nova`. A `${` in
    /// an inserted value is escaped, so a response can never name a variable.
    ///
    /// # Errors
    ///
    /// Fails on a malformed or unresolved `{{..}}`, with a byte position.
    pub fn interpolate_values(&self, text: &str) -> Result<String> {
        self.substitute(
            text,
            |piece| Ok(piece.to_string()),
            |v| v.replace("${", "$${"),
        )
    }

    /// Replaces every `{{key}}` in `text` with its value (through `value`) and
    /// passes the text between them through `literal`.
    fn substitute(
        &self,
        text: &str,
        literal: impl Fn(&str) -> Result<String>,
        value: impl Fn(String) -> String,
    ) -> Result<String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            let at = text.len() - rest.len() + start;
            out.push_str(&literal(&rest[..start])?);
            let tail = &rest[start + 2..];
            let end = tail
                .find("}}")
                .ok_or_else(|| anyhow!("unterminated {{{{ at byte {at}"))?;
            out.push_str(&value(scalar(&self.lookup(tail[..end].trim(), at)?)));
            rest = &tail[end + 2..];
        }

        out.push_str(&literal(rest)?);
        Ok(out)
    }

    /// The value of `{{key}}` found at byte `at`.
    fn lookup(&self, key: &str, at: usize) -> Result<Value> {
        let value = match key.strip_prefix("response:") {
            Some(ptr) => self.response_at(ptr),
            None => self.recall(key),
        };

        value.map_err(|e| e.context(format!("in {{{{..}}}} at byte {at}")))
    }
}

/// `keys` joined with `, `, or `empty` when there are none.
fn known<'a>(keys: impl Iterator<Item = &'a String>, empty: &str) -> String {
    let keys: Vec<&str> = keys.map(String::as_str).collect();
    if keys.is_empty() {
        empty.to_string()
    } else {
        keys.join(", ")
    }
}
