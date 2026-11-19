//! Where variables come from, and the up-front check of every `${NAME}` the
//! configured actors use.
//!
//! A name is looked up in this order, highest first: [`RunConfig::vars`]
//! (`--var`), the process environment, [`RunConfig::env_file_vars`] (`.env`
//! files), [`RunConfig::var_defaults`] (`vars:` in `apitest.nova`). The process
//! environment is only read, never written.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, anyhow};

use super::RunConfig;
use crate::auth::{TemplateField, interpolate_with, references};
use crate::dotenv::EnvFile;

impl RunConfig {
    /// A default for a variable, like `vars:` in `apitest.nova`: used only when
    /// no other source has the name.
    #[must_use]
    pub fn var_default(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.var_defaults.insert(k.into(), v.into());
        self
    }

    /// Reads a `.env` file into [`RunConfig::env_file_vars`]. Later files win
    /// over earlier ones.
    ///
    /// # Errors
    ///
    /// Fails when the file does not exist, cannot be read or does not parse
    /// (see [`crate::dotenv::read`]).
    pub fn env_file(mut self, path: impl AsRef<Path>) -> Result<Self> {
        self.load_env_file(&EnvFile {
            path: path.as_ref().to_path_buf(),
            required: true,
        })?;

        Ok(self)
    }

    /// Reads a `.env` file into [`RunConfig::env_file_vars`], over the values
    /// already there. Returns `false` when an optional file does not exist.
    ///
    /// # Errors
    ///
    /// Fails when a required file does not exist, or any file cannot be read or
    /// does not parse.
    pub fn load_env_file(&mut self, file: &EnvFile) -> Result<bool> {
        let vars = if file.required {
            crate::dotenv::read(&file.path)?
        } else {
            match crate::dotenv::read_if_exists(&file.path)? {
                Some(vars) => vars,
                None => return Ok(false),
            }
        };

        self.env_file_vars.extend(vars);
        Ok(true)
    }

    /// Masks every match of the regular expression `re` in logs, errors and
    /// reports (see [`crate::redact`]).
    ///
    /// # Errors
    ///
    /// Fails when `re` is not a valid regular expression.
    pub fn redact_pattern(self, re: &str) -> Result<Self> {
        self.redactor.pattern(re)?;
        Ok(self)
    }

