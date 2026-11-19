//! Property tests for the parsers and the drift diff.

use apitest::Contract;
use apitest::contract::{Severity, Spec};
use apitest::load::{Threshold, parse_quantity};
use apitest::trace::Traceparent;
use proptest::prelude::*;
use serde_json::{Map, Value, json};

/// Builds an `OpenAPI` document with one `GET` operation per entry, each returning an object
/// with the given property names (all `string`, all required).
fn openapi(ops: &[(String, Vec<String>)]) -> Value {
    let paths: Map<String, Value> = ops
        .iter()
        .map(|(id, props)| {
            let properties: Map<String, Value> = props
                .iter()
                .map(|p| (p.clone(), json!({ "type": "string" })))
                .collect();
            let schema = json!({ "type": "object", "properties": properties, "required": props });
            let response = json!({ "content": { "application/json": { "schema": schema } } });
            let op = json!({ "get": { "operationId": id, "responses": { "200": response } } });

            (format!("/{id}"), op)
        })
        .collect();

    json!({ "openapi": "3.0.3", "info": { "title": "t", "version": "1" }, "paths": paths })
}

/// Distinct operation ids, each with distinct property names.
fn operations() -> impl Strategy<Value = Vec<(String, Vec<String>)>> {
    prop::collection::btree_map(
        "[a-z][a-zA-Z0-9]{0,8}",
        prop::collection::btree_set("[a-z][a-z_]{0,8}", 0..6),
        1..6,
    )
    .prop_map(|m| {
        m.into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect()
    })
}

proptest! {
    /// A spec never drifts from itself, with or without recorded usage.
    #[test]
    fn spec_has_no_drift_against_itself(ops in operations()) {
        let doc = openapi(&ops);
        let contract = Contract::from_spec("c", Spec::from_value(&doc, String::new()).unwrap());
        let live = Spec::from_value(&doc, String::new()).unwrap();
        let usage = contract.usage();

        prop_assert!(contract.diff(&live, None, "self").findings.is_empty());
        prop_assert!(contract.diff(&live, Some(&usage), "self").findings.is_empty());
    }

    /// Dropping a property the client receives as required is always reported as breaking.
    #[test]
    fn removed_response_property_is_breaking(ops in operations(), pick in any::<prop::sample::Index>()) {
        let candidates: Vec<(usize, usize)> = ops
            .iter()
            .enumerate()
            .flat_map(|(i, (_, props))| (0..props.len()).map(move |j| (i, j)))
            .collect();
        prop_assume!(!candidates.is_empty());
        let (i, j) = *pick.get(&candidates);
        let mut live_ops = ops.clone();
        live_ops[i].1.remove(j);

        let contract = Contract::from_spec("c", Spec::from_value(&openapi(&ops), String::new()).unwrap());
        let live = Spec::from_value(&openapi(&live_ops), String::new()).unwrap();
        let report = contract.diff(&live, None, "live");

        prop_assert!(report.is_breaking(), "{:?}", report.findings);
        prop_assert!(report.findings.iter().any(|f| f.severity == Severity::Breaking && f.operation == ops[i].0));
    }

    /// JSON and YAML encodings of the same document load to the same operations.
    #[test]
    fn json_and_yaml_load_the_same_operations(ops in operations()) {
        let doc = openapi(&ops);
        let from_json = Spec::from_bytes(serde_json::to_string(&doc).unwrap().as_bytes(), Some("json")).unwrap();
        let from_yaml = Spec::from_bytes(serde_saphyr::to_string(&doc).unwrap().as_bytes(), Some("yaml")).unwrap();

        prop_assert_eq!(
            from_json.operations.keys().collect::<Vec<_>>(),
            from_yaml.operations.keys().collect::<Vec<_>>()
        );
    }

    /// A valid `traceparent` survives a print/parse round trip.
    #[test]
    fn traceparent_round_trips(
        trace_id in "[0-9a-f]{31}[1-9a-f]",
        span_id in "[0-9a-f]{15}[1-9a-f]",
        flags in any::<u8>(),
    ) {
        let tp = Traceparent { version: "00".into(), trace_id, span_id, flags };

        prop_assert_eq!(Traceparent::parse(&tp.to_string()).unwrap(), tp);
    }

    /// Quantities in every unit parse to milliseconds (or a plain ratio).
    #[test]
    #[expect(clippy::float_cmp, reason = "the parser must produce exactly these values, not approximately")]
    fn quantities_parse_in_every_unit(n in 0u32..1_000_000) {
        let v = f64::from(n);

        prop_assert_eq!(parse_quantity(&format!("{n}ms")).unwrap(), v);
        prop_assert_eq!(parse_quantity(&format!("{n}s")).unwrap(), v * 1000.0);
        prop_assert_eq!(parse_quantity(&format!("{n}m")).unwrap(), v * 60_000.0);
        prop_assert_eq!(parse_quantity(&format!("{n}%")).unwrap(), v / 100.0);
        prop_assert_eq!(parse_quantity(&n.to_string()).unwrap(), v);
    }

    /// Every comparison operator is accepted, for both metric groups.
    #[test]
    fn thresholds_accept_every_operator(
        op in prop::sample::select(vec!["<", "<=", ">", ">=", "=="]),
        group in prop::sample::select(vec!["http", "iterations"]),
        n in 0u32..10_000,
    ) {
        let expr = format!("{group}.p95 {op} {n}ms");

        prop_assert!(Threshold::parse(&expr).is_ok(), "{expr}");
    }
}
