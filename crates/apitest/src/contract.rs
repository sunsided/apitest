//! Contract pinning against OpenAPI documents.
//!
//! * [`Contract::pinned`] loads a checked-in spec (YAML or JSON, OpenAPI 3.0/3.1).
//! * Every `cx.call(contract, operationId)` validates the request body and the
//!   response (status must be documented, body must match the schema).
//! * The operations, parameters, request fields and statuses a suite actually
//!   uses are recorded and can be written to `apitest.lock`.
//! * Before a suite runs, the service's *live* spec is diffed against the pinned
//!   one, restricted to the used surface. Breaking findings fail the suite.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::http::RequestOpts;

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

/// A parsed OpenAPI document reduced to what matters for testing.
#[derive(Debug, Clone)]
pub struct Spec {
    pub title: String,
    pub version: String,
    pub sha256: String,
    pub operations: BTreeMap<String, Operation>,
}

#[derive(Debug, Clone)]
pub struct Operation {
    pub id: String,
    pub method: String,
    pub path: String,
    pub params: Vec<Param>,
    pub body: Option<Body>,
    /// Status key (`"201"`, `"4XX"`, `"default"`) → JSON body schema (None = no JSON body).
    pub responses: BTreeMap<String, Option<Value>>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub location: String,
    pub required: bool,
    pub schema: Value,
}

#[derive(Debug, Clone)]
pub struct Body {
    pub required: bool,
    pub schema: Value,
}

impl Spec {
    pub fn from_file(path: &Path) -> Result<Spec> {
        let raw =
            std::fs::read(path).with_context(|| format!("reading spec {}", path.display()))?;
        Spec::from_bytes(&raw, path.extension().and_then(|e| e.to_str()))
            .with_context(|| format!("parsing spec {}", path.display()))
    }

    pub fn from_bytes(raw: &[u8], ext_hint: Option<&str>) -> Result<Spec> {
        let doc: Value = match ext_hint {
            Some("yaml" | "yml") => serde_yaml_ng::from_slice(raw)?,
            Some("json") => serde_json::from_slice(raw)?,
            _ => serde_json::from_slice(raw).or_else(|_| serde_yaml_ng::from_slice(raw))?,
        };
        let sha256 = Sha256::digest(raw)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Spec::from_value(doc, sha256)
    }

    pub fn from_value(doc: Value, sha256: String) -> Result<Spec> {
        let resolved = resolve(&doc, &doc, 0);
        let info = resolved.get("info").cloned().unwrap_or(Value::Null);
        let mut operations = BTreeMap::new();
        let paths = resolved
            .get("paths")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("spec has no `paths`"))?;
        for (path, item) in paths {
            let shared_params = item
                .get("parameters")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for method in ["get", "put", "post", "delete", "patch", "head", "options"] {
                let Some(op) = item.get(method) else { continue };
                let id = op
                    .get("operationId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{} {path}", method.to_uppercase()));
                let mut params: Vec<Param> = vec![];
                let own = op
                    .get("parameters")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for p in shared_params.iter().chain(own.iter()) {
                    let name = p
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let location = p
                        .get("in")
                        .and_then(Value::as_str)
                        .unwrap_or("query")
                        .to_string();
                    params.retain(|q| !(q.name == name && q.location == location));
                    params.push(Param {
                        required: p
                            .get("required")
                            .and_then(Value::as_bool)
                            .unwrap_or(location == "path"),
                        schema: p.get("schema").cloned().unwrap_or(json!({})),
                        name,
                        location,
                    });
                }
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
                operations.insert(
                    id.clone(),
                    Operation {
                        id,
                        method: method.to_uppercase(),
                        path: path.clone(),
                        params,
                        body,
                        responses,
                    },
                );
            }
        }
        Ok(Spec {
            title: info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            version: info
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            sha256,
            operations,
        })
    }
}

