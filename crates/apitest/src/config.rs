//! Project configuration (`apitest.nova`).
//!
//! ```text
//! base_url: http://127.0.0.1:8080
//! services:
//!   hub: http://127.0.0.1:8081
//!   app: http://127.0.0.1:8082
//! lockfile: apitest.lock.nova
//! max_parallel: 8
//! test_timeout: @duration("30s")
//! vars:                       # defaults; --var, the environment and .env files win
//!   API_USER: demo
//! env_files: [.env, .env.ci]  # default: .env and .env.local, if they exist
//! redact:                     # regexes masked in logs, errors and reports
//!   - "cust-[0-9]+"
//! suites:
//!   - suites/billing.rhai
//!   - suites/billing.js
//! user_agent: my-api-tests/1.0
//! tracing:
//!   formats: [w3c, xray]
//!   scope: test
//!   test_baggage: true
//! reports:
//!   junit: target/apitest/junit.xml
//!   json: target/apitest/report.json
//! drift:
//!   against: [live, prod]
//!   targets:
//!     prod:
//!       billing: specs/billing-prod.yaml
//!     dev:
//!       billing: https://billing.dev.example.com/openapi.json
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::runner::RunConfig;

/// The NOVA schema of `apitest.nova` (also shipped as `schemas/apitest-config.novs`).
pub const CONFIG_SCHEMA: &str = include_str!("../schemas/apitest-config.novs");

/// The NOVA schema of `apitest.lock.nova` (also shipped as `schemas/apitest-lock.novs`).
pub const LOCK_SCHEMA: &str = include_str!("../schemas/apitest-lock.novs");

/// Parses and resolves `text` against a schema, then deserializes it. Errors list
/// every schema issue with its path (`services.hub: expected string, got int`).
pub(crate) fn from_nova<T: serde::de::DeserializeOwned>(
    text: &str,
    schema_src: &str,
    what: &str,
) -> Result<T> {
    let schema = novafile_schema::parse_schema(schema_src)
        .map_err(|e| anyhow!("built-in schema is invalid: {e}"))?;
    novafile_serde::from_str_with_schema(text, &schema).map_err(|e| {
        let issues = e
            .to_string()
            .split("; ")
            .map(|i| format!("  - {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow!("{what} does not match its schema:\n{issues}")
    })
}

/// A `Duration` stored as a native NOVA duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NovaDuration(#[serde(with = "novafile_serde::with::duration")] pub Duration);

/// Where to write machine-readable reports (`reports:`). Relative paths
/// resolve against [`ProjectConfig::root`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Reports {
    /// `JUnit` XML report path.
    pub junit: Option<PathBuf>,
    /// JSON report path.
    pub json: Option<PathBuf>,
}

/// What the contract drift check compares the pins against (`drift:`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Drift {
    /// Targets checked by default (`live`, names from `targets`, `all`).
    /// Empty: `live` only. `--against` on the command line replaces it.
    pub against: Vec<String>,
    /// Named comparison specs: target → contract → file path (relative to
    /// [`ProjectConfig::root`]) or `http(s)://` URL. Never uses the reserved
    /// names `live` or `all` once parsed.
    pub targets: BTreeMap<String, BTreeMap<String, String>>,
}

