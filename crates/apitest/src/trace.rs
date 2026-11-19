//! Trace-context propagation: W3C Trace Context (default), Zipkin B3, AWS X-Ray,
//! Google Cloud Trace and Datadog headers on every request.
//!
//! * **On by default**, opt-out globally ([`TracingConfig::enabled`]), per suite
//!   ([`crate::Suite::tracing`]) or per request (`.no_trace()` / `{ trace: false }`).
//! * One **trace per test** by default ([`TraceScope`]); every request is a new span
//!   (fresh span id), so a test's requests correlate in your backend.
//! * All formats carry the **same 128-bit trace id**. Ids start with the Unix time
//!   in seconds, which keeps them valid X-Ray roots (`1-<time>-<rest>`).
//! * If the run itself is part of a trace ([`TracingConfig::parent`], or the
//!   `TRACEPARENT`/`TRACESTATE` environment variables with
//!   [`TracingConfig::inherit_env`]), all requests join that trace (its trace id,
//!   new span ids), so per-test trace ids give way to the parent's.
//! * Headers set explicitly on a request (e.g. your own `traceparent`) always win.
//! * Tests can read what was sent: [`crate::Ctx::trace`] for the current trace,
//!   [`crate::Response::request`] / [`crate::Response::trace`] per request.

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Header formats to send. Combine freely, e.g. `[w3c, xray]` behind an AWS ALB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceFormat {
    /// `traceparent: 00-<trace-id>-<span-id>-<flags>` (+ `tracestate`, `baggage`).
    /// Also what Azure Monitor / Application Insights use.
    W3c,
    /// Zipkin single header `b3: <trace-id>-<span-id>-<sampled>`.
    B3,
    /// Zipkin multi headers `X-B3-TraceId`, `X-B3-SpanId`, `X-B3-Sampled`.
    B3multi,
    /// AWS `X-Amzn-Trace-Id: Root=1-<time>-<id>;Parent=<span-id>;Sampled=<0|1>`.
    Xray,
    /// Google Cloud `X-Cloud-Trace-Context: <trace-id>/<span-id decimal>;o=<0|1>`.
    Gcp,
    /// Datadog `x-datadog-trace-id`, `x-datadog-parent-id`, `x-datadog-sampling-priority`,
    /// `x-datadog-tags: _dd.p.tid=<high 64 bits>`.
    Datadog,
}

impl std::str::FromStr for TraceFormat {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "w3c" | "traceparent" | "otel" | "azure" => TraceFormat::W3c,
            "b3" => TraceFormat::B3,
            "b3multi" | "b3-multi" => TraceFormat::B3multi,
            "xray" | "aws" => TraceFormat::Xray,
            "gcp" | "google" | "cloudtrace" => TraceFormat::Gcp,
            "datadog" | "dd" => TraceFormat::Datadog,
            o => bail!("unknown trace format '{o}' (w3c, b3, b3multi, xray, gcp, datadog)"),
        })
    }
}

/// How long one trace id lives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceScope {
    /// Every request is its own trace.
    Request,
    /// One trace per test execution (per iteration under load). Requests made by
    /// suite fixtures share the suite's trace.
    #[default]
    Test,
    /// One trace per suite.
    Suite,
    /// One trace for the whole run.
    Run,
}

/// Run-wide tracing settings (`tracing:` in `apitest.nova`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TracingConfig {
    pub enabled: bool,
    pub formats: Vec<TraceFormat>,
    /// Sampled flag sent with every format (W3C flags `01` / `00`).
    pub sampled: bool,
    pub scope: TraceScope,
    /// W3C `tracestate` to send, e.g. `vendor=value`.
    pub tracestate: Option<String>,
    /// Static W3C `baggage` entries.
    pub baggage: BTreeMap<String, String>,
    /// Adds `apitest.suite` / `apitest.test` to `baggage`, so the service can log
    /// which test a request came from.
    pub test_baggage: bool,
    /// Parent `traceparent` for the whole run. Default: the `TRACEPARENT` environment
    /// variable (if [`TracingConfig::inherit_env`]).
    pub parent: Option<String>,
    /// Read `TRACEPARENT` / `TRACESTATE` from the environment (OpenTelemetry's
    /// environment-carrier convention). Off by default: CI platforms often export a
    /// `TRACEPARENT`, which would put every test into one trace.
    pub inherit_env: bool,
}