impl Operation {
    /// Finds the response key that documents `status` (exact, then `2XX`, then `default`).
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

fn json_schema_of(v: &Value) -> Option<Value> {
    let content = v.get("content")?.as_object()?;
    content
        .iter()
        .find(|(ct, _)| ct.starts_with("application/json") || ct.ends_with("+json"))
        .and_then(|(_, m)| m.get("schema").cloned())
}

/// Inlines local `$ref`s and rewrites OpenAPI 3.0 `nullable` into JSON Schema.
fn resolve(v: &Value, root: &Value, depth: usize) -> Value {
    if depth > 32 {
        return json!({}); // recursive schema; stop expanding
    }
    match v {
        Value::Object(o) => {
            if let Some(r) = o.get("$ref").and_then(Value::as_str)
                && let Some(ptr) = r.strip_prefix('#')
                && let Some(target) = root.pointer(ptr)
            {
                return resolve(target, root, depth + 1);
            }
            let mut out = Map::new();
            for (k, val) in o {
                // Don't treat example payloads as schema.
                if k == "example" || k == "examples" {
                    continue;
                }
                out.insert(k.clone(), resolve(val, root, depth));
            }
            if out.get("nullable") == Some(&Value::Bool(true)) {
                out.remove("nullable");
                if let Some(Value::String(t)) = out.get("type").cloned() {
                    out.insert("type".into(), json!([t, "null"]));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve(x, root, depth)).collect()),
        other => other.clone(),
    }
}

/// What a suite actually used from one operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpUsage {
    pub op: String,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub params: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub request_fields: BTreeSet<String>,
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
pub struct Contract {
    pub name: String,
    pub spec_path: PathBuf,
    pub pinned: Spec,
    pub live_spec: Option<LiveSpec>,
    /// Service whose base URL this contract's operations are sent to (see `RunConfig::services`).
    pub service: Option<String>,
    pub mode: Compat,
    usage: Mutex<BTreeMap<String, OpUsage>>,
    validators: Mutex<HashMap<String, Arc<jsonschema::Validator>>>,
}

impl std::fmt::Debug for Contract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Contract")
            .field("name", &self.name)
            .field("version", &self.pinned.version)
            .field("spec", &self.spec_path)
            .finish()
    }
}

impl Contract {
    /// Loads a pinned spec from disk.
    pub fn pinned(name: &str, spec: impl AsRef<Path>) -> Result<Contract> {
        let spec_path = spec.as_ref().to_path_buf();
        Ok(Contract {
            name: name.into(),
            pinned: Spec::from_file(&spec_path)?,
            spec_path,
            live_spec: None,
            service: None,
            mode: Compat::Tolerant,
            usage: Mutex::default(),
            validators: Mutex::default(),
        })
    }

    /// Where to fetch the service's current spec from before the suite runs
    /// (absolute URL or a path relative to the base URL).
    pub fn live_spec(mut self, url: impl Into<String>) -> Self {
        self.live_spec = Some(LiveSpec::Url(url.into()));
        self
    }

    /// Uses an in-process OpenAPI document as the live spec. With utoipa:
    /// `.live_document(serde_json::to_value(ApiDoc::openapi())?)`.
    pub fn live_document(mut self, doc: Value) -> Self {
        self.live_spec = Some(LiveSpec::Document(doc));
        self
    }

    /// Sends this contract's operations to a named service (default: the service named
    /// like the contract, if configured, else the base URL).
    pub fn service(mut self, name: impl Into<String>) -> Self {
        self.service = Some(name.into());
        self
    }

    pub fn mode(mut self, mode: Compat) -> Self {
        self.mode = mode;
        self
    }

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

    pub fn usage(&self) -> Vec<OpUsage> {
        self.usage.lock().unwrap().values().cloned().collect()
    }

