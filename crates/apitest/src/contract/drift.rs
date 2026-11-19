//! Drift detection: diffing a pinned spec against the service's live spec.
//!
//! The diff is direction-aware. For data the suite *sends* (parameters and
//! request bodies) the live side must still accept everything the pinned side
//! accepted. For data the suite *receives* (responses) the live side must not
//! produce anything the pinned side did not describe. [`Compat`] decides whether
//! purely additive changes count as breaking.
//!
//! ```mermaid
//! flowchart LR
//!     diff["Contract::diff"] --> op["diff_operation (per used op)"]
//!     op --> route --> params --> body["request body"] --> responses
//!     params & body & responses --> schema["Differ::compare_schema (recursive)"]
//! ```

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{Body, Compat, Contract, LIVE, OpUsage, Operation, Param, Spec};

/// Deepest schema nesting [`Differ::compare_schema`] descends into. Anything
/// below is not compared, which bounds the work on recursive schemas.
const MAX_SCHEMA_DEPTH: usize = 16;

/// How bad a drift [`Finding`] is. Ordered from least to most severe, so
/// `max()` over a report yields its overall severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The live spec changed in a way the suite does not depend on, such as a
    /// new operation or a new optional response property in tolerant mode.
    Info,
    /// The change may matter, but does not break the used surface on its own,
    /// such as a changed `format` or a sent field that is no longer documented.
    Warning,
    /// The used surface no longer holds. A breaking finding fails the suite.
    Breaking,
}

/// One difference between the pinned and the live spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// How bad the difference is.
    pub severity: Severity,
    /// Id of the affected operation (its `operationId`, or `METHOD /path` when
    /// the spec has none).
    pub operation: String,
    /// Where in the operation the difference is: `-` for the operation as a
    /// whole, otherwise e.g. `query param 'page'`, `request.email` or
    /// `response 200.items[].id`.
    pub location: String,
    /// Human-readable description of the change.
    pub message: String,
}

impl Finding {
    /// Builds a finding; `op` and `loc` fill [`Finding::operation`] and
    /// [`Finding::location`].
    fn new(severity: Severity, op: &str, loc: &str, msg: impl Into<String>) -> Self {
        Finding {
            severity,
            operation: op.into(),
            location: loc.into(),
            message: msg.into(),
        }
    }
}

/// The result of diffing one contract against one drift target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftReport {
    /// Name of the diffed [`Contract`].
    pub contract: String,
    /// What the pin was compared against: [`LIVE`] or the name of a
    /// [`DriftTarget`](super::DriftTarget). Reports written before targets
    /// existed read as [`LIVE`].
    #[serde(default = "live_target")]
    pub target: String,
    /// `info.version` of the pinned spec.
    pub pinned_version: String,
    /// `info.version` of the compared spec (the live one, or the target's).
    pub live_version: String,
    /// Where the compared spec came from (a URL, a file path or a description such
    /// as `in-process document`), for display only.
    pub source: String,
    /// Human-readable description of which operations were compared.
    pub scope: String,
    /// Every difference found, in operation order. Empty when nothing drifted.
    pub findings: Vec<Finding>,
}

/// The serde default of [`DriftReport::target`].
fn live_target() -> String {
    LIVE.to_string()
}

impl DriftReport {
    /// The name of this check in reports: `billing` for the live spec,
    /// `billing @ prod` for a named target, so live-only runs keep their names.
    #[must_use]
    pub fn label(&self) -> String {
        if self.target == LIVE {
            self.contract.clone()
        } else {
            format!("{} @ {}", self.contract, self.target)
        }
    }

    /// Whether any finding is [`Severity::Breaking`], i.e. whether the suite
    /// must fail.
    #[must_use]
    pub fn is_breaking(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == Severity::Breaking)
    }

    /// Masks secrets and redaction patterns in every text of the report: a
    /// target URL or a finding may carry a key or a customer id.
    pub fn redact(&mut self, r: &crate::redact::Redactor) {
        for text in [
            &mut self.contract,
            &mut self.target,
            &mut self.pinned_version,
            &mut self.live_version,
            &mut self.source,
            &mut self.scope,
        ] {
            r.redact_in_place(text);
        }

        for f in &mut self.findings {
            r.redact_in_place(&mut f.operation);
            r.redact_in_place(&mut f.location);
            r.redact_in_place(&mut f.message);
        }
    }
}