impl Default for TracingConfig {
    fn default() -> Self {
        TracingConfig {
            enabled: true,
            formats: vec![TraceFormat::W3c],
            sampled: true,
            scope: TraceScope::Test,
            tracestate: None,
            baggage: BTreeMap::new(),
            test_baggage: false,
            parent: None,
            inherit_env: false,
        }
    }
}

impl TracingConfig {
    #[must_use]
    pub fn disabled() -> Self {
        TracingConfig {
            enabled: false,
            ..Default::default()
        }
    }

    pub fn formats(mut self, formats: impl IntoIterator<Item = TraceFormat>) -> Self {
        self.formats = formats.into_iter().collect();
        self
    }

    #[must_use]
    pub fn scope(mut self, scope: TraceScope) -> Self {
        self.scope = scope;
        self
    }

    /// The effective parent context, if any (explicit `parent`, else `TRACEPARENT`).
    pub(crate) fn resolved_parent(&self) -> Result<Option<(Traceparent, Option<String>)>> {
        let env = |k| {
            if self.inherit_env {
                std::env::var(k).ok().filter(|v: &String| !v.is_empty())
            } else {
                None
            }
        };
        let Some(tp) = self.parent.clone().or_else(|| env("TRACEPARENT")) else {
            return Ok(None);
        };
        let parsed = Traceparent::parse(&tp)
            .map_err(|e| anyhow!("invalid parent traceparent '{tp}': {e}"))?;
        Ok(Some((
            parsed,
            self.tracestate.clone().or_else(|| env("TRACESTATE")),
        )))
    }
}

/// A trace the runner propagates: what [`crate::Ctx::trace`] returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    /// 32 lowercase hex digits.
    pub trace_id: String,
    pub sampled: bool,
    pub tracestate: Option<String>,
    /// The inherited parent span (from `TRACEPARENT`), if any.
    pub parent_span_id: Option<String>,
}

/// What was sent with one request: available as `resp.trace`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanContext {
    pub trace_id: String,
    /// 16 lowercase hex digits, unique per request.
    pub span_id: String,
    pub sampled: bool,
}

impl SpanContext {
    /// The W3C `traceparent` value of this span.
    #[must_use]
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{}",
            self.trace_id,
            self.span_id,
            if self.sampled { "01" } else { "00" }
        )
    }
}

/// A parsed W3C `traceparent` (useful for validating what a service echoes back).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traceparent {
    pub version: String,
    pub trace_id: String,
    pub span_id: String,
    pub flags: u8,
}

impl Traceparent {
    #[must_use]
    pub fn sampled(&self) -> bool {
        self.flags & 1 == 1
    }

    /// Parses and validates `00-<32 hex>-<16 hex>-<2 hex>` per W3C Trace Context.
    pub fn parse(s: &str) -> Result<Traceparent> {
        let parts: Vec<&str> = s.trim().split('-').collect();
        let hex = |p: &str, n: usize| {
            p.len() == n
                && p.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        match parts.as_slice() {
            [v, t, p, f, ..] if hex(v, 2) && hex(t, 32) && hex(p, 16) && hex(f, 2) => {
                if *v == "ff" {
                    bail!("version ff is invalid");
                }
                if *v == "00" && parts.len() != 4 {
                    bail!("version 00 has exactly four fields");
                }
                if t.bytes().all(|b| b == b'0') || p.bytes().all(|b| b == b'0') {
                    bail!("all-zero trace or span id");
                }
                Ok(Traceparent {
                    version: v.to_string(),
                    trace_id: t.to_string(),
                    span_id: p.to_string(),
                    flags: u8::from_str_radix(f, 16)?,
                })
            }
            _ => bail!("expected 00-<32 lowercase hex>-<16 lowercase hex>-<2 hex>"),
        }
    }
}

impl fmt::Display for Traceparent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{}-{}-{:02x}",
            self.version, self.trace_id, self.span_id, self.flags
        )
    }
}

