//! Drift-target flags (`--against`, `--against-spec`) and the typo guard that
//! checks the selected targets against every contract of the project.
//!
//! ```text
//! apitest contract check --against qa,next        # specs from apitest.nova, no service needed
//! apitest run --against live,qa
//! apitest contract check --against-spec billing=../billing/openapi.yaml
//! ```

use anyhow::{Result, bail};
use clap::Args;

use apitest::config::ProjectConfig;
use apitest::contract::{DriftTarget, LIVE, SpecSource};
use apitest::runner::RunConfig;
use apitest::script::{LoadOptions, load_suites};
use apitest::suite::Suite;

/// Name of the drift target `--against-spec` builds.
const CLI_TARGET: &str = "cli";

/// What the drift check compares the pinned specs against.
#[derive(Args, Clone)]
pub(crate) struct Flags {
    /// What to diff the pinned specs against (repeatable or comma-separated):
    /// `live`, a target from `drift.targets` in apitest.nova, or `all`.
    /// Replaces `drift.against` from the config. Default: live.
    #[arg(long, value_delimiter = ',', value_name = "TARGET")]
    against: Vec<String>,
    /// One-off drift target, `CONTRACT=FILE_OR_URL` (repeatable). The specs given
    /// form the target `cli`. Without `--against`, only `cli` is checked (this
    /// replaces `drift.against` from the config); use `--against cli,live,...`
    /// to combine it with other targets.
    #[arg(long = "against-spec", value_parser = crate::parse_kv, value_name = "CONTRACT=SPEC")]
    against_spec: Vec<(String, String)>,
}

impl Flags {
    /// Layers the flags over `cfg` and validates the resulting selection.
    ///
    /// # Errors
    ///
    /// Fails if `apitest.nova` already defines a target named `cli` while
    /// `--against-spec` is used, or the selection names an unknown target.
    pub(crate) fn apply(&self, cfg: &mut RunConfig) -> Result<()> {
        if !self.against_spec.is_empty() {
            if cfg.drift_targets.contains_key(CLI_TARGET) {
                bail!(
                    "--against-spec uses the target name '{CLI_TARGET}', which apitest.nova already defines"
                );
            }

            let cwd = std::env::current_dir()?;
            let target = self
                .against_spec
                .iter()
                .fold(DriftTarget::new(), |t, (c, s)| {
                    t.spec(c.clone(), SpecSource::parse(s, &cwd))
                });
            cfg.drift_targets.insert(CLI_TARGET.into(), target);
            cfg.drift_against = vec![CLI_TARGET.into()];
        }

        if !self.against.is_empty() {
            cfg.drift_against.clone_from(&self.against);
        }

        cfg.selected_drift_targets()?;
        Ok(())
    }
}

/// Fails if a selected named target has a spec for a contract no suite has,
/// which is almost always a typo that would silently skip the check.
///
/// The contracts are those of `loaded` plus, with `project`, those of every
/// suite listed in it, since a target may cover contracts this run does not
/// load. Project suites that fail to load are named only if the check fails.
/// Nothing is checked when only `live` is selected.
///
/// # Errors
///
/// Fails if the selection is invalid or names an unknown contract.
pub(crate) fn check_targets(
    cfg: &RunConfig,
    loaded: &[Suite],
    project: Option<&ProjectConfig>,
) -> Result<()> {
    if cfg.selected_drift_targets()?.iter().all(|t| *t == LIVE) {
        return Ok(());
    }

    let mut names: Vec<String> = contract_names(loaded);
    let mut unloadable = vec![];
    let project_files = project.map_or_else(Vec::new, |p| {
        p.suites.iter().map(|s| p.resolve(s)).collect::<Vec<_>>()
    });
    for file in project_files {
        match load_suites(&file, &LoadOptions::default()) {
            Ok(suites) => names.extend(contract_names(&suites)),
            Err(e) => unloadable.push(format!("{}: {e:#}", file.display())),
        }
    }

    cfg.check_drift_targets(names.iter().map(String::as_str))
        .map_err(|e| {
            if unloadable.is_empty() {
                e
            } else {
                e.context(format!(
                    "these project suites could not be loaded to look up their contracts: {}",
                    unloadable.join("; ")
                ))
            }
        })
}

/// The names of all contracts of `suites`.
fn contract_names(suites: &[Suite]) -> Vec<String> {
    suites
        .iter()
        .flat_map(|s| s.contracts.iter().map(|c| c.name.clone()))
        .collect()
}