impl Contract {
    /// Diffs the pinned spec against `live`.
    ///
    /// With `used` (the usage recorded in the lockfile), only operations the
    /// suites used are compared, and within them only the parameters, request
    /// fields and statuses they used. Without it, every pinned operation is
    /// compared, using its required parameters, all documented request fields
    /// and its `2xx` statuses; in [`Compat::Strict`] mode operations that only
    /// exist in `live` are reported as [`Severity::Info`]. `source` is copied
    /// into [`DriftReport::source`]; [`DriftReport::target`] is [`LIVE`], which
    /// the drift check replaces when it compares against a named target.
    #[must_use]
    pub fn diff(&self, live: &Spec, used: Option<&[OpUsage]>, source: &str) -> DriftReport {
        let mut findings = vec![];
        for (id, pinned_op) in &self.pinned.operations {
            let usage = match used {
                Some(list) => match list.iter().find(|u| &u.op == id) {
                    Some(u) => Some(u),
                    None => continue,
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
            findings.extend(self.new_operations(live));
        }

        DriftReport {
            contract: self.name.clone(),
            target: LIVE.to_string(),
            pinned_version: self.pinned.version.clone(),
            live_version: live.version.clone(),
            source: source.to_string(),
            scope: self.diff_scope(used),
            findings,
        }
    }

    /// One [`Severity::Info`] finding per operation that exists in `live` but
    /// not in the pinned spec.
    fn new_operations<'a>(&'a self, live: &'a Spec) -> impl Iterator<Item = Finding> + 'a {
        live.operations
            .keys()
            .filter(|id| !self.pinned.operations.contains_key(*id))
            .map(|id| Finding::new(Severity::Info, id, "-", "new operation in live spec"))
    }

    /// The text for [`DriftReport::scope`].
    fn diff_scope(&self, used: Option<&[OpUsage]>) -> String {
        match used {
            Some(u) => format!("{} used operation(s) from lockfile", u.len()),
            None => format!(
                "all {} pinned operations (no lockfile usage yet)",
                self.pinned.operations.len()
            ),
        }
    }
}

/// Which way data flows through the compared schema.
#[derive(Clone, Copy, PartialEq)]
enum Dir {
    /// We send it: the live side must still accept what we send.
    Request,
    /// We receive it: the live side must not produce anything we don't expect.
    Response,
}

/// Diffs one pinned operation against its live counterpart and appends the
/// findings to `findings`. `live` is `None` when the operation is gone.
fn diff_operation(
    pinned: &Operation,
    live: Option<&Operation>,
    used: Option<&OpUsage>,
    mode: Compat,
    findings: &mut Vec<Finding>,
) {
    let mut d = Differ {
        op: &pinned.id,
        mode,
        findings,
    };
    let Some(live) = live else {
        d.push(Severity::Breaking, "-", "operation removed from live spec");
        return;
    };

    d.route(pinned, live);
    d.params(pinned, live, used);
    d.request_body(pinned, live, used);
    d.responses(pinned, live, used);
}

/// Collects the findings for one operation while walking the pinned and the
/// live side in parallel.
struct Differ<'a> {
    /// Id of the operation every finding is attributed to.
    op: &'a str,
    /// Decides whether additive changes are breaking.
    mode: Compat,
    /// Where findings are appended, in discovery order.
    findings: &'a mut Vec<Finding>,
}

impl Differ<'_> {
    /// Appends a finding for this operation.
    fn push(&mut self, severity: Severity, loc: &str, msg: impl Into<String>) {
        self.findings
            .push(Finding::new(severity, self.op, loc, msg));
    }

    /// Severity of an additive change: [`Severity::Breaking`] in strict mode,
    /// `tolerant` otherwise.
    fn additive(&self, tolerant: Severity) -> Severity {
        if self.mode == Compat::Strict {
            Severity::Breaking
        } else {
            tolerant
        }
    }

    /// Reports an operation whose method or path changed.
    fn route(&mut self, p: &Operation, l: &Operation) {
        if p.method != l.method || p.path != l.path {
            self.push(
                Severity::Breaking,
                "-",
                format!(
                    "moved from {} {} to {} {}",
                    p.method, p.path, l.method, l.path
                ),
            );
        }
    }

