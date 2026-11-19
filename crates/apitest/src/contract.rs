//! Contract pinning against `OpenAPI` documents.
//!
//! * [`Contract::pinned`] loads a checked-in spec (YAML or JSON, `OpenAPI` 3.0/3.1).
//! * Every `cx.call(contract, operationId)` validates the request body and the
//!   response (status must be documented, body must match the schema).
//! * The operations, parameters, request fields and statuses a suite actually
//!   uses are recorded and can be written to `apitest.lock` ([`Lockfile`]).
//! * Before a suite runs, the service's *live* spec is diffed against the pinned
//!   one, restricted to the used surface ([`Contract::diff`]). Breaking findings
//!   fail the suite.

mod drift;
mod lockfile;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub use drift::{DriftReport, Finding, Severity};
pub use lockfile::{LockedContract, Lockfile};

use crate::http::RequestOpts;

/// HTTP methods an `OpenAPI` path item can define operations for, in the
/// order they are read.
const HTTP_METHODS: [&str; 7] = ["get", "put", "post", "delete", "patch", "head", "options"];

/// How deep [`resolve`] follows nested `$ref`s before giving up, which keeps
/// recursive schemas finite.
const MAX_REF_DEPTH: usize = 32;

/// How many schema violations [`Contract::check`] lists in one error.
const MAX_REPORTED_VIOLATIONS: usize = 5;

/// How strictly the live spec must match the pinned one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compat {
    /// Additive changes (new optional fields, new operations, new statuses) are fine.
    #[default]
    Tolerant,
    /// Any change to the used surface is breaking, including new response properties.
    Strict,
}

impl std::str::FromStr for Compat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "tolerant" => Ok(Compat::Tolerant),
            "strict" => Ok(Compat::Strict),
            o => bail!("unknown compat mode '{o}' (tolerant|strict)"),
        }
    }
}

/// A parsed `OpenAPI` document reduced to what matters for testing.
///
/// All local `$ref`s are inlined and `OpenAPI` 3.0 `nullable` is rewritten to
/// JSON Schema, so every schema in here can be fed to a validator as is.
#[derive(Debug, Clone)]
pub struct Spec {
    /// `info.title`, empty when missing.
    pub title: String,
    /// `info.version`, `?` when missing.
    pub version: String,
    /// Lowercase hex SHA-256 of the raw document bytes. Empty for documents
    /// built with [`Spec::from_value`] by callers that have no bytes.
    pub sha256: String,
    /// Operations keyed by [`Operation::id`].
    pub operations: BTreeMap<String, Operation>,
}

/// One operation (method + path) of a [`Spec`].
#[derive(Debug, Clone)]
pub struct Operation {
    /// The `operationId`, or `METHOD /path` when the spec gives none.
    pub id: String,
    /// Uppercase HTTP method, e.g. `POST`.
    pub method: String,
    /// Path template as written in the spec, e.g. `/invoices/{id}`.
    pub path: String,
    /// Path-item and operation parameters merged; an operation parameter
    /// replaces a path-item one with the same name and location.
    pub params: Vec<Param>,
    /// The request body, `None` when the operation documents none.
    pub body: Option<Body>,
    /// Status key (`"201"`, `"4XX"`, `"default"`) → JSON body schema (None = no JSON body).
    pub responses: BTreeMap<String, Option<Value>>,
}

/// One documented parameter of an [`Operation`].
#[derive(Debug, Clone)]
pub struct Param {
    /// Parameter name as sent on the wire.
    pub name: String,
    /// The `in` value: `path`, `query`, `header` or `cookie` (`query` when missing).
    pub location: String,
    /// Whether the parameter must be sent. Path parameters default to required.
    pub required: bool,
    /// JSON Schema of the value, `{}` when the spec gives none.
    pub schema: Value,
}

/// The request body of an [`Operation`].
#[derive(Debug, Clone)]
pub struct Body {
    /// Whether a body must be sent.
    pub required: bool,
    /// JSON Schema of the first JSON media type, `{}` when there is none.
    pub schema: Value,
}