/// The contents of `apitest.nova`. Every field is optional; unset fields keep
/// the [`RunConfig`] defaults when applied with [`ProjectConfig::apply`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// Base URL requests go to when no named service applies.
    pub base_url: Option<String>,
    /// Named services (`hub`, `app`, `mail`, ...) → base URL.
    pub services: BTreeMap<String, String>,
    /// Lockfile path, relative to [`ProjectConfig::root`] unless absolute.
    pub lockfile: Option<PathBuf>,
    /// Maximum number of tests running at the same time.
    pub max_parallel: Option<usize>,
    /// Time limit for one test.
    pub test_timeout: Option<NovaDuration>,
    /// Time limit for one fixture setup or teardown hook.
    pub hook_timeout: Option<NovaDuration>,
    /// Defaults for variables; `--var`, the environment and `.env` files win
    /// (see [`RunConfig::lookup`]).
    pub vars: BTreeMap<String, String>,
    /// `.env` files to read, relative to [`ProjectConfig::root`]; later files
    /// win. `None` reads [`DEFAULT_ENV_FILES`](crate::dotenv::DEFAULT_ENV_FILES)
    /// if they exist; listed files must exist.
    pub env_files: Option<Vec<PathBuf>>,
    /// Regular expressions whose matches are masked in logs, errors and reports.
    pub redact: Vec<String>,
    /// Script suite files to run, relative to [`ProjectConfig::root`].
    pub suites: Vec<PathBuf>,
    /// Report outputs.
    pub reports: Reports,
    /// JS runtimes per suite for functional runs (load runs use one per VU).
    pub js_workers: Option<usize>,
    /// Default `User-Agent` for the run.
    pub user_agent: Option<String>,
    /// Trace-context propagation; omitted fields keep their defaults.
    pub tracing: Option<crate::trace::TracingConfig>,
    /// Login flow per service.
    pub auth: BTreeMap<String, crate::auth::LoginFlow>,
    /// Named actors; `${VAR}` in credentials and headers is resolved from
    /// `--var`, the environment, `.env` files and `vars:`.
    pub actors: BTreeMap<String, crate::auth::ActorConfig>,
    /// Comparison specs for the contract drift check.
    pub drift: Drift,
    /// Directory the file was loaded from; relative paths resolve against it.
    #[serde(skip)]
    pub root: PathBuf,
}