    /// Compares path, query, header and cookie parameters.
    fn params(&mut self, p: &Operation, l: &Operation, used: Option<&OpUsage>) {
        self.new_required_params(p, l);
        self.used_params(p, l, used);
    }

    /// Reports required live parameters the pinned spec does not know, since
    /// the suite cannot be sending them.
    fn new_required_params(&mut self, p: &Operation, l: &Operation) {
        for lp in &l.params {
            let known = p.params.iter().any(|pp| same_param(pp, lp));
            if lp.required && !known {
                self.push(Severity::Breaking, &param_loc(lp), "new required parameter");
            }
        }
    }

    /// Checks every pinned parameter the suite sends (required ones always,
    /// optional ones when the usage records them) is still accepted.
    fn used_params(&mut self, p: &Operation, l: &Operation, used: Option<&OpUsage>) {
        for pp in &p.params {
            let we_use = pp.required || used.is_some_and(|u| u.params.contains(&pp.name));
            if !we_use {
                continue;
            }

            let loc = param_loc(pp);
            match l.params.iter().find(|lp| same_param(lp, pp)) {
                None => self.push(Severity::Breaking, &loc, "parameter removed"),
                Some(lp) => self.compare_schema(&pp.schema, &lp.schema, Dir::Request, &loc, 0),
            }
        }
    }

    /// Compares the request body: whether it appeared, disappeared, or what
    /// changed about the fields the suite sends.
    fn request_body(&mut self, p: &Operation, l: &Operation, used: Option<&OpUsage>) {
        match (&p.body, &l.body) {
            (None, Some(lb)) if lb.required => self.push(
                Severity::Breaking,
                "request",
                "request body is now required",
            ),
            (Some(_), None) => self.push(
                Severity::Warning,
                "request",
                "request body is no longer documented",
            ),
            (Some(pb), Some(lb)) => self.request_fields(pb, lb, used),
            _ => {}
        }
    }

    /// Checks that the live body requires nothing the suite does not send, and
    /// still accepts every field it does send.
    fn request_fields(&mut self, pb: &Body, lb: &Body, used: Option<&OpUsage>) {
        let sent = sent_fields(pb, used);
        for req in required(&lb.schema) {
            if !sent.contains(&req) {
                self.push(
                    Severity::Breaking,
                    &format!("request.{req}"),
                    "field is now required but this suite does not send it",
                );
            }
        }

        let lprops = props(&lb.schema);
        let pprops = props(&pb.schema);
        let closed = lb.schema.get("additionalProperties") == Some(&Value::Bool(false));
        for f in &sent {
            let loc = format!("request.{f}");
            match (pprops.get(f), lprops.get(f)) {
                (_, None) if closed => self.push(
                    Severity::Breaking,
                    &loc,
                    "field we send is rejected by the live spec",
                ),
                (_, None) => self.push(
                    Severity::Warning,
                    &loc,
                    "field we send is no longer documented (may be ignored)",
                ),
                (Some(ps), Some(ls)) => self.compare_schema(ps, ls, Dir::Request, &loc, 0),
                (None, Some(_)) => {}
            }
        }
    }

    /// Compares the statuses the suite relies on and, in strict mode, reports
    /// statuses that only the live side documents.
    fn responses(&mut self, p: &Operation, l: &Operation, used: Option<&OpUsage>) {
        for status in checked_statuses(p, used) {
            self.response(p, l, &status);
        }

        if self.mode == Compat::Strict {
            for k in l.responses.keys().filter(|k| !p.responses.contains_key(*k)) {
                self.push(
                    Severity::Info,
                    &format!("response {k}"),
                    "new status documented",
                );
            }
        }
    }

    /// Compares one status. `status` is a concrete code (`201`), matched via
    /// [`Operation::response_key`], or a literal key such as `2XX`.
    fn response(&mut self, p: &Operation, l: &Operation, status: &str) {
        let code: u16 = status.parse().unwrap_or(0);
        let (pkey, lkey) = if code > 0 {
            (p.response_key(code), l.response_key(code))
        } else {
            (
                Some(status),
                l.responses.contains_key(status).then_some(status),
            )
        };

        let loc = format!("response {status}");
        match (pkey, lkey) {
            (_, None) => self.push(Severity::Breaking, &loc, "status no longer documented"),
            (None, Some(_)) => {}
            (Some(pk), Some(lk)) => {
                self.response_body(p.responses[pk].as_ref(), l.responses[lk].as_ref(), &loc);
            }
        }
    }

