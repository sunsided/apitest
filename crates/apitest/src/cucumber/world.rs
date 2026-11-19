//! Per-scenario state: actors, remembered values and the last response.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::StepArgs;
use crate::context::Ctx;
use crate::dsl::Actor;
use crate::http::{Response, encode_segment, scalar, truncate};

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
        let parts = self.parts(text)?;
        let mut out = String::with_capacity(text.len());
        for (i, part) in parts.iter().enumerate() {
            let brace_follows = parts[i + 1..]
                .iter()
                .map(Part::text)
                .find(|t| !t.is_empty())
                .is_some_and(|t| t.starts_with('{'));
            let piece = match part {
                Part::Literal(s) => (*s).to_string(),
                Part::Value(v) => v.replace("${", "$${"),
            };
            out.push_str(&piece);

            // A `$` that meets a `{` across a boundary would form a new `${`:
            // `$${` makes it a literal one.
            if brace_follows && piece.ends_with('$') {
                out.push('$');
            }
        }

        Ok(out)
    }

    /// A JSON template (a docstring, a `json` table cell, the expected value of
    /// a check) as a value. The template is parsed as JSON first, then `${..}`
    /// and `{{..}}` are resolved inside each string, so an inserted value can
    /// never change the document's structure. A string that is exactly one
    /// `{{name}}`, and a template that is nothing else, take the value with its
    /// JSON type (`"{{id}}"` is a number when the remembered value is one).
    ///
    /// # Errors
    ///
    /// Fails when the template is not valid JSON (the error gives the position,
    /// not the text), or like [`World::interpolate`].
    pub fn interpolate_json(&self, template: &str) -> Result<Value> {
        let template = template.trim();
        if let Some(v) = self.whole_placeholder(template)? {
            return Ok(v);
        }

        let doc = serde_json::from_str(template).map_err(|e| {
            anyhow!("not valid JSON ({e}); write strings in double quotes, e.g. \"EUR\"")
        })?;
        self.interpolate_strings(doc)
    }

    /// `{{..}}` and `${..}` in a raw request path. Every inserted value is
    /// percent-encoded as one path segment (`.` and `..` included), so a value
    /// cannot add segments, a query or another host to the path.
    ///
    /// # Errors
    ///
    /// Fails like [`World::interpolate`].
    pub fn interpolate_path(&self, path: &str) -> Result<String> {
        self.substitute(
            path,
            |piece| crate::auth::interpolate_with(piece, |name| Ok(segment(&self.cx.env(name)?))),
            |v| segment(&v),
        )
    }

    /// `v` with `${..}` and `{{..}}` resolved inside every string.
    fn interpolate_strings(&self, v: Value) -> Result<Value> {
        Ok(match v {
            Value::String(s) => match self.whole_placeholder(&s)? {
                Some(v) => v,
                None => Value::String(self.interpolate(&s)?),
            },
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|v| self.interpolate_strings(v))
                    .collect::<Result<_>>()?,
            ),
            Value::Object(fields) => Value::Object(
                fields
                    .into_iter()
                    .map(|(k, v)| Ok((k, self.interpolate_strings(v)?)))
                    .collect::<Result<_>>()?,
            ),
            other => other,
        })
    }

    /// The value of `text` when it is exactly one `{{key}}`.
    fn whole_placeholder(&self, text: &str) -> Result<Option<Value>> {
        let key = text
            .strip_prefix("{{")
            .and_then(|t| t.strip_suffix("}}"))
            .filter(|k| !k.contains("{{") && !k.contains("}}"));
        key.map(|k| self.lookup(k.trim(), 0)).transpose()
    }

    /// Replaces every `{{key}}` in `text` with its value (through `value`) and
    /// passes the text between them through `literal`.
    fn substitute(
        &self,
        text: &str,
        literal: impl Fn(&str) -> Result<String>,
        value: impl Fn(String) -> String,
    ) -> Result<String> {
        self.parts(text)?
            .into_iter()
            .map(|p| match p {
                Part::Literal(s) => literal(s),
                Part::Value(v) => Ok(value(v)),
            })
            .collect()
    }

    /// `text` split into literal pieces and the values of its `{{key}}`s.
    fn parts<'t>(&self, text: &'t str) -> Result<Vec<Part<'t>>> {
        let mut parts = vec![];
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            let at = text.len() - rest.len() + start;
            parts.push(Part::Literal(&rest[..start]));
            let tail = &rest[start + 2..];
            let end = tail
                .find("}}")
                .ok_or_else(|| anyhow!("unterminated {{{{ at byte {at}"))?;
            parts.push(Part::Value(scalar(&self.lookup(tail[..end].trim(), at)?)));
            rest = &tail[end + 2..];
        }

        parts.push(Part::Literal(rest));
        Ok(parts)
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