impl Spec {
    /// Reads and parses a spec file. The extension (`.yaml`, `.yml`, `.json`)
    /// selects the parser; anything else tries JSON, then YAML.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or parsed (see [`Spec::from_bytes`]).
    pub fn from_file(path: &Path) -> Result<Spec> {
        let raw =
            std::fs::read(path).with_context(|| format!("reading spec {}", path.display()))?;
        Spec::from_bytes(&raw, path.extension().and_then(|e| e.to_str()))
            .with_context(|| format!("parsing spec {}", path.display()))
    }

    /// Parses a spec from raw bytes and records their SHA-256. `ext_hint` is a
    /// file extension without the dot (`yaml`, `yml`, `json`); with no or an
    /// unknown hint, JSON is tried first, then YAML.
    ///
    /// # Errors
    ///
    /// Fails when the bytes are neither valid JSON nor YAML, or when the
    /// document has no `paths` object.
    pub fn from_bytes(raw: &[u8], ext_hint: Option<&str>) -> Result<Spec> {
        let doc: Value = match ext_hint {
            Some("yaml" | "yml") => serde_saphyr::from_slice(raw)?,
            Some("json") => serde_json::from_slice(raw)?,
            _ => serde_json::from_slice(raw).or_else(|_| serde_saphyr::from_slice(raw))?,
        };
        let sha256 = Sha256::digest(raw)
            .iter()
            .fold(String::new(), |mut hex, b| {
                // Writing into a `String` cannot fail.
                let _ = write!(hex, "{b:02x}");
                hex
            });

        Spec::from_value(&doc, sha256)
    }

    /// Builds a spec from an already parsed document. `sha256` is stored as
    /// given in [`Spec::sha256`].
    ///
    /// # Errors
    ///
    /// Fails when the document has no `paths` object.
    pub fn from_value(doc: &Value, sha256: String) -> Result<Spec> {
        let resolved = resolve(doc, doc, 0);
        let info = resolved.get("info").cloned().unwrap_or(Value::Null);
        let paths = resolved
            .get("paths")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("spec has no `paths`"))?;

        let mut operations = BTreeMap::new();
        for (path, item) in paths {
            for op in path_operations(path, item) {
                operations.insert(op.id.clone(), op);
            }
        }

        Ok(Spec {
            title: str_field(&info, "title", ""),
            version: str_field(&info, "version", "?"),
            sha256,
            operations,
        })
    }
}

impl Operation {
    /// Finds the response key that documents `status` (exact, then `2XX`, then `default`).
    #[must_use]
    pub fn response_key(&self, status: u16) -> Option<&str> {
        let exact = status.to_string();
        let range = format!("{}XX", status / 100);
        [exact.as_str(), range.as_str(), "default"]
            .into_iter()
            .find_map(|k| self.responses.get_key_value(k).map(|(k, _)| k.as_str()))
            .or_else(|| {
                self.responses
                    .keys()
                    .find(|k| k.eq_ignore_ascii_case(&range))
                    .map(String::as_str)
            })
    }
}

/// All operations of one (already `$ref`-resolved) path item, in
/// [`HTTP_METHODS`] order.
fn path_operations(path: &str, item: &Value) -> Vec<Operation> {
    let shared_params = array_field(item, "parameters");
    HTTP_METHODS
        .iter()
        .filter_map(|method| {
            item.get(method)
                .map(|op| parse_operation(path, method, op, &shared_params))
        })
        .collect()
}

/// Builds one [`Operation`] from its `OpenAPI` object. `shared_params` are the
/// path item's parameters, which the operation's own parameters override.
fn parse_operation(path: &str, method: &str, op: &Value, shared_params: &[Value]) -> Operation {
    let id = op.get("operationId").and_then(Value::as_str).map_or_else(
        || format!("{} {path}", method.to_uppercase()),
        str::to_string,
    );
    let own_params = array_field(op, "parameters");
    let body = op.get("requestBody").map(|rb| Body {
        required: rb.get("required").and_then(Value::as_bool).unwrap_or(false),
        schema: json_schema_of(rb).unwrap_or(json!({})),
    });
    let responses = op
        .get("responses")
        .and_then(Value::as_object)
        .map(|r| {
            r.iter()
                .map(|(k, v)| (k.clone(), json_schema_of(v)))
                .collect()
        })
        .unwrap_or_default();

    Operation {
        id,
        method: method.to_uppercase(),
        path: path.to_string(),
        params: merge_params(shared_params.iter().chain(&own_params)),
        body,
        responses,
    }
}

