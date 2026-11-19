//! Property tests for the parsers, the drift diff, `.env` parsing, redaction and
//! Gherkin tag expressions.

use std::collections::BTreeMap;

use apitest::Contract;
use apitest::Redactor;
use apitest::RunConfig;
use apitest::contract::{ALL, DriftTarget, LIVE, Severity, Spec, SpecSource};
use apitest::load::{Threshold, parse_duration, parse_quantity};
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

    /// Durations never panic: negative, non-finite and huge values are errors.
    #[test]
    fn durations_never_panic(s in "-?[0-9.e+-]{0,8}(ms|s|m|inf|NaN)?") {
        if let Ok(d) = parse_duration(&s) {
            prop_assert!(d.as_secs_f64().is_finite());
        }
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

    /// A spec reference never panics: an `http(s)://` prefix in any case makes a URL,
    /// kept as given apart from surrounding whitespace; anything else is a file under
    /// the root (or absolute).
    #[test]
    fn spec_sources_are_urls_or_files(
        scheme in prop::sample::select(vec!["", "http://", "https://", "HTTPS://", "Http://", "ftp://", "http:/"]),
        rest in "\\PC{0,20}",
    ) {
        let raw = format!(" {scheme}{rest} ");
        let lower = raw.trim().to_ascii_lowercase();
        let is_url = lower.starts_with("http://") || lower.starts_with("https://");

        // An absolute base on every platform: "/root" has no drive letter on Windows.
        let base = std::env::temp_dir();

        match SpecSource::parse(&raw, &base) {
            SpecSource::Url(u) => {
                prop_assert!(is_url, "{raw:?} became URL {u:?}");
                prop_assert_eq!(u, raw.trim());
            }
            SpecSource::File(p) => {
                prop_assert!(!is_url, "{raw:?} became file {p:?}");
                let typed = std::path::Path::new(raw.trim());
                let expected = if typed.is_absolute() { typed.to_path_buf() } else { base.join(typed) };
                prop_assert_eq!(p, expected);
            }
            SpecSource::Document(_) => prop_assert!(false, "parsed into a document"),
        }
    }

    /// The selection never repeats a target, only names known targets, and with
    /// `all` covers `live` and every named target.
    #[test]
    fn drift_selection_is_unique_and_complete(
        names in prop::collection::btree_set("[a-z]{1,4}", 0..4),
        picks in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
    ) {
        let names: Vec<String> = names.into_iter().filter(|n| n != LIVE && n != ALL).collect();
        let pool: Vec<String> = names.iter().cloned().chain([LIVE.into(), ALL.into()]).collect();
        let against: Vec<String> = picks.iter().map(|i| i.get(&pool).clone()).collect();
        let cfg = names
            .iter()
            .fold(RunConfig::default(), |c, n| c.drift_target(n.clone(), DriftTarget::new()))
            .drift_against(against.clone());

        let selected = cfg.selected_drift_targets().unwrap();
        let unique: std::collections::BTreeSet<&str> = selected.iter().copied().collect();
        prop_assert_eq!(unique.len(), selected.len());
        prop_assert!(selected.iter().all(|t| *t == LIVE || names.iter().any(|n| n == t)));
        if against.iter().any(|a| a == ALL) {
            prop_assert_eq!(selected.len(), names.len() + 1);
        }
    }
}

/// Secrets over a tiny alphabet (with `*` and a multi-byte letter) and a text made of
/// whole secrets and filler, so occurrences overlap, nest and touch each other and the mask.
fn secrets_and_text() -> impl Strategy<Value = (Vec<String>, String)> {
    prop::collection::vec("[ab*é]{4,7}", 1..5).prop_flat_map(|secrets| {
        let piece = prop_oneof![prop::sample::select(secrets.clone()), "[ab*é ]{0,4}"];
        let text = prop::collection::vec(piece, 0..12).prop_map(|p| p.concat());
        (Just(secrets), text)
    })
}

/// A redactor that knows `secrets`.
fn redactor(secrets: &[String]) -> Redactor {
    let r = Redactor::default();
    for s in secrets {
        r.secret(s);
    }

    r
}

/// Malformed `.env` lines; `VALUE` stands for the value the parser chokes on.
const BROKEN_DOTENV: [&str; 5] = [
    "A='VALUE",
    "A=\"VALUE",
    "BAD KEY=VALUE",
    "A=VALUE 'x",
    "A=\"$${VALUE\"",
];