static SEQ: AtomicU64 = AtomicU64::new(1);

/// 64 random bits, never zero. `RandomState` is seeded from OS randomness; good
/// enough for trace ids, not for secrets.
fn random_u64() -> u64 {
    loop {
        let mut h = std::hash::RandomState::new().build_hasher();
        h.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
        let v = h.finish();
        if v != 0 {
            return v;
        }
    }
}

pub(crate) fn new_trace_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_secs() as u32);
    format!("{secs:08x}{:08x}{:016x}", random_u64() as u32, random_u64())
}

pub(crate) fn new_span_id() -> String {
    format!("{:016x}", random_u64())
}

impl TraceContext {
    pub(crate) fn fresh(
        cfg: &TracingConfig,
        parent: Option<&(Traceparent, Option<String>)>,
    ) -> TraceContext {
        match parent {
            Some((p, state)) => TraceContext {
                trace_id: p.trace_id.clone(),
                sampled: p.sampled(),
                tracestate: state.clone().or_else(|| cfg.tracestate.clone()),
                parent_span_id: Some(p.span_id.clone()),
            },
            None => TraceContext {
                trace_id: new_trace_id(),
                sampled: cfg.sampled,
                tracestate: cfg.tracestate.clone(),
                parent_span_id: None,
            },
        }
    }

    pub(crate) fn span(&self) -> SpanContext {
        SpanContext {
            trace_id: self.trace_id.clone(),
            span_id: new_span_id(),
            sampled: self.sampled,
        }
    }
}

