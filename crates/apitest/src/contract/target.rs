//! Drift targets: the specs a pin is compared against besides the live one.
//!
//! The live spec of each contract is the built-in target [`LIVE`]. Named
//! targets ([`DriftTarget`]) add one comparison spec per contract, e.g. what the
//! `dev`, `qa` and `prod` environments serve or a local branch, so the drift
//! check can run without a running service.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Name of the built-in drift target: each contract's own
/// [`LiveSpec`](super::LiveSpec).
pub const LIVE: &str = "live";

/// Selects [`LIVE`] plus every named drift target.
pub const ALL: &str = "all";

/// A spec to diff a pin against, other than a contract's own
/// [`LiveSpec`](super::LiveSpec).
#[derive(Debug, Clone, PartialEq)]
pub enum SpecSource {
    /// Absolute `http://` or `https://` URL.
    Url(String),
    /// Spec file on disk (YAML or JSON, chosen by the extension).
    File(PathBuf),
    /// An in-process document.
    Document(Value),
}

impl SpecSource {
    /// Parses a spec reference from a config file or the command line.
    ///
    /// Surrounding whitespace is ignored. A value starting with `http://` or
    /// `https://` (in any letter case) is a [`SpecSource::Url`]; anything else
    /// is a [`SpecSource::File`], resolved against `root` unless absolute.
    #[must_use]
    pub fn parse(s: &str, root: &Path) -> SpecSource {
        let s = s.trim();
        if has_prefix_ignore_case(s, "http://") || has_prefix_ignore_case(s, "https://") {
            return SpecSource::Url(s.to_string());
        }

        let p = Path::new(s);
        SpecSource::File(if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        })
    }
}

/// Whether `s` starts with the ASCII `prefix`, ignoring letter case.
fn has_prefix_ignore_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// A named set of comparison specs, one per contract, e.g. what the `prod`
/// environment currently serves.
///
/// Contracts without an entry are not checked against this target.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriftTarget {
    /// Contract name → the spec to compare its pin against.
    pub specs: BTreeMap<String, SpecSource>,
}

impl DriftTarget {
    /// A target without any specs.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compares `contract` against a spec file.
    #[must_use]
    pub fn file(self, contract: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        self.spec(contract, SpecSource::File(path.into()))
    }

    /// Compares `contract` against a spec fetched from an absolute URL.
    #[must_use]
    pub fn url(self, contract: impl Into<String>, url: impl Into<String>) -> Self {
        self.spec(contract, SpecSource::Url(url.into()))
    }

    /// Compares `contract` against an in-process document.
    #[must_use]
    pub fn document(self, contract: impl Into<String>, doc: Value) -> Self {
        self.spec(contract, SpecSource::Document(doc))
    }

    /// Compares `contract` against `source`, replacing an earlier entry.
    #[must_use]
    pub fn spec(mut self, contract: impl Into<String>, source: SpecSource) -> Self {
        self.specs.insert(contract.into(), source);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_files_are_told_apart() {
        let root = Path::new("/proj");
        assert_eq!(
            SpecSource::parse(" HTTPS://x.test/a.yaml ", root),
            SpecSource::Url("HTTPS://x.test/a.yaml".into())
        );
        assert_eq!(
            SpecSource::parse("specs/a.yaml", root),
            SpecSource::File("/proj/specs/a.yaml".into())
        );
        assert_eq!(
            SpecSource::parse("/abs/a.yaml", root),
            SpecSource::File("/abs/a.yaml".into())
        );
        // Too short for the scheme, and a multi-byte character where it would end.
        assert!(matches!(
            SpecSource::parse("http", root),
            SpecSource::File(_)
        ));
        assert!(matches!(
            SpecSource::parse("httpé//x", root),
            SpecSource::File(_)
        ));
    }
}
