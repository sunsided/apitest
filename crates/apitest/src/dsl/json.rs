//! Partial JSON matching: [`json_includes`] and the [`assert_json!`](crate::assert_json) macro.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use crate::http::Response;

/// Partial match: every field in `expected` must be present in `actual` with an
/// equal (recursively partially matching) value. Arrays must have the same length
/// and match element-wise. Extra fields in `actual` are ignored. Numbers compare
/// by value, so `1` matches `1.0`.
///
/// # Errors
///
/// Fails on the first mismatch and names its JSON pointer path:
/// `at /therapy/plan: expected "CBT", got "DBT"`.
pub fn json_includes(actual: &Value, expected: &Value) -> Result<()> {
    walk(actual, expected, &mut String::new())
}

/// Matches `a` against `e`; `path` is the JSON pointer of both, restored on success.
fn walk(a: &Value, e: &Value, path: &mut String) -> Result<()> {
    match (a, e) {
        (Value::Object(ao), Value::Object(eo)) => walk_object(ao, eo, path),
        (Value::Array(aa), Value::Array(ea)) => walk_array(aa, ea, path),
        (Value::Number(an), Value::Number(en)) if an.as_f64() == en.as_f64() => Ok(()),
        _ if a == e => Ok(()),
        _ => bail!("at {}: expected {e}, got {a}", show(path)),
    }
}

/// Every key of `eo` must exist in `ao` and match recursively.
fn walk_object(ao: &Map<String, Value>, eo: &Map<String, Value>, path: &mut String) -> Result<()> {
    for (k, ev) in eo {
        let len = path.len();
        path.push('/');
        path.push_str(k);
        match ao.get(k) {
            Some(av) => walk(av, ev, path)?,
            None => bail!("at {path}: missing (expected {ev})"),
        }

        path.truncate(len);
    }

    Ok(())
}

/// Both arrays must have the same length and match element-wise.
fn walk_array(aa: &[Value], ea: &[Value], path: &mut String) -> Result<()> {
    if aa.len() != ea.len() {
        bail!(
            "at {}: expected {} elements, got {}",
            show(path),
            ea.len(),
            aa.len()
        );
    }

    for (i, (av, ev)) in aa.iter().zip(ea).enumerate() {
        let len = path.len();
        path.push('/');
        path.push_str(&i.to_string());
        walk(av, ev, path)?;
        path.truncate(len);
    }

    Ok(())
}

/// The path for error messages; the empty pointer reads as `<root>`.
fn show(p: &str) -> &str {
    if p.is_empty() { "<root>" } else { p }
}

/// `assert_json!(value, { "status": "in_progress", "therapy": { "plan": "CBT" } })`:
/// returns early with an error unless `value` partially matches (see [`json_includes`]).
/// `value` can be a `serde_json::Value` or a [`Response`] (its body is used).
#[macro_export]
macro_rules! assert_json {
    ($actual:expr, $($expected:tt)+) => {
        $crate::dsl::json_includes(
            $crate::dsl::AsJson::as_json(&$actual),
            &$crate::__serde_json::json!($($expected)+),
        )?
    };
}

/// Things [`assert_json!`](crate::assert_json) accepts.
pub trait AsJson {
    /// The JSON value to match against.
    fn as_json(&self) -> &Value;
}

impl AsJson for Value {
    fn as_json(&self) -> &Value {
        self
    }
}

impl AsJson for Response {
    /// The parsed response body.
    fn as_json(&self) -> &Value {
        &self.body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_json_matching() {
        let actual = json!({ "id": 1, "status": "in_progress", "therapy": { "plan": "CBT", "sessions": 2 }, "tags": ["a", "b"] });
        json_includes(
            &actual,
            &json!({ "status": "in_progress", "therapy": { "plan": "CBT" } }),
        )
        .unwrap();
        json_includes(&actual, &json!({ "tags": ["a", "b"] })).unwrap();
        let e = json_includes(&actual, &json!({ "therapy": { "plan": "DBT" } }))
            .unwrap_err()
            .to_string();
        assert_eq!(e, r#"at /therapy/plan: expected "DBT", got "CBT""#);
        let e = json_includes(&actual, &json!({ "owner": "x" }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("at /owner: missing"), "{e}");
        let e = json_includes(&actual, &json!({ "tags": ["a"] }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("expected 1 elements, got 2"), "{e}");
    }
}
