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
//! vars:
//!   API_USER: demo
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
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Reports {
    /// JUnit XML report path.
    pub junit: Option<PathBuf>,
    /// JSON report path.
    pub json: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub base_url: Option<String>,
    /// Named services (`hub`, `app`, `mail`, ...) → base URL.
    pub services: BTreeMap<String, String>,
    pub lockfile: Option<PathBuf>,
    pub max_parallel: Option<usize>,
    pub test_timeout: Option<NovaDuration>,
    pub hook_timeout: Option<NovaDuration>,
    pub vars: BTreeMap<String, String>,
    pub suites: Vec<PathBuf>,
    pub reports: Reports,
    /// JS runtimes per suite for functional runs (load runs use one per VU).
    pub js_workers: Option<usize>,
    /// Default `User-Agent` for the run.
    pub user_agent: Option<String>,
    /// Trace-context propagation; omitted fields keep their defaults.
    pub tracing: Option<crate::trace::TracingConfig>,
    /// Login flow per service.
    pub auth: BTreeMap<String, crate::auth::LoginFlow>,
    /// Named actors; `${VAR}` in credentials is resolved from vars / the environment.
    pub actors: BTreeMap<String, crate::auth::ActorConfig>,
    /// Directory the file was loaded from; relative paths resolve against it.
    #[serde(skip)]
    pub root: PathBuf,
}

impl ProjectConfig {
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
    pub fn parse(text: &str, what: &str) -> Result<ProjectConfig> {
        from_nova(text, CONFIG_SCHEMA, what)
    }

    /// Looks for `apitest.nova` in `dir` and its parents.
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

    pub fn resolve(&self, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        }
    }

    /// Applies this config on top of `base`.
    pub fn apply(&self, mut base: RunConfig) -> RunConfig {
        if let Some(u) = &self.base_url {
            base.base_url = u.clone();
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
        base.vars.extend(self.vars.clone());
        base.services.extend(self.services.clone());
        if let Some(ua) = &self.user_agent {
            base.user_agent = ua.clone();
        }
        if let Some(t) = &self.tracing {
            base.tracing = t.clone();
        }
        base.auth.extend(self.auth.clone());
        base.actors.extend(self.actors.clone());
        base
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
        let run = cfg.apply(RunConfig::default());
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
        assert!(!off.apply(RunConfig::default()).tracing.enabled);
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
        let run = cfg.apply(RunConfig::default());
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
