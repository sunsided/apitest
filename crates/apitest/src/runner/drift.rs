//! The drift check: diffing each contract's pin against the selected drift
//! targets (its live spec and/or named comparison specs).
//!
//! ```mermaid
//! flowchart LR
//!     sel[selected_drift_targets] --> src["target_source (per contract × target)"]
//!     src -->|none| skip[skip pair]
//!     src --> cache{cached?}
//!     cache -->|yes| report[DriftReport]
//!     cache -->|no| load[load spec: file, URL, document] --> diff[Contract::diff] --> report
//! ```

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};

use super::RunConfig;
use crate::contract::{
    ALL, Contract, DriftReport, DriftTarget, Finding, LIVE, LiveSpec, LockedContract, Lockfile,
    OpUsage, Severity, Spec, SpecSource,
};

impl RunConfig {
    /// Registers a named comparison target, e.g.
    /// `.drift_target("prod", DriftTarget::new().file("billing", "specs/billing-prod.yaml"))`.
    /// A target with the same name is replaced.
    #[must_use]
    pub fn drift_target(mut self, name: impl Into<String>, target: DriftTarget) -> Self {
        self.drift_targets.insert(name.into(), target);
        self
    }

    /// Selects the targets the drift check compares against: [`LIVE`], names
    /// registered with [`RunConfig::drift_target`], or [`ALL`]. With
    /// `.drift_against(["prod"])` only the `prod` specs are checked and nothing is
    /// fetched from the live service.
    #[must_use]
    pub fn drift_against<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.drift_against = names.into_iter().map(Into::into).collect();
        self
    }

    /// The selected drift targets in selection order, with [`ALL`] expanded to
    /// [`LIVE`] plus every named target and duplicates removed. An empty
    /// [`RunConfig::drift_against`] selects [`LIVE`] only.
    ///
    /// # Errors
    ///
    /// Fails if a named target uses a reserved name ([`LIVE`], [`ALL`]), or a
    /// selected name is neither reserved nor a registered target.
    pub fn selected_drift_targets(&self) -> Result<Vec<&str>> {
        if let Some(reserved) = [LIVE, ALL]
            .into_iter()
            .find(|r| self.drift_targets.contains_key(*r))
        {
            bail!("drift target name '{reserved}' is reserved");
        }

        let requested: Vec<&str> = if self.drift_against.is_empty() {
            vec![LIVE]
        } else {
            self.drift_against.iter().map(String::as_str).collect()
        };

        let mut selected: Vec<&str> = vec![];
        for name in requested {
            for t in self.expand_drift_target(name)? {
                if !selected.contains(&t) {
                    selected.push(t);
                }
            }
        }

        Ok(selected)
    }

    /// The targets one selected `name` stands for.
    ///
    /// # Errors
    ///
    /// Fails if `name` is neither [`LIVE`], [`ALL`] nor a registered target.
    fn expand_drift_target<'a>(&'a self, name: &'a str) -> Result<Vec<&'a str>> {
        match name {
            ALL => Ok(std::iter::once(LIVE)
                .chain(self.drift_targets.keys().map(String::as_str))
                .collect()),
            LIVE => Ok(vec![LIVE]),
            _ => self
                .drift_targets
                .get_key_value(name)
                .map(|(k, _)| vec![k.as_str()])
                .ok_or_else(|| {
                    let known: Vec<&str> = [LIVE, ALL]
                        .into_iter()
                        .chain(self.drift_targets.keys().map(String::as_str))
                        .collect();
                    anyhow!(
                        "unknown drift target '{name}' (known: {})",
                        known.join(", ")
                    )
                }),
        }
    }

    /// Checks that every selected named target only has specs for `contracts`.
    ///
    /// A spec for a contract no suite has is almost always a typo that would
    /// silently skip the check, so callers that know every contract of the
    /// project (such as the CLI) call this before running.
    ///
    /// # Errors
    ///
    /// Fails if the selection is invalid (see
    /// [`RunConfig::selected_drift_targets`]) or a selected target names a
    /// contract that is not in `contracts`.
    pub fn check_drift_targets<'a>(
        &self,
        contracts: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let known: BTreeSet<&str> = contracts.into_iter().collect();
        for name in self.selected_drift_targets()? {
            let Some(target) = self.drift_targets.get(name) else {
                continue;
            };

            if let Some(c) = target.specs.keys().find(|c| !known.contains(c.as_str())) {
                bail!(
                    "drift target '{name}' has a spec for unknown contract '{c}' (contracts: {})",
                    known.into_iter().collect::<Vec<_>>().join(", ")
                );
            }
        }

        Ok(())
    }
}

