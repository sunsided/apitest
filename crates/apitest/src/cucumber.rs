//! Gherkin frontend (feature `cucumber`): `.feature` files as suites.
//!
//! A feature file is compiled into an ordinary [`Suite`], the same way a `.rhai`
//! or `.js` file is: one suite per feature, one test per scenario (and per row
//! of a `Scenario Outline`'s examples). Backgrounds become the leading steps of
//! every scenario, and a `Rule:` prefixes the names of its scenarios. Each
//! Gherkin step runs as a [`crate::dsl::step`], so it shows up in the step trail
//! of failures, in `JUnit` `system-out` and in the JSON report, with the requests
//! it sends nested inside it.
//!
//! Everything else is the existing machinery: the runner's scheduler, fixtures
//! and cleanups, contract pinning and the drift check, `apitest.nova`, `.env`
//! files and redaction, and the reports.
//!
//! ```gherkin
//! @contract:billing=../specs/billing-1.0.yaml @live_spec:billing=/openapi.json
//! Feature: Invoices
//!
//!   Background:
//!     Given "user" logs in on "billing" as "${API_USER}" with password "${API_PASS}"
//!
//!   @parallel
//!   Scenario: create and fetch an invoice
//!     When "user" calls "createInvoice" expecting 201 with the JSON body:
//!       """json
//!       { "amount": 4200, "currency": "EUR" }
//!       """
//!     And afterwards "user" calls "deleteInvoice" expecting 204 with:
//!       | path | id | {{response:/id}} |
//! ```
//!
//! Step definitions are matched by regular expression against the step text,
//! regardless of the keyword (`Given`/`When`/`Then`/`And`/`But`), as Cucumber
//! does. [`Steps::builtin`] is the generic library documented in
//! `docs/design/cucumber.md`; Rust users add their own with [`Steps::step`] and
//! [`load_with`].
//!
//! Every step of every scenario is matched when the file is loaded, so
//! undefined and ambiguous steps fail the load (all of them at once, with their
//! lines), before anything is sent.
//!
//! ```mermaid
//! flowchart LR
//!     file[.feature] --> parse[gherkin parser]
//!     parse --> tags[feature tags: contracts]
//!     parse --> expand[scenarios and outline rows]
//!     expand --> filter{--tags}
//!     filter --> plan[match steps against Steps]
//!     plan --> suite[Suite with one test each]
//! ```

mod builtin;
mod plan;
mod steps;
mod tags;
mod world;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use gherkin::{Feature, GherkinEnv, Scenario, Step};

use crate::context::Ctx;
use crate::suite::{BoxFut, CtxFn, Suite};
use plan::{PlannedStep, Within};

pub use steps::{StepArgs, StepFn, Steps};
pub use tags::{MAX_TAG_EXPRESSION_LEN, TagFilter};
pub use world::World;

/// Options for loading feature files.
#[derive(Debug, Clone, Default)]
pub struct FeatureOptions {
    /// Cucumber tag expression, e.g. `@smoke and not @slow`. Only matching
    /// scenarios become tests. Tags are inherited from the feature and rule.
    pub tags: Option<String>,
}

/// Loads a `.feature` file with the built-in step library.
///
/// # Errors
///
/// See [`load_with`].
pub fn load(path: &Path, opts: &FeatureOptions) -> Result<Vec<Suite>> {
    load_with(path, opts, Arc::new(Steps::builtin()))
}

/// Loads a `.feature` file with a custom step library (usually
/// `Steps::builtin().step(..)...`).
///
/// # Errors
///
/// Fails when the file cannot be read or parsed, the tag expression is
/// invalid, a feature tag is malformed or its contract cannot be loaded, or a
/// step has no unique definition.
pub fn load_with(path: &Path, opts: &FeatureOptions, steps: Arc<Steps>) -> Result<Vec<Suite>> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(vec![load_str(&source, path, opts, steps)?])
}