impl ProjectConfig {
    /// Reads and parses the config file at `path` and sets
    /// [`ProjectConfig::root`] to its directory.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or does not match [`CONFIG_SCHEMA`].
    pub fn load(path: &Path) -> Result<ProjectConfig> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
        let mut cfg = Self::parse(&text, &path.display().to_string())?;
        cfg.root = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Ok(cfg)
    }

    /// Parses `apitest.nova` source text and validates it against [`CONFIG_SCHEMA`].
    /// `what` names the source in error messages. Relative paths resolve against
    /// the current directory until [`ProjectConfig::root`] is set.
    ///
    /// # Errors
    ///
    /// Fails when `text` is not valid NOVA or does not match the schema; the
    /// message lists every issue with its path. Also fails when a drift target
    /// uses a reserved name (`live`, `all`).
    pub fn parse(text: &str, what: &str) -> Result<ProjectConfig> {
        let cfg: ProjectConfig = from_nova(text, CONFIG_SCHEMA, what)?;
        if let Some(name) = cfg
            .drift
            .targets
            .keys()
            .find(|n| *n == crate::contract::LIVE || *n == crate::contract::ALL)
        {
            bail!("{what}: drift target name '{name}' is reserved");
        }

        Ok(cfg)
    }

    /// Looks for `apitest.nova` in `dir` and its parents and loads the
    /// nearest one. Returns `Ok(None)` when there is none up to the root.
    ///
    /// # Errors
    ///
    /// Fails when a found file cannot be loaded (see [`ProjectConfig::load`]).
    pub fn discover(dir: &Path) -> Result<Option<ProjectConfig>> {
        let mut d = Some(dir);
        while let Some(dir) = d {
            let candidate = dir.join("apitest.nova");
            if candidate.is_file() {
                return ProjectConfig::load(&candidate).map(Some);
            }
            d = dir.parent();
        }
        Ok(None)
    }

    /// Resolves `p` against [`ProjectConfig::root`]; absolute paths are
    /// returned unchanged.
    #[must_use]
    pub fn resolve(&self, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        }
    }

    /// The `.env` files this project reads, in load order (later files win):
    /// `env_files:` resolved against [`ProjectConfig::root`] (each must exist),
    /// else [`DEFAULT_ENV_FILES`](crate::dotenv::DEFAULT_ENV_FILES) next to the
    /// config (read if they exist).
    #[must_use]
    pub fn env_files(&self) -> Vec<crate::dotenv::EnvFile> {
        match &self.env_files {
            Some(files) => files
                .iter()
                .map(|f| crate::dotenv::EnvFile {
                    path: self.resolve(f),
                    required: true,
                })
                .collect(),
            None => crate::dotenv::DEFAULT_ENV_FILES
                .iter()
                .map(|f| crate::dotenv::EnvFile {
                    path: self.root.join(f),
                    required: false,
                })
                .collect(),
        }
    }

    /// Applies this config on top of `base`. Set scalar fields replace the
    /// base value; maps (`services`, `auth`, `actors`, and `vars` into
    /// [`RunConfig::var_defaults`]) are merged, with this config winning on
    /// equal keys. Drift targets are merged per contract, and a non-empty
    /// `drift.against` replaces the base selection. `redact:` patterns are added
    /// to the base's redactor. `.env` files are not read here (see
    /// [`ProjectConfig::env_files`] and [`RunConfig::load_env_file`]).
    ///
    /// # Errors
    ///
    /// Fails when a `redact:` pattern is not a valid regular expression.
    pub fn apply(&self, mut base: RunConfig) -> Result<RunConfig> {
        for (i, re) in self.redact.iter().enumerate() {
            base.redactor
                .pattern(re)
                .with_context(|| format!("redact[{i}] in apitest.nova"))?;
        }

        if let Some(u) = &self.base_url {
            base.base_url.clone_from(u);
        }

        if let Some(l) = &self.lockfile {
            base.lockfile = Some(self.resolve(l));
        }

        if let Some(n) = self.max_parallel {
            base.max_parallel = n;
        }

        if let Some(t) = self.test_timeout {
            base.test_timeout = t.0;
        }

        if let Some(t) = self.hook_timeout {
            base.hook_timeout = t.0;
        }

        base.var_defaults.extend(self.vars.clone());
        base.services.extend(self.services.clone());

        if let Some(ua) = &self.user_agent {
            base.user_agent.clone_from(ua);
        }

        if let Some(t) = &self.tracing {
            base.tracing = t.clone();
        }

        base.auth.extend(self.auth.clone());
        base.actors.extend(self.actors.clone());
        self.apply_drift(&mut base);

        Ok(base)
    }

    /// Merges `drift:` into `base`, resolving file specs against
    /// [`ProjectConfig::root`].
    fn apply_drift(&self, base: &mut RunConfig) {
        for (name, specs) in &self.drift.targets {
            let target = base.drift_targets.entry(name.clone()).or_default();
            for (contract, spec) in specs {
                let source = crate::contract::SpecSource::parse(spec, &self.root);
                target.specs.insert(contract.clone(), source);
            }
        }

        if !self.drift.against.is_empty() {
            base.drift_against.clone_from(&self.drift.against);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_parse() {
        novafile_schema::parse_schema(CONFIG_SCHEMA).unwrap();
        novafile_schema::parse_schema(LOCK_SCHEMA).unwrap();
    }

    #[test]
    fn demo_config_matches_schema() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/apitest.nova");
        let cfg = ProjectConfig::load(&path).unwrap();
        assert_eq!(
            cfg.test_timeout,
            Some(NovaDuration(Duration::from_secs(30)))
        );
        assert_eq!(cfg.vars["API_USER"], "demo");
    }

    #[test]
    fn tracing_and_user_agent_in_config() {
        let cfg: ProjectConfig = from_nova(
            "user_agent: my-tests/2.0\ntracing:\n  formats: [w3c, xray, datadog]\n  scope: suite\n  sampled: false\n  baggage:\n    env: ci\n",
            CONFIG_SCHEMA,
            "apitest.nova",
        )
        .unwrap();
        let run = cfg.apply(RunConfig::default()).unwrap();
        assert_eq!(run.user_agent, "my-tests/2.0");
        assert_eq!(
            run.tracing.formats,
            vec![
                crate::trace::TraceFormat::W3c,
                crate::trace::TraceFormat::Xray,
                crate::trace::TraceFormat::Datadog
            ]
        );
        assert_eq!(run.tracing.scope, crate::trace::TraceScope::Suite);
        assert!(!run.tracing.sampled && run.tracing.enabled);
        assert_eq!(run.tracing.baggage["env"], "ci");
        let off: ProjectConfig =
            from_nova("tracing:\n  enabled: false\n", CONFIG_SCHEMA, "x").unwrap();
        assert!(!off.apply(RunConfig::default()).unwrap().tracing.enabled);
        let bad = from_nova::<ProjectConfig>("tracing:\n  formats: [zipkin]\n", CONFIG_SCHEMA, "x")
            .unwrap_err()
            .to_string();
        assert!(bad.contains("tracing.formats"), "{bad}");
    }

    #[test]
    fn auth_and_actors_in_config() {
        let text = r#"
auth:
  hub:
    operation: login
    body:
      email: "{username}"
      password: "{password}"
    token: /token
actors:
  hub_admin:
    service: hub
    auth:
      type: login
      username: "${HUB_ADMIN_EMAIL}"
      password: "${HUB_ADMIN_PASSWORD}"
  reporting:
    service: reports
    auth:
      type: client_credentials
      token_url: https://idp.example.test/oauth/token
      client_id: reporting
      client_secret: "${REPORTING_SECRET}"
      scope: reports.read
  monitor:
    service: hub
    user_agent: synthetic-monitor/1.0
    auth:
      type: header
      name: X-Api-Key
      value: "${MONITOR_KEY}"
"#;
        let cfg: ProjectConfig = from_nova(text, CONFIG_SCHEMA, "apitest.nova").unwrap();
        let run = cfg.apply(RunConfig::default()).unwrap();
        assert_eq!(run.auth["hub"].body["email"], "{username}");
        assert_eq!(run.auth["hub"].token, "/token");
        assert!(
            matches!(&run.actors["hub_admin"].auth, crate::auth::Auth::Login { username, .. } if username == "${HUB_ADMIN_EMAIL}")
        );
        assert!(
            matches!(&run.actors["reporting"].auth, crate::auth::Auth::ClientCredentials(c) if c.scope.as_deref() == Some("reports.read"))
        );
        assert_eq!(
            run.actors["monitor"].user_agent.as_deref(),
            Some("synthetic-monitor/1.0")
        );
        let bad = from_nova::<ProjectConfig>(
            "actors:\n  x:\n    auth:\n      type: kerberos\n",
            CONFIG_SCHEMA,
            "x",
        )
        .unwrap_err()
        .to_string();
        assert!(bad.contains("actors.x.auth.type"), "{bad}");
    }

    #[test]
    fn drift_targets_in_config() {
        let mut cfg = ProjectConfig::parse(
            "drift:\n  against: [prod, live]\n  targets:\n    prod:\n      billing: specs/billing-prod.yaml\n    dev:\n      billing: https://dev.example.test/openapi.json\n",
            "apitest.nova",
        )
        .unwrap();
        cfg.root = PathBuf::from("/proj");
        let run = cfg.apply(RunConfig::default()).unwrap();
        assert_eq!(
            run.drift_targets["prod"].specs["billing"],
            crate::contract::SpecSource::File("/proj/specs/billing-prod.yaml".into())
        );
        assert_eq!(
            run.drift_targets["dev"].specs["billing"],
            crate::contract::SpecSource::Url("https://dev.example.test/openapi.json".into())
        );
        assert_eq!(run.selected_drift_targets().unwrap(), ["prod", "live"]);

        // Config targets merge into programmatic ones per contract.
        let merged = cfg
            .apply(RunConfig::default().drift_target(
                "prod",
                crate::contract::DriftTarget::new().file("hub", "/hub.yaml"),
            ))
            .unwrap();
        assert_eq!(merged.drift_targets["prod"].specs.len(), 2);
    }

    #[test]
    fn env_files_and_redaction_in_config() {
        let mut cfg = ProjectConfig::parse(
            "vars:\n  A: default\nenv_files: [.env, .env.ci]\nredact:\n  - \"cust-[0-9]+\"\n",
            "apitest.nova",
        )
        .unwrap();
        cfg.root = PathBuf::from("/project");
        let files = cfg.env_files();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.required));
        assert_eq!(files[1].path, PathBuf::from("/project/.env.ci"));

        let run = cfg.apply(RunConfig::default()).unwrap();
        // `vars:` are defaults, not overrides.
        assert!(run.vars.is_empty());
        assert_eq!(run.var_defaults["A"], "default");
        assert_eq!(run.redactor.redact("id cust-42"), "id ***");

        let defaults = ProjectConfig {
            root: PathBuf::from("/project"),
            ..Default::default()
        }
        .env_files();
        assert_eq!(
            defaults
                .iter()
                .map(|f| (f.path.clone(), f.required))
                .collect::<Vec<_>>(),
            [
                (PathBuf::from("/project/.env"), false),
                (PathBuf::from("/project/.env.local"), false)
            ]
        );

        let bad = ProjectConfig {
            redact: vec!["(".into()],
            ..Default::default()
        };
        let err = format!("{:#}", bad.apply(RunConfig::default()).unwrap_err());
        assert!(err.contains("redact[0]"), "{err}");
    }

    #[test]
    fn older_schema_versions_still_load() {
        for version in ["0.1.0", "0.2.0"] {
            let text = format!("@schema apitest-config@{version}\nvars:\n  A: x\n");
            let cfg = ProjectConfig::parse(&text, "apitest.nova").unwrap();
            assert_eq!(cfg.vars["A"], "x");
        }
    }

    #[test]
    fn drift_config_errors() {
        let err = ProjectConfig::parse(
            "drift:\n  targets:\n    live:\n      billing: a.yaml\n",
            "apitest.nova",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("drift target name 'live' is reserved"),
            "{err}"
        );
        let err = ProjectConfig::parse(
            "drift:\n  targets:\n    qa:\n      billing: \"\"\n",
            "apitest.nova",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("drift.targets"), "{err}");
    }

    #[test]
    fn demo_lockfile_matches_schema() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/apitest.lock.nova");
        let lock = crate::contract::Lockfile::load(&path).unwrap().unwrap();
        assert!(
            lock.contract["billing"]
                .used
                .iter()
                .any(|u| u.op == "createInvoice")
        );
    }

    #[test]
    fn schema_errors_name_the_path() {
        let err = from_nova::<ProjectConfig>(
            "max_parallel: 0\nservices:\n  hub: 8080\n",
            CONFIG_SCHEMA,
            "apitest.nova",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("max_parallel"), "{err}");
        let err =
            from_nova::<ProjectConfig>("base_urll: http://x\n", CONFIG_SCHEMA, "apitest.nova")
                .unwrap_err()
                .to_string();
        assert!(err.contains("base_urll"), "{err}");
    }

    #[test]
    fn written_lockfile_round_trips_through_its_schema() {
        let mut lock = crate::contract::Lockfile::default();
        lock.contract.insert(
            "hub".into(),
            crate::contract::LockedContract {
                spec: "hub.yaml".into(),
                version: "1.0.0".into(),
                sha256: "a".repeat(64),
                used: vec![crate::contract::OpUsage {
                    op: "createUser".into(),
                    request_fields: ["email".to_string()].into(),
                    statuses: [201u16].into(),
                    ..Default::default()
                }],
            },
        );
        let path = std::env::temp_dir().join(format!(
            "apitest-lock-roundtrip-{}.nova",
            std::process::id()
        ));
        lock.save(&path).unwrap();
        let back = crate::contract::Lockfile::load(&path).unwrap().unwrap();
        assert_eq!(back.contract["hub"].used[0].statuses, [201u16].into());
        let _ = std::fs::remove_file(path);
    }
}