/// A piece of a template: text as written, or an inserted value.
enum Part<'t> {
    /// Text between `{{..}}`s, as written in the step.
    Literal(&'t str),
    /// The value of a `{{..}}`, as text.
    Value(String),
}

impl Part<'_> {
    /// The piece's text.
    fn text(&self) -> &str {
        match self {
            Part::Literal(s) => s,
            Part::Value(v) => v,
        }
    }
}

/// `value` percent-encoded as a single path segment; `.` and `..` are encoded
/// too, so they cannot climb out of the path they are inserted into.
fn segment(value: &str) -> String {
    match value {
        "." => "%2E".into(),
        ".." => "%2E%2E".into(),
        other => encode_segment(other),
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::RunConfig;

    /// A world on a standalone context with `SECRET` set and `v` and `n` remembered.
    fn world(v: &str) -> World {
        let cfg = RunConfig::new("http://127.0.0.1:1").var("SECRET", "hunter2-secret");
        let w = World::new(Ctx::standalone(&cfg).unwrap());
        w.remember("v", Value::String(v.into()));
        w.remember("n", json!(42));
        w
    }

    #[tokio::test]
    async fn json_templates_keep_their_structure() {
        let w = world(r#"", "x": 1, "p": "${SECRET}"#);
        let doc = w
            .interpolate_json(r#"{ "name": "{{v}}", "id": "{{n}}", "s": "${SECRET}" }"#)
            .unwrap();
        assert_eq!(
            doc,
            json!({ "name": r#"", "x": 1, "p": "${SECRET}"#, "id": 42, "s": "hunter2-secret" })
        );
        assert_eq!(w.interpolate_json("{{n}}").unwrap(), json!(42));
        assert_eq!(w.interpolate_json(r#""id-{{n}}""#).unwrap(), json!("id-42"));

        let e = w
            .interpolate_json("hunter2-secret")
            .unwrap_err()
            .to_string();
        assert!(!e.contains("hunter2"), "{e}");
    }

    #[tokio::test]
    async fn path_values_are_single_segments() {
        let w = world("http://other.test/steal?x=1");
        assert_eq!(
            w.interpolate_path("/go/{{v}}").unwrap(),
            "/go/http%3A%2F%2Fother.test%2Fsteal%3Fx%3D1"
        );
        w.remember("v", Value::String("..".into()));
        assert_eq!(w.interpolate_path("/a/{{v}}/b").unwrap(), "/a/%2E%2E/b");
        assert_eq!(
            w.interpolate_path("/s/${SECRET}").unwrap(),
            "/s/hunter2-secret"
        );
    }

    #[tokio::test]
    async fn inserted_values_never_form_a_variable() {
        let w = world("a$");
        assert_eq!(
            w.interpolate_values("{{v}}{SECRET}").unwrap(),
            "a$${SECRET}"
        );
        w.remember("w", Value::String("{SECRET}".into()));
        assert_eq!(w.interpolate_values("x${{w}}").unwrap(), "x$${SECRET}");
        assert_eq!(w.interpolate_values("{{v}}{{w}}").unwrap(), "a$${SECRET}");
        w.remember("v", Value::String("${SECRET}".into()));
        assert_eq!(
            w.interpolate_values("{{v}} ${KEEP}").unwrap(),
            "$${SECRET} ${KEEP}"
        );

        let resolved = crate::auth::interpolate("a$${SECRET}", w.cx()).unwrap();
        assert_eq!(resolved, "a${SECRET}");
    }
}