/// Compiles feature source text. `path` names the file in messages, and
/// relative `@contract:` paths resolve against its directory.
///
/// # Errors
///
/// Like [`load_with`], except for reading the file.
pub fn load_str(
    source: &str,
    path: &Path,
    opts: &FeatureOptions,
    steps: Arc<Steps>,
) -> Result<Suite> {
    let filter = opts.tags.as_deref().map(TagFilter::parse).transpose()?;
    let feature = Feature::parse(source, GherkinEnv::default())
        .map_err(|e| anyhow!("{e}"))
        .with_context(|| format!("cannot parse {}", path.display()))?;
    Compiler {
        file: path.display().to_string(),
        filter,
        steps,
        problems: vec![],
    }
    .compile(&feature, path.parent().unwrap_or(Path::new(".")))
}

/// Turns one parsed feature into a suite.
struct Compiler {
    /// The file name for messages and step locations.
    file: String,
    /// Scenarios that don't match are left out.
    filter: Option<TagFilter>,
    /// The step library.
    steps: Arc<Steps>,
    /// Steps without a unique definition, as `file:line: message`.
    problems: Vec<String>,
}

impl Compiler {
    /// The suite of `feature`; relative contract paths resolve against `base`.
    fn compile(mut self, feature: &Feature, base: &Path) -> Result<Suite> {
        let mut suite = Suite::new(feature.name.trim());
        for c in tags::contracts(&feature.tags, base).with_context(|| self.file.clone())? {
            suite.add_contract(c);
        }

        let background: Vec<&Step> = feature.background.iter().flat_map(|b| &b.steps).collect();
        for sc in &feature.scenarios {
            self.scenario(&mut suite, None, &background, sc, &feature.tags)?;
        }

        for rule in &feature.rules {
            let bg: Vec<&Step> = background
                .iter()
                .copied()
                .chain(rule.background.iter().flat_map(|b| &b.steps))
                .collect();
            let tags: Vec<String> = feature.tags.iter().chain(&rule.tags).cloned().collect();
            for sc in &rule.scenarios {
                self.scenario(&mut suite, Some(rule.name.trim()), &bg, sc, &tags)?;
            }
        }

        if !self.problems.is_empty() {
            bail!(
                "{} step(s) in {} have no unique definition:\n  {}",
                self.problems.len(),
                self.file,
                self.problems.join("\n  ")
            );
        }

        Ok(suite)
    }

    /// Adds the tests of one scenario (one per outline row) that the tag
    /// filter selects.
    fn scenario(
        &mut self,
        suite: &mut Suite,
        rule: Option<&str>,
        background: &[&Step],
        sc: &Scenario,
        inherited: &[String],
    ) -> Result<()> {
        for inst in plan::expand(sc)? {
            let tags: Vec<String> = inherited
                .iter()
                .chain(&sc.tags)
                .chain(&inst.tags)
                .cloned()
                .collect();
            if self.filter.as_ref().is_some_and(|f| !f.matches(&tags)) {
                continue;
            }

            let name = match rule {
                Some(r) => format!("{r}: {}", inst.name),
                None => inst.name,
            };
            let steps = background.iter().copied().chain(&sc.steps);
            let planned = plan::plan(
                steps,
                &inst.values,
                &self.file,
                &self.steps,
                &mut self.problems,
            );
            let opts = tags::test_opts(&tags)
                .with_context(|| format!("{}: scenario '{name}'", self.file))?;
            suite.add_test(&name, opts, scenario_body(planned, self.steps.clone()));
        }

        Ok(())
    }
}

/// The test body of one scenario: a fresh [`World`], then each step in order
/// until one fails.
fn scenario_body(planned: Vec<PlannedStep>, steps: Arc<Steps>) -> CtxFn<()> {
    let planned = Arc::new(planned);
    Arc::new(move |cx: Ctx| -> BoxFut<()> {
        let planned = planned.clone();
        let steps = steps.clone();
        Box::pin(async move {
            let world = World::new(cx);
            for (i, s) in planned.iter().enumerate() {
                if let Err(e) = run_planned(&world, s, &steps).await {
                    let skipped = planned.len() - i - 1;
                    if skipped > 0 {
                        world
                            .cx()
                            .log(format!("{skipped} remaining step(s) skipped"));
                    }

                    return Err(e);
                }
            }

            Ok(())
        })
    })
}