    /// The value of a variable, highest source first: [`RunConfig::vars`]
    /// (`--var`), the process environment, [`RunConfig::env_file_vars`] (`.env`
    /// files), [`RunConfig::var_defaults`] (`vars:`).
    ///
    /// A variable that is set but empty counts as set in every source: an empty
    /// `FOO` in the environment hides `FOO` in `.env`. An environment value that
    /// is not valid UTF-8 reads as unset here; [`RunConfig::try_lookup`] reports it.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<String> {
        self.try_lookup(key).ok().flatten()
    }

    /// Like [`RunConfig::lookup`], but an environment value that is not valid
    /// UTF-8 is an error instead of falling through to the next source.
    ///
    /// # Errors
    ///
    /// Fails when the process environment has `key` with a non-UTF-8 value.
    pub fn try_lookup(&self, key: &str) -> Result<Option<String>> {
        if let Some(v) = self.vars.get(key) {
            return Ok(Some(v.clone()));
        }

        if let Some(v) = process_env(key)? {
            return Ok(Some(v));
        }

        Ok(self
            .env_file_vars
            .get(key)
            .or_else(|| self.var_defaults.get(key))
            .cloned())
    }

    /// Checks that every `${NAME}` in the configured actors resolves, and
    /// registers their secret values with the [`RunConfig::redactor`].
    ///
    /// All problems are reported at once: missing variables with every field
    /// that uses them, malformed templates, and variables that are set but empty
    /// (once per field). A literal empty value is fine. Runs call this before any
    /// test starts.
    ///
    /// # Errors
    ///
    /// Fails when any of the problems above occurs. The message names variables
    /// and fields, never values or templates.
    pub fn check_variables(&self) -> Result<()> {
        let mut problems = Problems::default();
        for (name, actor) in &self.actors {
            for field in actor.template_fields() {
                let place = format!("actors.{name}.{}", field.path);
                self.check_field(&place, &field, &mut problems);
            }
        }

        problems.into_result()
    }

    /// Checks one template field at `place` (see [`RunConfig::check_variables`]),
    /// registering the values of a secret field.
    fn check_field(&self, place: &str, field: &TemplateField<'_>, problems: &mut Problems) {
        // Once per name, however often the template repeats it.
        let names: BTreeSet<String> = match references(field.template) {
            Ok(names) => names.into_iter().collect(),
            Err(e) => return problems.error(place, &e),
        };

        let mut complete = true;
        for name in &names {
            match self.try_lookup(name) {
                Ok(Some(v)) => {
                    if v.is_empty() {
                        problems.error(place, &format!("variable {name} is empty"));
                    }

                    if field.secret {
                        // Each part on its own too: `KEY` of "prefix ${KEY}".
                        self.redactor.secret(&v);
                    }
                }
                Ok(None) => {
                    complete = false;
                    problems.missing(name, place);
                }
                Err(e) => {
                    complete = false;
                    problems.error(place, &e);
                }
            }
        }

        if complete && field.secret {
            self.register_resolved(field.template);
        }
    }

    /// Registers the resolved value of a secret template whose variables all exist.
    fn register_resolved(&self, template: &str) {
        if let Ok(v) = interpolate_with(template, |k| Ok(self.lookup(k).unwrap_or_default())) {
            self.redactor.secret(&v);
        }
    }
}

/// A process environment variable. Names no platform accepts (empty, or with
/// `=` or NUL, as `${A=B}` would give) are simply unset.
///
/// # Errors
///
/// Fails when the value is not valid UTF-8.
fn process_env(key: &str) -> Result<Option<String>> {
    if key.is_empty() || key.contains(['=', '\0']) {
        return Ok(None);
    }

    match std::env::var(key) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(anyhow!("environment variable '{key}' is not valid UTF-8"))
        }
    }
}

/// What [`RunConfig::check_variables`] found.
#[derive(Default)]
struct Problems {
    /// Unset variable → the fields that use it, in order.
    missing: BTreeMap<String, Vec<String>>,
    /// `  - <field>: <problem>` lines.
    errors: Vec<String>,
}

impl Problems {
    /// Records that `name`, used at `place`, is not set anywhere.
    fn missing(&mut self, name: &str, place: &str) {
        self.missing
            .entry(name.to_string())
            .or_default()
            .push(place.to_string());
    }

    /// Records a problem with the field at `place`.
    fn error(&mut self, place: &str, problem: &dyn std::fmt::Display) {
        self.errors.push(format!("  - {place}: {problem}"));
    }