    pub(crate) fn record_request(&self, id: &str, opts: &RequestOpts) {
        let mut u = self.usage.lock().unwrap();
        let e = u.entry(id.to_string()).or_insert_with(|| OpUsage {
            op: id.into(),
            ..Default::default()
        });
        e.params.extend(opts.path.keys().cloned());
        e.params.extend(opts.query.keys().cloned());
        if let Some(Value::Object(o)) = &opts.json {
            e.request_fields.extend(o.keys().cloned());
        }
    }

    pub(crate) fn record_status(&self, id: &str, status: u16) {
        let mut u = self.usage.lock().unwrap();
        let e = u.entry(id.to_string()).or_insert_with(|| OpUsage {
            op: id.into(),
            ..Default::default()
        });
        e.statuses.insert(status);
    }

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

    fn check(&self, key: String, what: &str, schema: &Value, instance: &Value) -> Result<()> {
        let v = self.validator(key, schema)?;
        let errors: Vec<String> = v
            .iter_errors(instance)
            .take(5)
            .map(|e| {
                let p = e.instance_path().to_string();
                format!(
                    "  - {}: {e}",
                    if p.is_empty() { "<root>".into() } else { p }
                )
            })
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            bail!(
                "contract violation: {what} does not match pinned '{}' v{}:\n{}",
                self.name,
                self.pinned.version,
                errors.join("\n")
            )
        }
    }

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

    /// Diffs the pinned spec against `live`, restricted to `used` if given.
    pub fn diff(&self, live: &Spec, used: Option<&[OpUsage]>, source: &str) -> DriftReport {
        let mut findings = vec![];
        for (id, pinned_op) in &self.pinned.operations {
            let usage = match used {
                Some(list) => match list.iter().find(|u| &u.op == id) {
                    Some(u) => Some(u),
                    None => continue, // not used → not our problem
                },
                None => None,
            };
            diff_operation(
                pinned_op,
                live.operations.get(id),
                usage,
                self.mode,
                &mut findings,
            );
        }
        if self.mode == Compat::Strict && used.is_none() {
            for id in live
                .operations
                .keys()
                .filter(|id| !self.pinned.operations.contains_key(*id))
            {
                findings.push(Finding::new(
                    Severity::Info,
                    id,
                    "-",
                    "new operation in live spec",
                ));
            }
        }
        DriftReport {
            contract: self.name.clone(),
            pinned_version: self.pinned.version.clone(),
            live_version: live.version.clone(),
            source: source.to_string(),
            scope: match used {
                Some(u) => format!("{} used operation(s) from lockfile", u.len()),
                None => format!(
                    "all {} pinned operations (no lockfile usage yet)",
                    self.pinned.operations.len()
                ),
            },
            findings,
        }
    }
}

// ---------------------------------------------------------------------------
// Drift detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Breaking,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub operation: String,
    pub location: String,
    pub message: String,
}