/// Runs one planned step as a reported [`crate::dsl::step`], its location
/// added to the error.
async fn run_planned(world: &World, s: &PlannedStep, steps: &Steps) -> Result<()> {
    let f = steps.def(s.def).clone();
    let location = s.location.clone();
    let (w, args, within) = (world.clone(), s.args.clone(), s.within);
    crate::dsl::step(s.label.clone(), async move {
        run_step(&f, &w, &args, within)
            .await
            .with_context(|| format!("at {location}"))
    })
    .await
}

/// Runs a step once, or with `within` until it passes or the time is up.
async fn run_step(f: &StepFn, w: &World, args: &StepArgs, within: Option<Within>) -> Result<()> {
    let Some(within) = within else {
        return f(w.clone(), args.clone()).await;
    };

    let mut retry = crate::dsl::Retry::new(within.timeout);
    let mut first = true;
    loop {
        if within.resend && !first {
            // A failed re-send is a failed attempt like a failed check.
            if let Err(e) = builtin::resend(w).await {
                retry.wait(e).await?;
                continue;
            }
        }

        first = false;
        match f(w.clone(), args.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => retry.wait(e).await?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compiles `src` with the built-in steps and no tag filter.
    fn compile(src: &str) -> Result<Suite> {
        load_str(
            src,
            Path::new("x.feature"),
            &FeatureOptions::default(),
            Arc::new(Steps::builtin()),
        )
    }

    #[test]
    fn undefined_and_ambiguous_steps_fail_the_load() {
        let src = r#"
Feature: f
  Scenario: s
    Given "a" is an anonymous actor on "svc"
    When "a" frobnicates
    Then the status is 200
    And "a" juggles
"#;
        let e = compile(src).err().unwrap().to_string();
        assert!(e.contains("2 step(s) in x.feature"), "{e}");
        assert!(
            e.contains("x.feature:5: undefined step: \"a\" frobnicates"),
            "{e}"
        );
        assert!(e.contains("x.feature:7"), "{e}");

        let steps = Steps::builtin()
            .step(r"the status is (\d+)", |_w, _a| async { Ok(()) })
            .unwrap();
        let e = load_str(
            src,
            Path::new("x.feature"),
            &FeatureOptions::default(),
            Arc::new(steps),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(e.contains("x.feature:6: ambiguous step"), "{e}");
    }

    #[test]
    fn outlines_expand_per_example_row() {
        let src = r#"
Feature: f
  Background:
    Given "a" is an anonymous actor on "svc"

  Rule: lookups
    Scenario Outline: get <id>
      When "a" sends GET "/things/<id>"
      Then the status is <status>

      @fast
      Examples:
        | id | status |
        | 1  | 200    |
        | 9  | 404    |
"#;
        let s = compile(src).unwrap();
        let names: Vec<&str> = s.tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "lookups: get 1 (id=1, status=200)",
                "lookups: get 9 (id=9, status=404)"
            ]
        );
        assert_eq!(s.tests[0].opts.tags, vec!["fast"]);
    }

    #[test]
    fn tag_filter_selects_scenarios() {
        let src = "
@svc
Feature: f
  @smoke
  Scenario: a
    Given \"a\" is an anonymous actor on \"svc\"

  @slow
  Scenario: b
    Given \"a\" is an anonymous actor on \"svc\"
";
        let opts = FeatureOptions {
            tags: Some("@svc and not @slow".into()),
        };
        let s = load_str(
            src,
            Path::new("x.feature"),
            &opts,
            Arc::new(Steps::builtin()),
        )
        .unwrap();
        let names: Vec<&str> = s.tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["a"]);
    }
}