/// Diffs every contract's pin against each selected drift target (see
/// [`RunConfig::drift_against`]): its live spec and/or named comparison specs.
///
/// Returns one report per contract and target, contracts first, targets in
/// selection order. A contract without a live spec, or without a spec in a named
/// target, is skipped for that target. With a lockfile whose pin matches, only
/// the recorded used surface is checked; otherwise every operation. With
/// [`RunConfig::drift_cache`] set, results are read from and written to that
/// directory so concurrent processes of one run diff each pair only once.
///
/// # Errors
///
/// Fails if the target selection is invalid, the lockfile cannot be read or
/// parsed, a contract's service is unknown, a compared spec cannot be read,
/// fetched or parsed, or a result cannot be serialized for the cache.
pub async fn check_drift(contracts: &[Arc<Contract>], cfg: &RunConfig) -> Result<Vec<DriftReport>> {
    let targets = cfg.selected_drift_targets()?;
    let lock = match &cfg.lockfile {
        Some(p) => Lockfile::load(p)?,
        None => None,
    };
    let http = reqwest::Client::new();

    let mut out = vec![];
    for c in contracts {
        let locked = lock.as_ref().and_then(|l| l.contract.get(&c.name));
        for &target in &targets {
            let Some(source) = target_source(c, target, cfg)? else {
                continue;
            };

            let pair = Pair {
                contract: c,
                target,
                source: &source,
                locked,
            };
            out.push(pair_drift(&pair, cfg, &http).await?);
        }
    }

    Ok(out)
}

/// One contract compared against one drift target.
struct Pair<'a> {
    /// The contract whose pin is compared.
    contract: &'a Contract,
    /// The target name, [`LIVE`] or a named target.
    target: &'a str,
    /// The spec the target has for the contract.
    source: &'a SpecSource,
    /// The contract's lockfile entry, if any.
    locked: Option<&'a LockedContract>,
}

/// The spec `target` has for `c`, `None` if it has none.
///
/// # Errors
///
/// Fails if the live spec is relative and the contract's service is unknown.
fn target_source(c: &Contract, target: &str, cfg: &RunConfig) -> Result<Option<SpecSource>> {
    if target != LIVE {
        return Ok(cfg
            .drift_targets
            .get(target)
            .and_then(|t| t.specs.get(&c.name))
            .cloned());
    }

    Ok(match &c.live_spec {
        None => None,
        Some(LiveSpec::Url(u)) => Some(SpecSource::Url(live_url(u, cfg.contract_url(c)?))),
        Some(LiveSpec::Document(d)) => Some(SpecSource::Document(d.clone())),
    })
}

/// Drift report of one pair, from the cache if possible.
///
/// A file spec is read once: its content goes into the cache key and, on a
/// cache miss, into the diff.
///
/// # Errors
///
/// Fails if the compared spec cannot be read, fetched or parsed, or the report
/// cannot be serialized for the cache.
async fn pair_drift(
    pair: &Pair<'_>,
    cfg: &RunConfig,
    http: &reqwest::Client,
) -> Result<DriftReport> {
    let c = pair.contract;
    let what = || {
        format!(
            "loading the '{}' spec of contract '{}'",
            pair.target, c.name
        )
    };
    let file_bytes = read_spec_file(pair.source).with_context(what)?;
    let (used, notes) = used_surface(c, pair.locked);
    let cache_file = cfg.drift_cache.as_ref().map(|dir| {
        let key = drift_cache_key(pair, file_bytes.as_deref(), used);
        dir.join(format!("drift-{key}.json"))
    });
    if let Some(cached) = cache_file.as_deref().and_then(read_cached_drift) {
        return Ok(cached);
    }

    let loaded = match (pair.source, file_bytes) {
        (SpecSource::File(p), Some(raw)) => {
            parse_spec_file(p, &raw).map(|s| (s, p.display().to_string()))
        }
        (source, _) => fetch_spec(source, http).await,
    };
    let (spec, shown) = loaded.with_context(what)?;

    let mut report = c.diff(&spec, used, &shown);
    report.target = pair.target.to_string();
    report.findings.splice(0..0, notes);
    if let Some(file) = &cache_file {
        write_cached_drift(file, &report)?;
    }

    Ok(report)
}