    /// Compares the JSON body schemas of one documented status (`None` = no
    /// JSON body).
    fn response_body(&mut self, ps: Option<&Value>, ls: Option<&Value>, loc: &str) {
        match (ps, ls) {
            (Some(ps), Some(ls)) => self.compare_schema(ps, ls, Dir::Response, loc, 0),
            (Some(_), None) => self.push(Severity::Breaking, loc, "JSON body no longer documented"),
            (None, Some(_)) if self.mode == Compat::Strict => {
                self.push(Severity::Breaking, loc, "body added (strict)");
            }
            _ => {}
        }
    }

    /// Recursively compares two JSON schemas at `loc`. Stops at the first type
    /// incompatibility, since everything below it is moot, and below
    /// [`MAX_SCHEMA_DEPTH`].
    fn compare_schema(&mut self, p: &Value, l: &Value, dir: Dir, loc: &str, depth: usize) {
        if depth > MAX_SCHEMA_DEPTH || !self.compare_types(p, l, dir, loc) {
            return;
        }

        self.compare_format(p, l, loc);
        self.compare_enums(p, l, dir, loc);
        self.compare_properties(p, l, dir, loc, depth);
        if let (Some(pi), Some(li)) = (p.get("items"), l.get("items")) {
            self.compare_schema(pi, li, dir, &format!("{loc}[]"), depth + 1);
        }
    }

    /// Reports an incompatible `type` change and returns whether the types are
    /// compatible. Schemas without a `type` are compatible with anything.
    fn compare_types(&mut self, p: &Value, l: &Value, dir: Dir, loc: &str) -> bool {
        let (tp, tl) = (types(p), types(l));
        if tp.is_empty() || tl.is_empty() {
            return true;
        }

        let compatible = match dir {
            Dir::Response => tl.is_subset(&tp),
            Dir::Request => tp.is_subset(&tl),
        };
        if !compatible {
            self.push(
                Severity::Breaking,
                loc,
                format!(
                    "type changed from {} to {}",
                    display_types(&tp),
                    display_types(&tl)
                ),
            );
        }

        compatible
    }

    /// Warns when a pinned `format` changed or disappeared.
    fn compare_format(&mut self, p: &Value, l: &Value, loc: &str) {
        if let Some(pf) = p.get("format")
            && l.get("format") != Some(pf)
        {
            self.push(
                Severity::Warning,
                loc,
                format!(
                    "format changed from {pf} to {}",
                    l.get("format").unwrap_or(&Value::Null)
                ),
            );
        }
    }

    /// Reports enum values the live side gained (responses) or lost
    /// (requests). Only applies when both sides declare an `enum`.
    fn compare_enums(&mut self, p: &Value, l: &Value, dir: Dir, loc: &str) {
        let (Some(pe), Some(le)) = (
            p.get("enum").and_then(Value::as_array),
            l.get("enum").and_then(Value::as_array),
        ) else {
            return;
        };

        match dir {
            Dir::Response => {
                let added: Vec<&Value> = le.iter().filter(|v| !pe.contains(v)).collect();
                if !added.is_empty() {
                    let sev = self.additive(Severity::Warning);
                    self.push(sev, loc, format!("enum gained values {}", list(&added)));
                }
            }
            Dir::Request => {
                let removed: Vec<&Value> = pe.iter().filter(|v| !le.contains(v)).collect();
                if !removed.is_empty() {
                    self.push(
                        Severity::Breaking,
                        loc,
                        format!("enum lost values {}", list(&removed)),
                    );
                }
            }
        }
    }

    /// Compares object `properties` and `required` lists, if either side has
    /// properties.
    fn compare_properties(&mut self, p: &Value, l: &Value, dir: Dir, loc: &str, depth: usize) {
        let (p, l) = (ObjectShape::of(p), ObjectShape::of(l));
        if p.props.is_empty() && l.props.is_empty() {
            return;
        }

        match dir {
            Dir::Response => self.response_properties(&p, &l, loc, depth),
            Dir::Request => self.request_properties(&p, &l, loc, depth),
        }
    }