/// Parses parameters in order; a later parameter replaces an earlier one with
/// the same name and location.
fn merge_params<'a>(raw: impl Iterator<Item = &'a Value>) -> Vec<Param> {
    let mut params: Vec<Param> = vec![];
    for p in raw {
        let param = parse_param(p);
        params.retain(|q| !(q.name == param.name && q.location == param.location));
        params.push(param);
    }

    params
}

/// Builds a [`Param`] from its `OpenAPI` object, filling in the defaults
/// documented on its fields.
fn parse_param(p: &Value) -> Param {
    let location = str_field(p, "in", "query");
    Param {
        name: str_field(p, "name", ""),
        required: p
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(location == "path"),
        schema: p.get("schema").cloned().unwrap_or(json!({})),
        location,
    }
}

/// The string at `v[key]`, or `default` when it is missing or not a string.
fn str_field(v: &Value, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

/// A copy of the array at `v[key]`, or an empty vector.
fn array_field(v: &Value, key: &str) -> Vec<Value> {
    v.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The schema of the first JSON media type (`application/json…` or `…+json`)
/// in a request body or response object's `content`.
fn json_schema_of(v: &Value) -> Option<Value> {
    let content = v.get("content")?.as_object()?;
    content
        .iter()
        .find(|(ct, _)| ct.starts_with("application/json") || ct.ends_with("+json"))
        .and_then(|(_, m)| m.get("schema").cloned())
}

/// Inlines local `$ref`s and rewrites `OpenAPI` 3.0 `nullable` into JSON Schema.
///
/// `root` is the whole document `$ref` pointers resolve against. Beyond
/// [`MAX_REF_DEPTH`] nested references (a recursive schema) the schema is
/// replaced by `{}`, which accepts anything. `example`/`examples` keys are
/// dropped, since they are payloads, not schema.
fn resolve(v: &Value, root: &Value, depth: usize) -> Value {
    if depth > MAX_REF_DEPTH {
        return json!({});
    }

    match v {
        Value::Object(o) => {
            if let Some(r) = o.get("$ref").and_then(Value::as_str)
                && let Some(ptr) = r.strip_prefix('#')
                && let Some(target) = root.pointer(ptr)
            {
                return resolve(target, root, depth + 1);
            }

            let mut out: Map<String, Value> = o
                .iter()
                .filter(|(k, _)| *k != "example" && *k != "examples")
                .map(|(k, val)| (k.clone(), resolve(val, root, depth)))
                .collect();
            rewrite_nullable(&mut out);
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve(x, root, depth)).collect()),
        other => other.clone(),
    }
}

/// Turns `nullable: true` with a single string `type` into `type: [t, "null"]`
/// and drops the `nullable` key.
fn rewrite_nullable(schema: &mut Map<String, Value>) {
    if schema.get("nullable") != Some(&Value::Bool(true)) {
        return;
    }

    schema.remove("nullable");
    if let Some(Value::String(t)) = schema.get("type").cloned() {
        schema.insert("type".into(), json!([t, "null"]));
    }
}

/// What a suite actually used from one operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpUsage {
    /// The [`Operation::id`].
    pub op: String,
    /// Names of the path and query parameters the suite sent.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub params: BTreeSet<String>,
    /// Top-level keys of the JSON object bodies the suite sent.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub request_fields: BTreeSet<String>,
    /// HTTP statuses the service answered with.
    #[serde(default)]
    pub statuses: BTreeSet<u16>,
}