proptest! {
    /// No registered secret survives redaction, and redacting again changes nothing.
    #[test]
    fn redaction_never_leaks_a_registered_secret((secrets, text) in secrets_and_text()) {
        let r = redactor(&secrets);
        let out = r.redact(&text);
        for s in &secrets {
            prop_assert!(!out.contains(s.as_str()), "{s:?} in {out:?}");
        }

        prop_assert_eq!(r.redact(&out), out);
    }

    /// Literal patterns are masked wherever they occur, also next to secrets, and
    /// redaction stays idempotent.
    #[test]
    fn patterns_are_masked_and_redaction_is_idempotent(
        words in prop::collection::vec("[a-c]{2,4}", 1..4),
        secrets in prop::collection::vec("[a-c*]{4,6}", 0..3),
        text in "[a-c *]{0,40}",
    ) {
        let r = redactor(&secrets);
        for w in &words {
            r.pattern(w).unwrap();
        }

        let out = r.redact(&text);
        for w in &words {
            prop_assert!(!out.contains(w.as_str()), "{w:?} in {out:?}");
        }

        prop_assert_eq!(r.redact(&out), out);
    }

    /// Simple `KEY=value` lines parse to exactly those variables, later lines winning.
    #[test]
    fn dotenv_assignments_round_trip(
        lines in prop::collection::vec(("[A-Z][A-Z0-9_]{0,6}", "[a-zA-Z0-9_./:-]{0,12}"), 0..8),
    ) {
        let text: String = lines.iter().flat_map(|(k, v)| [k, "=", v, "\n"]).collect();
        let expected: BTreeMap<String, String> = lines.into_iter().collect();
        prop_assert_eq!(apitest::dotenv::parse(&text).unwrap(), expected);
    }

    /// Malformed `.env` text fails without quoting the value it choked on.
    #[test]
    fn dotenv_errors_never_quote_values(
        value in "zq[0-9]{6}",
        shape in prop::sample::select(BROKEN_DOTENV.to_vec()),
    ) {
        let text = format!("OK=1\n{}\n", shape.replace("VALUE", &value).replace("$$", "$"));
        if let Err(e) = apitest::dotenv::parse(&text) {
            prop_assert!(!format!("{e:#}").contains(&value), "{e:#}");
        }
    }

    /// Arbitrary text never makes the `.env` parser panic.
    #[test]
    fn dotenv_never_panics(text in any::<String>()) {
        let _ = apitest::dotenv::parse(&text);
    }
}

/// `--tags` expressions: user input, parsed by the `gherkin` crate.
#[cfg(feature = "cucumber")]
mod tag_expressions {
    use apitest::cucumber::{MAX_TAG_EXPRESSION_LEN, TagFilter};
    use proptest::prelude::*;

    /// The tags expressions draw from.
    const TAGS: [&str; 4] = ["a", "b", "c", "d"];

    /// A tag expression tree.
    #[derive(Debug, Clone)]
    enum Expr {
        /// `@TAGS[i]`.
        Tag(usize),
        /// `not x`.
        Not(Box<Expr>),
        /// `x and y`.
        And(Box<Expr>, Box<Expr>),
        /// `x or y`.
        Or(Box<Expr>, Box<Expr>),
    }

    impl Expr {
        /// Binding strength: `or` < `and` < `not` < a tag.
        fn precedence(&self) -> u8 {
            match self {
                Expr::Or(..) => 1,
                Expr::And(..) => 2,
                Expr::Not(_) => 3,
                Expr::Tag(_) => 4,
            }
        }

        /// The expression with parentheses only where precedence needs them.
        fn render(&self) -> String {
            match self {
                Expr::Tag(i) => format!("@{}", TAGS[*i]),
                Expr::Not(x) => format!("not {}", x.operand(3)),
                Expr::And(x, y) => format!("{} and {}", x.operand(2), y.operand(3)),
                Expr::Or(x, y) => format!("{} or {}", x.operand(1), y.operand(2)),
            }
        }

        /// Rendered as an operand that must bind at least as strongly as `min`.
        fn operand(&self, min: u8) -> String {
            if self.precedence() < min {
                format!("({})", self.render())
            } else {
                self.render()
            }
        }

        /// Whether a scenario with `tags` is selected.
        fn holds(&self, tags: &[&str]) -> bool {
            match self {
                Expr::Tag(i) => tags.contains(&TAGS[*i]),
                Expr::Not(x) => !x.holds(tags),
                Expr::And(x, y) => x.holds(tags) && y.holds(tags),
                Expr::Or(x, y) => x.holds(tags) || y.holds(tags),
            }
        }
    }

    /// Random expression trees up to a few levels deep.
    fn expr() -> impl Strategy<Value = Expr> {
        (0..TAGS.len())
            .prop_map(Expr::Tag)
            .prop_recursive(4, 24, 2, |inner| {
                prop_oneof![
                    inner.clone().prop_map(|x| Expr::Not(Box::new(x))),
                    (inner.clone(), inner.clone())
                        .prop_map(|(x, y)| Expr::And(Box::new(x), Box::new(y))),
                    (inner.clone(), inner).prop_map(|(x, y)| Expr::Or(Box::new(x), Box::new(y))),
                ]
            })
    }

    proptest! {
        /// A rendered expression parses and selects exactly what the tree selects,
        /// so precedence and `not` work as Cucumber documents them.
        #[test]
        fn expressions_select_like_their_tree(
            e in expr(),
            tags in prop::sample::subsequence(TAGS.to_vec(), 0..=TAGS.len()),
        ) {
            let text = e.render();
            let filter = TagFilter::parse(&text).unwrap();
            prop_assert_eq!(filter.matches(&tags), e.holds(&tags), "{}", text);
        }

        /// Arbitrary text never makes the parser or the matcher panic.
        #[test]
        fn tag_expressions_never_panic(
            text in any::<String>(),
            tags in prop::collection::vec("@?[a-z]{1,3}", 0..4),
        ) {
            if let Ok(filter) = TagFilter::parse(&text) {
                let _ = filter.matches(&tags);
            }
        }

        /// Nesting as deep as the length limit allows parses without exhausting the stack.
        #[test]
        fn nesting_up_to_the_limit_is_safe(open in prop::sample::select(vec!["(", "not ", "not ("])) {
            let close = if open.contains('(') { ")" } else { "" };
            let depth = (MAX_TAG_EXPRESSION_LEN - 2) / (open.len() + close.len());
            let text = format!("{}@a{}", open.repeat(depth), close.repeat(depth));
            prop_assert!(text.len() <= MAX_TAG_EXPRESSION_LEN);
            let filter = TagFilter::parse(&text).unwrap();
            let _ = filter.matches(&["a"]);
        }
    }
}