fn baggage_escape(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                o.push(b as char);
            }
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// The headers to send for one span.
pub(crate) fn headers(
    span: &SpanContext,
    trace: &TraceContext,
    cfg: &TracingConfig,
    baggage: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = vec![];
    let s01 = if span.sampled { "1" } else { "0" };
    let trace_hi = &span.trace_id[..16];
    let trace_lo = &span.trace_id[16..];
    let dec = |hex: &str| u64::from_str_radix(hex, 16).unwrap_or(0).to_string();
    for f in &cfg.formats {
        match f {
            TraceFormat::W3c => {
                out.push(("traceparent".into(), span.traceparent()));
                if let Some(ts) = &trace.tracestate {
                    out.push(("tracestate".into(), ts.clone()));
                }
                if !baggage.is_empty() {
                    let v: Vec<String> = baggage
                        .iter()
                        .map(|(k, v)| format!("{}={}", baggage_escape(k), baggage_escape(v)))
                        .collect();
                    out.push(("baggage".into(), v.join(",")));
                }
            }
            TraceFormat::B3 => out.push((
                "b3".into(),
                format!("{}-{}-{s01}", span.trace_id, span.span_id),
            )),
            TraceFormat::B3multi => {
                out.push(("X-B3-TraceId".into(), span.trace_id.clone()));
                out.push(("X-B3-SpanId".into(), span.span_id.clone()));
                out.push(("X-B3-Sampled".into(), s01.into()));
            }
            TraceFormat::Xray => out.push((
                "X-Amzn-Trace-Id".into(),
                format!(
                    "Root=1-{}-{};Parent={};Sampled={s01}",
                    &span.trace_id[..8],
                    &span.trace_id[8..],
                    span.span_id
                ),
            )),
            TraceFormat::Gcp => out.push((
                "X-Cloud-Trace-Context".into(),
                format!("{}/{};o={s01}", span.trace_id, dec(&span.span_id)),
            )),
            TraceFormat::Datadog => {
                out.push(("x-datadog-trace-id".into(), dec(trace_lo)));
                out.push(("x-datadog-parent-id".into(), dec(&span.span_id)));
                out.push(("x-datadog-sampling-priority".into(), s01.into()));
                out.push(("x-datadog-tags".into(), format!("_dd.p.tid={trace_hi}")));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> (SpanContext, TraceContext) {
        let t = TraceContext {
            trace_id: "6750c1f2a1b2c3d4e5f60718293a4b5c".into(),
            sampled: true,
            tracestate: Some("apitest=1".into()),
            parent_span_id: None,
        };
        let s = SpanContext {
            trace_id: t.trace_id.clone(),
            span_id: "00f067aa0ba902b7".into(),
            sampled: true,
        };
        (s, t)
    }

    #[test]
    fn all_formats() {
        let (s, t) = span();
        let cfg = TracingConfig::default().formats([
            TraceFormat::W3c,
            TraceFormat::B3,
            TraceFormat::B3multi,
            TraceFormat::Xray,
            TraceFormat::Gcp,
            TraceFormat::Datadog,
        ]);
        let baggage = BTreeMap::from([(
            "apitest.test".to_string(),
            "billing::create invoice".to_string(),
        )]);
        let h: BTreeMap<String, String> = headers(&s, &t, &cfg, &baggage).into_iter().collect();
        assert_eq!(
            h["traceparent"],
            "00-6750c1f2a1b2c3d4e5f60718293a4b5c-00f067aa0ba902b7-01"
        );
        assert_eq!(h["tracestate"], "apitest=1");
        assert_eq!(h["baggage"], "apitest.test=billing::create%20invoice");
        assert_eq!(
            h["b3"],
            "6750c1f2a1b2c3d4e5f60718293a4b5c-00f067aa0ba902b7-1"
        );
        assert_eq!(h["X-B3-TraceId"], "6750c1f2a1b2c3d4e5f60718293a4b5c");
        assert_eq!(
            h["X-Amzn-Trace-Id"],
            "Root=1-6750c1f2-a1b2c3d4e5f60718293a4b5c;Parent=00f067aa0ba902b7;Sampled=1"
        );
        assert_eq!(
            h["X-Cloud-Trace-Context"],
            "6750c1f2a1b2c3d4e5f60718293a4b5c/67667974448284343;o=1"
        );
        assert_eq!(
            h["x-datadog-trace-id"],
            u64::from_str_radix("e5f60718293a4b5c", 16)
                .unwrap()
                .to_string()
        );
        assert_eq!(h["x-datadog-tags"], "_dd.p.tid=6750c1f2a1b2c3d4");
    }

    #[test]
    fn traceparent_parsing() {
        let p =
            Traceparent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert!(p.sampled());
        assert_eq!(
            p.to_string(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
        for bad in [
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "garbage",
        ] {
            assert!(Traceparent::parse(bad).is_err(), "{bad}");
        }
        // future versions may carry more fields
        assert!(
            Traceparent::parse("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-xyz")
                .is_ok()
        );
    }

    #[test]
    fn generated_ids_are_valid() {
        let t = TraceContext::fresh(&TracingConfig::default(), None);
        let s = t.span();
        let p = Traceparent::parse(&s.traceparent()).unwrap();
        assert_eq!(p.trace_id, t.trace_id);
        assert_ne!(t.span().span_id, s.span_id);
        assert_ne!(new_trace_id(), new_trace_id());
    }

    #[test]
    fn inherits_parent() {
        let cfg = TracingConfig {
            parent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00".into()),
            ..Default::default()
        };
        let parent = cfg.resolved_parent().unwrap();
        let t = TraceContext::fresh(&cfg, parent.as_ref());
        assert_eq!(t.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(t.parent_span_id.as_deref(), Some("00f067aa0ba902b7"));
        assert!(!t.sampled);
    }
}