/// Where the service's *current* spec comes from.
#[derive(Debug, Clone)]
pub enum LiveSpec {
    /// Absolute URL, or a path relative to the run's base URL (e.g. `/openapi.json`).
    Url(String),
    /// An in-process document, e.g. `serde_json::to_value(ApiDoc::openapi())` with utoipa.
    Document(Value),
}

/// A pinned contract bound to a suite.
///
/// Usage recording and the validator cache sit behind mutexes, so one
/// contract can be shared by concurrently running tests.
pub struct Contract {
    /// Name used in messages, as the lockfile key and as the default service name.
    pub name: String,
    /// Path of the pinned spec file; empty for [`Contract::from_spec`].
    pub spec_path: PathBuf,
    /// The pinned spec every request and response is validated against.
    pub pinned: Spec,
    /// Where to get the live spec for drift detection; `None` skips it.
    pub live_spec: Option<LiveSpec>,
    /// Service whose base URL this contract's operations are sent to (see `RunConfig::services`).
    pub service: Option<String>,
    /// How strictly drift is judged.
    pub mode: Compat,
    /// Recorded usage keyed by [`Operation::id`].
    usage: Mutex<BTreeMap<String, OpUsage>>,
    /// Compiled validators keyed by `<op>#request` or `<op>#<status key>`, so
    /// each schema compiles once.
    validators: Mutex<HashMap<String, Arc<jsonschema::Validator>>>,
}

impl std::fmt::Debug for Contract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Contract")
            .field("name", &self.name)
            .field("version", &self.pinned.version)
            .field("spec", &self.spec_path)
            .finish_non_exhaustive()
    }
}

impl Contract {
    /// Loads a pinned spec from disk.
    ///
    /// # Errors
    ///
    /// Fails when the spec cannot be read or parsed (see [`Spec::from_file`]).
    pub fn pinned(name: &str, spec: impl AsRef<Path>) -> Result<Contract> {
        let spec_path = spec.as_ref().to_path_buf();
        let pinned = Spec::from_file(&spec_path)?;
        Ok(Contract {
            spec_path,
            ..Contract::from_spec(name, pinned)
        })
    }

    /// Pins an already loaded spec. [`Contract::spec_path`] stays empty, so the
    /// contract cannot be recorded in a lockfile relative to its file.
    #[must_use]
    pub fn from_spec(name: &str, pinned: Spec) -> Contract {
        Contract {
            name: name.into(),
            spec_path: PathBuf::new(),
            pinned,
            live_spec: None,
            service: None,
            mode: Compat::Tolerant,
            usage: Mutex::default(),
            validators: Mutex::default(),
        }
    }

    /// Where to fetch the service's current spec from before the suite runs
    /// (absolute URL or a path relative to the base URL).
    #[must_use]
    pub fn live_spec(mut self, url: impl Into<String>) -> Self {
        self.live_spec = Some(LiveSpec::Url(url.into()));
        self
    }

    /// Uses an in-process `OpenAPI` document as the live spec. With utoipa:
    /// `.live_document(serde_json::to_value(ApiDoc::openapi())?)`.
    #[must_use]
    pub fn live_document(mut self, doc: Value) -> Self {
        self.live_spec = Some(LiveSpec::Document(doc));
        self
    }

    /// Sends this contract's operations to a named service (default: the service named
    /// like the contract, if configured, else the base URL).
    #[must_use]
    pub fn service(mut self, name: impl Into<String>) -> Self {
        self.service = Some(name.into());
        self
    }

    /// Sets how strictly drift is judged; see [`Compat`].
    #[must_use]
    pub fn mode(mut self, mode: Compat) -> Self {
        self.mode = mode;
        self
    }

    /// Looks up a pinned operation by id.
    ///
    /// # Errors
    ///
    /// Fails when the pinned spec has no operation `id`; the message names the
    /// contract, its version and spec file.
    pub fn operation(&self, id: &str) -> Result<&Operation> {
        self.pinned.operations.get(id).ok_or_else(|| {
            anyhow!(
                "operation '{id}' is not in pinned contract '{}' v{} ({})",
                self.name,
                self.pinned.version,
                self.spec_path.display()
            )
        })
    }