/// The raw content of a file spec, `None` for other sources.
///
/// # Errors
///
/// Fails if the file cannot be read.
fn read_spec_file(source: &SpecSource) -> Result<Option<Vec<u8>>> {
    match source {
        SpecSource::File(p) => std::fs::read(p)
            .map(Some)
            .with_context(|| format!("reading {}", p.display())),
        SpecSource::Url(_) | SpecSource::Document(_) => Ok(None),
    }
}

/// Parses the already read content of the spec file at `path`.
///
/// # Errors
///
/// Fails if the content is not a valid spec.
fn parse_spec_file(path: &Path, raw: &[u8]) -> Result<Spec> {
    Spec::from_bytes(raw, path.extension().and_then(|e| e.to_str()))
        .with_context(|| format!("parsing {}", path.display()))
}

/// Loads a compared spec and describes where it came from.
///
/// # Errors
///
/// Fails if the request fails or returns an error status, the file cannot be
/// read, or the document is not a valid spec.
async fn fetch_spec(source: &SpecSource, http: &reqwest::Client) -> Result<(Spec, String)> {
    match source {
        SpecSource::Document(doc) => Ok((
            Spec::from_value(doc, String::new())?,
            "in-process document".to_string(),
        )),
        SpecSource::File(p) => Ok((Spec::from_file(p)?, p.display().to_string())),
        SpecSource::Url(url) => {
            let resp = http
                .get(url)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .with_context(|| format!("fetching {url}"))?;
            let bytes = resp.bytes().await?;
            let ext = is_yaml_url(url).then_some("yaml");
            Ok((Spec::from_bytes(&bytes, ext)?, url.clone()))
        }
    }
}

/// The used surface to check, from the lockfile if its pin matches the contract's.
///
/// A stale lockfile entry yields `None` (check everything) plus an info finding
/// explaining why.
fn used_surface<'a>(
    c: &Contract,
    locked: Option<&'a LockedContract>,
) -> (Option<&'a [OpUsage]>, Vec<Finding>) {
    match locked {
        Some(l) if l.sha256 == c.pinned.sha256 => (Some(l.used.as_slice()), vec![]),
        Some(l) => {
            let note = Finding {
                severity: Severity::Info,
                operation: "-".into(),
                location: "lockfile".into(),
                message: format!(
                    "pinned spec changed since the lockfile was written (lock: v{}, now: v{}); checking all operations",
                    l.version, c.pinned.version
                ),
            };
            (None, vec![note])
        }
        None => (None, vec![]),
    }
}

/// A cached drift report, if `file` exists and parses.
fn read_cached_drift(file: &Path) -> Option<DriftReport> {
    let bytes = std::fs::read(file).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Caches a drift report. I/O failures are ignored: the cache is an optimization.
///
/// # Errors
///
/// Fails only if the report cannot be serialized.
fn write_cached_drift(file: &Path, report: &DriftReport) -> Result<()> {
    // Write-then-rename so concurrent processes never read a partial file.
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")));
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(report)?).is_ok() {
        let _ = std::fs::rename(&tmp, file);
    }

    Ok(())
}

/// `true` if the path of `url` (without query and fragment) ends in `.yaml` or
/// `.yml` (case-sensitive, like the server's path).
fn is_yaml_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| ext == "yaml" || ext == "yml")
}

