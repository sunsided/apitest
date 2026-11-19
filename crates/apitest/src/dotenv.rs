//! `.env` files as a source of variables.
//!
//! Files are parsed with `dotenvy` into [`RunConfig::env_file_vars`]; the
//! process environment is never changed. Where a variable comes from, highest
//! first (see [`RunConfig::lookup`]):
//!
//! 1. `--var NAME=…` / [`RunConfig::vars`]
//! 2. the process environment
//! 3. `.env` files, a later file winning (`.env.local` over `.env`)
//! 4. `vars:` in `apitest.nova` / [`RunConfig::var_defaults`]
//!
//! Without `env_files:` in `apitest.nova`, [`DEFAULT_ENV_FILES`] next to it are
//! read if they exist. Files named explicitly (`env_files:`, `--env-file`,
//! [`RunConfig::env_file`]) must exist.
//!
//! The files hold secrets, so no error quotes their content: messages name the
//! file, the kind of problem and, for syntax errors, a position.
//!
//! [`RunConfig::env_file_vars`]: crate::RunConfig::env_file_vars
//! [`RunConfig::lookup`]: crate::RunConfig::lookup
//! [`RunConfig::vars`]: crate::RunConfig::vars
//! [`RunConfig::var_defaults`]: crate::RunConfig::var_defaults
//! [`RunConfig::env_file`]: crate::RunConfig::env_file

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

/// The files read when `apitest.nova` has no `env_files:`, in load order.
pub const DEFAULT_ENV_FILES: [&str; 2] = [".env", ".env.local"];

/// One `.env` file to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFile {
    /// Where the file is.
    pub path: PathBuf,
    /// Whether a missing file is an error. `false` for [`DEFAULT_ENV_FILES`].
    pub required: bool,
}

/// Parses `.env` text into variables, without touching the process environment.
///
/// Supports `KEY=value`, `export KEY=value`, quotes and `#` comments. `${OTHER}`
/// refers to earlier lines of the same text or to the process environment.
/// A later line wins over an earlier one with the same key.
///
/// # Errors
///
/// Fails on a syntax error or a `${OTHER}` that cannot be resolved. The message
/// gives the kind of error and a position, never the text itself.
pub fn parse(text: &str) -> Result<BTreeMap<String, String>> {
    let mut vars = BTreeMap::new();
    for item in dotenvy::from_read_iter(text.as_bytes()) {
        let (k, v) = item.map_err(|e| anyhow!(describe(&e)))?;
        vars.insert(k, v);
    }

    Ok(vars)
}

/// Reads and parses the `.env` file at `path` (see [`parse`]).
///
/// # Errors
///
/// Fails when the file does not exist or cannot be read, is not UTF-8 text, or
/// does not parse.
pub fn read(path: &Path) -> Result<BTreeMap<String, String>> {
    read_if_exists(path)?.ok_or_else(|| anyhow!("reading {}: file not found", path.display()))
}

/// Like [`read`], but `Ok(None)` when the file does not exist.
///
/// # Errors
///
/// Fails on every other problem: no permission, a directory, text that is not
/// UTF-8, a syntax error.
pub fn read_if_exists(path: &Path) -> Result<Option<BTreeMap<String, String>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind() == ErrorKind::InvalidData => {
            bail!("reading {}: not valid UTF-8", path.display())
        }
        Err(e) => bail!("reading {}: {}", path.display(), e.kind()),
    };

    parse(&text)
        .map(Some)
        .map_err(|e| anyhow!("parsing {}: {e}", path.display()))
}

/// What went wrong, without quoting any file content.
///
/// dotenvy's own `Display` includes the offending line or value, which may
/// hold a secret.
fn describe(e: &dotenvy::Error) -> String {
    match e {
        dotenvy::Error::LineParse(_, index) => format!("syntax error at byte {index} of a line"),
        dotenvy::Error::Io(io) => format!("I/O error ({})", io.kind()),
        dotenvy::Error::EnvVar(std::env::VarError::NotPresent) => {
            "a referenced variable is not set".into()
        }
        dotenvy::Error::EnvVar(std::env::VarError::NotUnicode(_)) => {
            "a referenced variable is not valid UTF-8".into()
        }
        // Non-exhaustive: a future variant must not quote content either.
        _ => "unrecognized error".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, unique to this test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("apitest-dotenv-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_without_touching_the_environment() {
        let v = parse(
            "# comment\nAPITEST_DOTENV_A=one\nexport APITEST_DOTENV_B=\"two words\"\nAPITEST_DOTENV_C=${APITEST_DOTENV_A}-x\nEMPTY=\n",
        )
        .unwrap();
        assert_eq!(v["APITEST_DOTENV_A"], "one");
        assert_eq!(v["APITEST_DOTENV_B"], "two words");
        assert_eq!(v["APITEST_DOTENV_C"], "one-x");
        assert_eq!(v["EMPTY"], "");
        assert!(std::env::var("APITEST_DOTENV_A").is_err());
    }

    #[test]
    fn errors_never_quote_the_content() {
        for text in [
            "OK=1\nBROKEN='hunter2-secret\n",
            "BAD KEY=hunter2-secret\n",
            "A=\"hunter2-${secret\"\n",
        ] {
            if let Err(e) = parse(text) {
                assert!(!e.to_string().contains("hunter2"), "{e}");
            }
        }

        assert!(parse("BAD KEY=hunter2-secret\n").is_err());
    }

    #[test]
    fn missing_files_and_directories() {
        let dir = temp_dir("missing");
        assert!(read_if_exists(&dir.join("missing.env")).unwrap().is_none());
        assert!(read(&dir.join("missing.env")).is_err());
        // A directory is not "missing": it is an error even for optional files.
        assert!(read_if_exists(&dir).is_err());

        std::fs::write(dir.join(".env"), b"A=\xff\xfe\n").unwrap();
        let err = read(&dir.join(".env")).unwrap_err().to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