    /// A snapshot of the usage recorded so far, sorted by operation id.
    ///
    /// # Panics
    ///
    /// Panics when the usage lock is poisoned, i.e. a thread panicked while
    /// recording usage.
    #[must_use]
    pub fn usage(&self) -> Vec<OpUsage> {
        self.usage.lock().unwrap().values().cloned().collect()
    }

    /// Records the path and query parameters and top-level JSON fields of a
    /// request to operation `id`.
    pub(crate) fn record_request(&self, id: &str, opts: &RequestOpts) {
        let mut usage = self.usage.lock().unwrap();
        let e = usage_entry(&mut usage, id);
        e.params.extend(opts.path.keys().cloned());
        e.params.extend(opts.query.keys().cloned());
        if let Some(Value::Object(o)) = &opts.json {
            e.request_fields.extend(o.keys().cloned());
        }
    }

    /// Records that operation `id` answered with `status`.
    pub(crate) fn record_status(&self, id: &str, status: u16) {
        let mut usage = self.usage.lock().unwrap();
        usage_entry(&mut usage, id).statuses.insert(status);
    }

    /// The compiled validator for `schema`, cached under `key`.
    fn validator(&self, key: String, schema: &Value) -> Result<Arc<jsonschema::Validator>> {
        let mut cache = self.validators.lock().unwrap();
        if let Some(v) = cache.get(&key) {
            return Ok(v.clone());
        }

        let v = Arc::new(
            jsonschema::validator_for(schema)
                .map_err(|e| anyhow!("invalid schema in contract {key}: {e}"))?,
        );
        cache.insert(key, v.clone());
        Ok(v)
    }

    /// Validates `instance` against `schema` (cached under `key`). `what`
    /// describes the instance in the error, which lists up to
    /// [`MAX_REPORTED_VIOLATIONS`] violations with their JSON paths.
    fn check(&self, key: String, what: &str, schema: &Value, instance: &Value) -> Result<()> {
        let v = self.validator(key, schema)?;
        let errors: Vec<String> = v
            .iter_errors(instance)
            .take(MAX_REPORTED_VIOLATIONS)
            .map(|e| {
                let p = e.instance_path().to_string();
                format!(
                    "  - {}: {e}",
                    if p.is_empty() { "<root>".into() } else { p }
                )
            })
            .collect();
        if errors.is_empty() {
            return Ok(());
        }

        bail!(
            "contract violation: {what} does not match pinned '{}' v{}:\n{}",
            self.name,
            self.pinned.version,
            errors.join("\n")
        )
    }

    /// Validates a request body against operation `id`'s pinned request
    /// schema. Operations without a documented body accept anything.
    pub(crate) fn validate_request(&self, id: &str, body: &Value) -> Result<()> {
        let op = self.operation(id)?;
        match &op.body {
            Some(b) => self.check(
                format!("{id}#request"),
                &format!("request body of {id}"),
                &b.schema,
                body,
            ),
            None => Ok(()),
        }
    }

    /// Validates a response: `status` must be documented for operation `id`,
    /// and `body` must match its JSON schema if it has one.
    pub(crate) fn validate_response(&self, id: &str, status: u16, body: &Value) -> Result<()> {
        let op = self.operation(id)?;
        let Some(key) = op.response_key(status) else {
            let documented: Vec<&str> = op.responses.keys().map(String::as_str).collect();
            bail!(
                "contract violation: {id} returned status {status}, which pinned '{}' v{} does not document (documented: {})",
                self.name,
                self.pinned.version,
                documented.join(", ")
            );
        };

        match op.responses.get(key).cloned().flatten() {
            Some(schema) => self.check(
                format!("{id}#{key}"),
                &format!("response {status} of {id}"),
                &schema,
                body,
            ),
            None => Ok(()),
        }
    }
}

/// The usage entry of operation `id`, created empty on first use.
fn usage_entry<'a>(usage: &'a mut BTreeMap<String, OpUsage>, id: &str) -> &'a mut OpUsage {
    usage.entry(id.to_string()).or_insert_with(|| OpUsage {
        op: id.into(),
        ..Default::default()
    })
}