/// Absolute URL of a live spec: `u` itself if absolute, else `u` appended to `base`.
fn live_url(u: &str, base: &str) -> String {
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("{}{}", base.trim_end_matches('/'), u)
    }
}

/// Everything that influences a drift result: contract, pin, mode, target,
/// compared spec (a file by its path and content), used surface.
///
/// Returns the first 12 bytes of a SHA-256 over those inputs as 24 hex characters.
fn drift_cache_key(pair: &Pair<'_>, file_bytes: Option<&[u8]>, used: Option<&[OpUsage]>) -> String {
    use sha2::{Digest, Sha256};

    let c = pair.contract;
    let mut h = Sha256::new();
    h.update(c.name.as_bytes());
    h.update(c.pinned.sha256.as_bytes());
    h.update(format!("{:?}", c.mode).as_bytes());
    h.update(pair.target.as_bytes());
    match pair.source {
        SpecSource::Url(u) => h.update(u.as_bytes()),
        SpecSource::File(p) => {
            h.update(p.display().to_string().as_bytes());
            h.update(file_bytes.unwrap_or_default());
        }
        SpecSource::Document(d) => h.update(d.to_string().as_bytes()),
    }

    h.update(serde_json::to_vec(&used).unwrap_or_default());
    h.finalize()
        .iter()
        .take(12)
        .fold(String::with_capacity(24), |mut key, b| {
            let _ = write!(key, "{b:02x}");
            key
        })
}

/// The setup error for breaking drift reports, `None` if none is breaking:
/// `live API is incompatible with pinned contract(s): billing`, or with
/// `compared spec(s)` for named targets (`billing @ prod`).
pub(crate) fn drift_failure(reports: &[DriftReport]) -> Option<String> {
    let breaking: Vec<&DriftReport> = reports.iter().filter(|d| d.is_breaking()).collect();
    if breaking.is_empty() {
        return None;
    }

    let live = breaking.iter().any(|d| d.target == LIVE);
    let named = breaking.iter().any(|d| d.target != LIVE);
    let what = match (live, named) {
        (true, true) => "live API and compared spec(s) are",
        (true, false) => "live API is",
        _ => "compared spec(s) are",
    };
    let labels: Vec<String> = breaking.iter().map(|d| d.label()).collect();

    Some(format!(
        "{what} incompatible with pinned contract(s): {}",
        labels.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_expands_all_and_rejects_unknown_and_reserved_names() {
        let cfg = RunConfig::default()
            .drift_target("qa", DriftTarget::new())
            .drift_target("dev", DriftTarget::new());
        assert_eq!(cfg.selected_drift_targets().unwrap(), [LIVE]);
        let all = cfg.clone().drift_against(["qa", ALL, LIVE]);
        assert_eq!(all.selected_drift_targets().unwrap(), ["qa", LIVE, "dev"]);

        let unknown = cfg.clone().drift_against(["prod"]);
        let err = unknown.selected_drift_targets().unwrap_err().to_string();
        assert!(
            err.contains("unknown drift target 'prod' (known: live, all, dev, qa)"),
            "{err}"
        );

        let reserved = RunConfig::default().drift_target(LIVE, DriftTarget::new());
        let err = reserved.selected_drift_targets().unwrap_err().to_string();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn unknown_contracts_in_selected_targets_are_rejected() {
        let cfg = RunConfig::default()
            .drift_target("qa", DriftTarget::new().file("billing", "/b.yaml"))
            .drift_target("dev", DriftTarget::new().file("hub", "/h.yaml"));
        // `dev` is not selected, so its unknown contract does not matter.
        assert!(cfg.check_drift_targets(["billing"]).is_ok());
        let err = cfg
            .drift_against(["qa", "dev"])
            .check_drift_targets(["billing"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("drift target 'dev' has a spec for unknown contract 'hub'"),
            "{err}"
        );
    }

    #[test]
    fn yaml_urls_ignore_query_and_fragment() {
        assert!(is_yaml_url("https://x.test/openapi.yaml?v=2#top"));
        assert!(!is_yaml_url("https://x.test/openapi.json?f=a.yaml"));
    }
}