    /// `Ok` without problems, else one error listing them all.
    fn into_result(mut self) -> Result<()> {
        for (name, places) in &self.missing {
            self.errors
                .push(format!("  - {name} (used by {})", places.join(", ")));
        }

        if self.errors.is_empty() {
            return Ok(());
        }

        Err(anyhow!(
            "unresolved variables in configured actors (set them with --var, the environment, \
             .env.local, .env or `vars:` in apitest.nova):\n{}",
            self.errors.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::auth::{ActorConfig, Auth};

    use super::*;

    #[test]
    fn variable_precedence() {
        // PATH is set wherever tests run; it stands in for "the environment".
        let path = std::env::var("PATH").unwrap();
        let mut c = RunConfig::default()
            .var_default("PATH", "from-vars")
            .var_default("ONLY_DEFAULT_APITEST", "d");
        c.env_file_vars.insert("PATH".into(), "from-file".into());
        c.env_file_vars
            .insert("ONLY_FILE_APITEST".into(), "f".into());
        c.var_defaults
            .insert("ONLY_FILE_APITEST".into(), "d".into());
        assert_eq!(c.lookup("PATH").unwrap(), path);
        assert_eq!(c.lookup("ONLY_FILE_APITEST").unwrap(), "f");
        assert_eq!(c.lookup("ONLY_DEFAULT_APITEST").unwrap(), "d");
        assert_eq!(c.lookup("NOT_SET_ANYWHERE_APITEST"), None);
        assert_eq!(c.lookup("A=B"), None);

        let c = c.var("PATH", "from-cli");
        assert_eq!(c.lookup("PATH").unwrap(), "from-cli");
    }

    #[test]
    fn missing_env_files() {
        let mut c = RunConfig::default();
        let path = std::env::temp_dir().join("apitest-definitely-missing.env");
        let optional = EnvFile {
            path: path.clone(),
            required: false,
        };
        assert!(!c.load_env_file(&optional).unwrap());
        assert!(RunConfig::default().env_file(&path).is_err());
    }

    #[test]
    fn registers_secrets_and_never_echoes_templates() {
        let c = RunConfig::default()
            .actor("a", "svc", Auth::basic("${U}", "${P}"))
            .var("U", "user")
            .var("P", "hunter22");
        c.check_variables().unwrap();
        assert_eq!(c.redactor.redact("user:hunter22"), "user:***");

        let bad = RunConfig::default()
            .actor("a", "svc", Auth::bearer("${UNTERMINATED"))
            .check_variables()
            .unwrap_err()
            .to_string();
        assert!(bad.contains("actors.a.auth.token: unterminated"), "{bad}");
        assert!(!bad.contains("UNTERMINATED"), "template echoed: {bad}");

        // Parts of composite secrets are registered on their own as well.
        let c = RunConfig::default()
            .actor("a", "svc", Auth::header("X-Key", "key=${K}"))
            .var("K", "abcdef123");
        c.check_variables().unwrap();
        assert_eq!(c.redactor.redact("abcdef123 key=abcdef123"), "*** ***");
    }

    #[test]
    fn empty_variables_are_reported_once_per_field() {
        let err = |auth: Auth| {
            RunConfig::default()
                .actor("a", "svc", auth)
                .var("E", "")
                .check_variables()
                .unwrap_err()
                .to_string()
        };

        let e = err(Auth::bearer("${E}"));
        assert!(
            e.contains("actors.a.auth.token: variable E is empty"),
            "{e}"
        );
        let e = err(Auth::header("Authorization", "Bearer ${E}"));
        assert!(
            e.contains("actors.a.auth.value: variable E is empty"),
            "{e}"
        );
        let e = err(Auth::basic("${E}", "${E}-${E}"));
        assert_eq!(e.matches("variable E is empty").count(), 2, "{e}");
        assert!(
            e.contains("actors.a.auth.username: variable E is empty"),
            "{e}"
        );

        // A literal empty header is intentional and allowed.
        let mut literal = RunConfig::default();
        literal.actors.insert(
            "a".into(),
            ActorConfig {
                headers: [("X-Optional".to_string(), String::new())].into(),
                ..Default::default()
            },
        );
        literal.check_variables().unwrap();
    }

    #[test]
    fn missing_variables_are_listed_with_their_fields() {
        let e = RunConfig::default()
            .actor(
                "a",
                "svc",
                Auth::login("svc", "${NOPE_USER_APITEST}", "${NOPE_PW_APITEST}"),
            )
            .actor("b", "svc", Auth::bearer("${NOPE_PW_APITEST}"))
            .check_variables()
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("NOPE_PW_APITEST (used by actors.a.auth.password, actors.b.auth.token)"),
            "{e}"
        );
        assert!(
            e.contains("NOPE_USER_APITEST (used by actors.a.auth.username)"),
            "{e}"
        );
    }
}
