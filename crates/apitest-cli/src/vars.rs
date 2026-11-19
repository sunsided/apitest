//! Variable and redaction flags (`--var`, `--env-file`, `--no-env-file`,
//! `--redact`), shared by every subcommand that sends requests.
//!
//! ```text
//! apitest run --env-file .env.ci --var API_USER=demo
//! apitest call hub GET /me --as hub_admin --redact 'cust-[0-9]+'
//! ```

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Args;

use apitest::config::ProjectConfig;
use apitest::dotenv::EnvFile;
use apitest::runner::RunConfig;

/// Where variables come from and what to mask in output.
#[derive(Args, Clone)]
// clap names the group after the struct; `drift::Flags` already has that name.
#[group(id = "variables")]
pub(crate) struct Flags {
    /// Variable for `cx.env()` and `${NAME}` in the config, e.g. --var `API_USER=demo`
    /// (repeatable). Wins over the environment, .env files and `vars:`.
    #[arg(long = "var", value_name = "KEY=VALUE", value_parser = crate::parse_kv)]
    vars: Vec<(String, String)>,
    /// .env file to read instead of the project's (repeatable; later files win).
    /// Default: `env_files:` from the config, else .env and .env.local next to it.
    #[arg(long = "env-file", value_name = "PATH", conflicts_with = "no_env_file")]
    env_files: Vec<PathBuf>,
    /// Read no .env files.
    #[arg(long)]
    no_env_file: bool,
    /// Regular expression to mask as *** in output, errors and reports (repeatable).
    #[arg(long = "redact", value_name = "REGEX")]
    redact: Vec<String>,
}

impl Flags {
    /// Layers the flags over `cfg`: adds the `--redact` patterns and `--var`
    /// variables, and reads the `.env` files.
    ///
    /// # Errors
    ///
    /// Fails on an invalid `--redact` pattern, or a `.env` file that is missing
    /// (when required), unreadable or malformed.
    pub(crate) fn apply(&self, cfg: &mut RunConfig, project: Option<&ProjectConfig>) -> Result<()> {
        for re in &self.redact {
            cfg.redactor
                .pattern(re)
                .context("invalid --redact pattern")?;
        }

        cfg.vars.extend(self.vars.iter().cloned());
        for file in self.env_files(project) {
            cfg.load_env_file(&file)?;
        }

        Ok(())
    }

    /// The `.env` files to read: none with `--no-env-file`, else the
    /// `--env-file`s (relative to the current directory, each required), else
    /// the project's (see [`ProjectConfig::env_files`]).
    fn env_files(&self, project: Option<&ProjectConfig>) -> Vec<EnvFile> {
        if self.no_env_file {
            return vec![];
        }

        if self.env_files.is_empty() {
            return project.map(ProjectConfig::env_files).unwrap_or_default();
        }

        self.env_files
            .iter()
            .map(|path| EnvFile {
                path: path.clone(),
                required: true,
            })
            .collect()
    }
}