    /// Received objects: every pinned property must still be there and stay
    /// required if it was; new properties are additive.
    fn response_properties(&mut self, p: &ObjectShape, l: &ObjectShape, loc: &str, depth: usize) {
        for (name, ps) in &p.props {
            let sub = format!("{loc}.{name}");
            let Some(ls) = l.props.get(name) else {
                self.push(Severity::Breaking, &sub, "property removed");
                continue;
            };

            if p.required.contains(name) && !l.required.contains(name) {
                self.push(
                    Severity::Breaking,
                    &sub,
                    "property is no longer required (may be absent)",
                );
            }

            self.compare_schema(ps, ls, Dir::Response, &sub, depth + 1);
        }

        let sev = self.additive(Severity::Info);
        for name in l.props.keys().filter(|n| !p.props.contains_key(*n)) {
            self.push(sev, &format!("{loc}.{name}"), "new property");
        }
    }

    /// Sent objects: the live side must not require properties the pinned
    /// side does not know, and shared properties must still accept our values.
    fn request_properties(&mut self, p: &ObjectShape, l: &ObjectShape, loc: &str, depth: usize) {
        for name in l.required.difference(&p.required) {
            if !p.props.contains_key(name) {
                self.push(
                    Severity::Breaking,
                    &format!("{loc}.{name}"),
                    "new required property",
                );
            }
        }

        for (name, ps) in &p.props {
            if let Some(ls) = l.props.get(name) {
                self.compare_schema(ps, ls, Dir::Request, &format!("{loc}.{name}"), depth + 1);
            }
        }
    }
}

/// The object-related parts of one schema.
struct ObjectShape {
    /// `properties`, name to schema. Empty when the schema declares none.
    props: Map<String, Value>,
    /// Names listed in `required`.
    required: BTreeSet<String>,
}

impl ObjectShape {
    /// Extracts the object shape of `schema`.
    fn of(schema: &Value) -> Self {
        ObjectShape {
            props: props(schema),
            required: required(schema),
        }
    }
}

/// Whether two parameters occupy the same slot (same name in the same location).
fn same_param(a: &Param, b: &Param) -> bool {
    a.name == b.name && a.location == b.location
}

/// The [`Finding::location`] of a parameter, e.g. `query param 'page'`.
fn param_loc(p: &Param) -> String {
    format!("{} param '{}'", p.location, p.name)
}

/// The request fields the suite sends: the recorded ones if any, otherwise
/// every property the pinned body documents.
fn sent_fields(pb: &Body, used: Option<&OpUsage>) -> BTreeSet<String> {
    match used {
        Some(u) if !u.request_fields.is_empty() => u.request_fields.clone(),
        _ => props(&pb.schema).keys().cloned().collect(),
    }
}

/// The statuses to compare: the recorded ones if any, otherwise every pinned
/// `2xx` key (which may be a literal such as `2XX`).
fn checked_statuses(p: &Operation, used: Option<&OpUsage>) -> Vec<String> {
    match used {
        Some(u) if !u.statuses.is_empty() => u
            .statuses
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        _ => p
            .responses
            .keys()
            .filter(|k| k.starts_with('2'))
            .cloned()
            .collect(),
    }
}

/// Renders JSON values as a comma-separated list for messages.
fn list(values: &[&Value]) -> String {
    values
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `properties` of `schema`, or an empty map.
fn props(schema: &Value) -> Map<String, Value> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// The string entries of `schema.required`, or an empty set.
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

/// The JSON Schema `type`s of `schema`, as a set. `number` implies `integer`,
/// since every integer is a number. Empty when no `type` is given.
fn types(schema: &Value) -> BTreeSet<String> {
    let mut t: BTreeSet<String> = match schema.get("type") {
        Some(Value::String(s)) => [s.clone()].into(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => BTreeSet::new(),
    };
    if t.contains("number") {
        t.insert("integer".into());
    }

    t
}

/// Renders a [`types`] set as `a|b`, hiding the `integer` that [`types`]
/// derived from `number`.
fn display_types(s: &BTreeSet<String>) -> String {
    s.iter()
        .filter(|t| !(t.as_str() == "integer" && s.contains("number")))
        .cloned()
        .collect::<Vec<_>>()
        .join("|")
}