impl Finding {
    fn new(severity: Severity, op: &str, loc: &str, msg: impl Into<String>) -> Self {
        Finding {
            severity,
            operation: op.into(),
            location: loc.into(),
            message: msg.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftReport {
    pub contract: String,
    pub pinned_version: String,
    pub live_version: String,
    pub source: String,
    pub scope: String,
    pub findings: Vec<Finding>,
}

impl DriftReport {
    pub fn is_breaking(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == Severity::Breaking)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    /// We send it: the live side must still accept what we send.
    Request,
    /// We receive it: the live side must not produce anything we don't expect.
    Response,
}

fn diff_operation(
    p: &Operation,
    live: Option<&Operation>,
    used: Option<&OpUsage>,
    mode: Compat,
    out: &mut Vec<Finding>,
) {
    let id = p.id.as_str();
    let Some(l) = live else {
        out.push(Finding::new(
            Severity::Breaking,
            id,
            "-",
            "operation removed from live spec",
        ));
        return;
    };
    if p.method != l.method || p.path != l.path {
        out.push(Finding::new(
            Severity::Breaking,
            id,
            "-",
            format!(
                "moved from {} {} to {} {}",
                p.method, p.path, l.method, l.path
            ),
        ));
    }

    // Parameters.
    for lp in &l.params {
        let known = p
            .params
            .iter()
            .any(|pp| pp.name == lp.name && pp.location == lp.location);
        if lp.required && !known {
            out.push(Finding::new(
                Severity::Breaking,
                id,
                &format!("{} param '{}'", lp.location, lp.name),
                "new required parameter",
            ));
        }
    }
    for pp in &p.params {
        let we_use = pp.required || used.is_some_and(|u| u.params.contains(&pp.name));
        if !we_use {
            continue;
        }
        let loc = format!("{} param '{}'", pp.location, pp.name);
        match l
            .params
            .iter()
            .find(|lp| lp.name == pp.name && lp.location == pp.location)
        {
            None => out.push(Finding::new(
                Severity::Breaking,
                id,
                &loc,
                "parameter removed",
            )),
            Some(lp) => {
                compare_schema(&pp.schema, &lp.schema, Dir::Request, mode, id, &loc, out, 0)
            }
        }
    }

    // Request body.
    match (&p.body, &l.body) {
        (None, Some(lb)) if lb.required => out.push(Finding::new(
            Severity::Breaking,
            id,
            "request",
            "request body is now required",
        )),
        (Some(_), None) => out.push(Finding::new(
            Severity::Warning,
            id,
            "request",
            "request body is no longer documented",
        )),
        (Some(pb), Some(lb)) => {
            let sent: BTreeSet<String> = match used {
                Some(u) if !u.request_fields.is_empty() => u.request_fields.clone(),
                _ => props(&pb.schema).keys().cloned().collect(),
            };
            for req in required(&lb.schema) {
                if !sent.contains(&req) {
                    out.push(Finding::new(
                        Severity::Breaking,
                        id,
                        &format!("request.{req}"),
                        "field is now required but this suite does not send it",
                    ));
                }
            }
            let lprops = props(&lb.schema);
            let pprops = props(&pb.schema);
            for f in &sent {
                let loc = format!("request.{f}");
                match (pprops.get(f), lprops.get(f)) {
                    (_, None)
                        if lb.schema.get("additionalProperties") == Some(&Value::Bool(false)) =>
                    {
                        out.push(Finding::new(
                            Severity::Breaking,
                            id,
                            &loc,
                            "field we send is rejected by the live spec",
                        ))
                    }
                    (_, None) => out.push(Finding::new(
                        Severity::Warning,
                        id,
                        &loc,
                        "field we send is no longer documented (may be ignored)",
                    )),
                    (Some(ps), Some(ls)) => {
                        compare_schema(ps, ls, Dir::Request, mode, id, &loc, out, 0)
                    }
                    (None, Some(_)) => {}
                }
            }
        }
        _ => {}
    }

    // Responses.
    let statuses: Vec<String> = match used {
        Some(u) if !u.statuses.is_empty() => u.statuses.iter().map(|s| s.to_string()).collect(),
        _ => p
            .responses
            .keys()
            .filter(|k| k.starts_with('2'))
            .cloned()
            .collect(),
    };
    for s in statuses {
        let code: u16 = s.parse().unwrap_or(0);
        let pkey = if code > 0 {
            p.response_key(code).map(str::to_string)
        } else {
            Some(s.clone())
        };
        let lkey = if code > 0 {
            l.response_key(code).map(str::to_string)
        } else {
            l.responses.contains_key(&s).then(|| s.clone())
        };
        let loc = format!("response {s}");
        match (pkey, lkey) {
            (_, None) => out.push(Finding::new(
                Severity::Breaking,
                id,
                &loc,
                "status no longer documented",
            )),
            (None, Some(_)) => {}
            (Some(pk), Some(lk)) => match (p.responses[&pk].as_ref(), l.responses[&lk].as_ref()) {
                (Some(ps), Some(ls)) => {
                    compare_schema(ps, ls, Dir::Response, mode, id, &loc, out, 0)
                }
                (Some(_), None) => out.push(Finding::new(
                    Severity::Breaking,
                    id,
                    &loc,
                    "JSON body no longer documented",
                )),
                (None, Some(_)) if mode == Compat::Strict => out.push(Finding::new(
                    Severity::Breaking,
                    id,
                    &loc,
                    "body added (strict)",
                )),
                _ => {}
            },
        }
    }
    if mode == Compat::Strict {
        for k in l.responses.keys().filter(|k| !p.responses.contains_key(*k)) {
            out.push(Finding::new(
                Severity::Info,
                id,
                &format!("response {k}"),
                "new status documented",
            ));
        }
    }
}

fn list(values: &[&Value]) -> String {
    values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn props(schema: &Value) -> Map<String, Value> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn required(schema: &Value) -> BTreeSet<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn types(schema: &Value) -> BTreeSet<String> {
    let mut t: BTreeSet<String> = match schema.get("type") {
        Some(Value::String(s)) => [s.clone()].into(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => BTreeSet::new(),
    };
    // "integer" is a subset of "number".
    if t.contains("number") {
        t.insert("integer".into());
    }
    t
}

#[allow(clippy::too_many_arguments)]
fn compare_schema(
    p: &Value,
    l: &Value,
    dir: Dir,
    mode: Compat,
    op: &str,
    loc: &str,
    out: &mut Vec<Finding>,
    depth: usize,
) {
    if depth > 16 {
        return;
    }
    let (tp, tl) = (types(p), types(l));
    if !tp.is_empty() && !tl.is_empty() {
        let compatible = match dir {
            Dir::Response => tl.is_subset(&tp),
            Dir::Request => tp.is_subset(&tl),
        };
        if !compatible {
            let fmt = |s: &BTreeSet<String>| {
                s.iter()
                    .filter(|t| !(t.as_str() == "integer" && s.contains("number")))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("|")
            };
            out.push(Finding::new(
                Severity::Breaking,
                op,
                loc,
                format!("type changed from {} to {}", fmt(&tp), fmt(&tl)),
            ));
            return;
        }
    }
    if p.get("format") != l.get("format") && p.get("format").is_some() {
        out.push(Finding::new(
            Severity::Warning,
            op,
            loc,
            format!(
                "format changed from {} to {}",
                p["format"],
                l.get("format").unwrap_or(&Value::Null)
            ),
        ));
    }

    // Enums.
    if let (Some(pe), Some(le)) = (
        p.get("enum").and_then(Value::as_array),
        l.get("enum").and_then(Value::as_array),
    ) {
        match dir {
            Dir::Response => {
                let added: Vec<&Value> = le.iter().filter(|v| !pe.contains(v)).collect();
                if !added.is_empty() {
                    let sev = if mode == Compat::Strict {
                        Severity::Breaking
                    } else {
                        Severity::Warning
                    };
                    out.push(Finding::new(
                        sev,
                        op,
                        loc,
                        format!("enum gained values {}", list(&added)),
                    ));
                }
            }
            Dir::Request => {
                let removed: Vec<&Value> = pe.iter().filter(|v| !le.contains(v)).collect();
                if !removed.is_empty() {
                    out.push(Finding::new(
                        Severity::Breaking,
                        op,
                        loc,
                        format!("enum lost values {}", list(&removed)),
                    ));
                }
            }
        }
    }

    // Object properties.
    let (pp, lp) = (props(p), props(l));
    if !pp.is_empty() || !lp.is_empty() {
        let (rp, rl) = (required(p), required(l));
        match dir {
            Dir::Response => {
                for (name, ps) in &pp {
                    let sub = format!("{loc}.{name}");
                    match lp.get(name) {
                        None => out.push(Finding::new(
                            Severity::Breaking,
                            op,
                            &sub,
                            "property removed",
                        )),
                        Some(ls) => {
                            if rp.contains(name) && !rl.contains(name) {
                                out.push(Finding::new(
                                    Severity::Breaking,
                                    op,
                                    &sub,
                                    "property is no longer required (may be absent)",
                                ));
                            }
                            compare_schema(ps, ls, dir, mode, op, &sub, out, depth + 1);
                        }
                    }
                }
                for name in lp.keys().filter(|n| !pp.contains_key(*n)) {
                    let sev = if mode == Compat::Strict {
                        Severity::Breaking
                    } else {
                        Severity::Info
                    };
                    out.push(Finding::new(
                        sev,
                        op,
                        &format!("{loc}.{name}"),
                        "new property",
                    ));
                }
            }
            Dir::Request => {
                for name in rl.difference(&rp) {
                    if !pp.contains_key(name) {
                        out.push(Finding::new(
                            Severity::Breaking,
                            op,
                            &format!("{loc}.{name}"),
                            "new required property",
                        ));
                    }
                }
                for (name, ps) in &pp {
                    if let Some(ls) = lp.get(name) {
                        compare_schema(
                            ps,
                            ls,
                            dir,
                            mode,
                            op,
                            &format!("{loc}.{name}"),
                            out,
                            depth + 1,
                        );
                    }
                }
            }
        }
    }

    // Arrays.
    if let (Some(pi), Some(li)) = (p.get("items"), l.get("items")) {
        compare_schema(pi, li, dir, mode, op, &format!("{loc}[]"), out, depth + 1);
    }
}

// ---------------------------------------------------------------------------
// Lockfile
// ---------------------------------------------------------------------------

/// `apitest.lock.nova`: what each contract is pinned to and which surface the suites use.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(default)]
    pub contract: BTreeMap<String, LockedContract>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedContract {
    pub spec: String,
    pub version: String,
    pub sha256: String,
    #[serde(default)]
    pub used: Vec<OpUsage>,
}

impl Lockfile {
    pub fn load(path: &Path) -> Result<Option<Lockfile>> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(crate::config::from_nova(
                &s,
                crate::config::LOCK_SCHEMA,
                &path.display().to_string(),
            )?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let body =
            novafile_serde::to_string(self).map_err(|e| anyhow!("encoding lockfile: {e}"))?;
        std::fs::write(
            path,
            format!(
                "# Generated by apitest. Records the API surface your suites depend on.\n# Commit this file.\n@schema apitest-lock@0.1.0\n\n{body}"
            ),
        )?;
        Ok(())
    }

    /// Merges a contract's recorded usage into the lockfile.
    pub fn record(&mut self, c: &Contract, lock_dir: &Path) {
        let spec = pathdiff(&c.spec_path, lock_dir);
        let entry = self
            .contract
            .entry(c.name.clone())
            .or_insert_with(|| LockedContract {
                spec: spec.clone(),
                version: c.pinned.version.clone(),
                sha256: c.pinned.sha256.clone(),
                used: vec![],
            });
        if entry.sha256 != c.pinned.sha256 {
            // Re-pinned to a different spec: start the usage record over.
            entry.used.clear();
        }
        entry.spec = spec;
        entry.version = c.pinned.version.clone();
        entry.sha256 = c.pinned.sha256.clone();
        for u in c.usage() {
            match entry.used.iter_mut().find(|e| e.op == u.op) {
                Some(e) => {
                    e.params.extend(u.params);
                    e.request_fields.extend(u.request_fields);
                    e.statuses.extend(u.statuses);
                }
                None => entry.used.push(u),
            }
        }
        entry.used.sort_by(|a, b| a.op.cmp(&b.op));
    }
}

fn pathdiff(target: &Path, base: &Path) -> String {
    let (Ok(t), Ok(b)) = (target.canonicalize(), base.canonicalize()) else {
        return target.display().to_string();
    };
    let tc: Vec<_> = t.components().collect();
    let bc: Vec<_> = b.components().collect();
    let common = tc.iter().zip(&bc).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..bc.len() {
        out.push("..");
    }
    for c in &tc[common..] {
        out.push(c);
    }
    out.display().to_string()
}
