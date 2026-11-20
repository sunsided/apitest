//! Keeps secrets out of console output, error messages and reports.
//!
//! Two sources feed a [`Redactor`]:
//!
//! - **Secret values**: the resolved credentials of configured actors and of every
//!   [`Auth`](crate::auth::Auth) a request uses (passwords, client secrets, bearer
//!   tokens, API keys, actor header values), and every token a login or
//!   client-credentials grant returns. They are registered by
//!   [`RunConfig::check_variables`](crate::RunConfig::check_variables) before a
//!   run starts and whenever a credential is resolved.
//! - **Patterns**: regular expressions from `redact:` in `apitest.nova`,
//!   `--redact` or [`RunConfig::redact_pattern`](crate::RunConfig::redact_pattern),
//!   for anything else that must never be printed (customer IDs, hostnames, ...).
//!
//! Every match is replaced with [`MASK`] in suite and load reports (console,
//! `JUnit`, JSON), in `cx.log` and script `print` output, and in CLI output and
//! errors. Secret values shorter than [`MIN_SECRET_LEN`] characters are not
//! registered: masking `"a"` would garble every line. Use a pattern for those.
//!
//! ```mermaid
//! flowchart LR
//!     text --> ranges[secret and pattern match ranges]
//!     ranges --> merge[merge overlapping or touching ranges]
//!     merge --> mask[replace each range with ***]
//!     mask -->|a secret formed across a mask| ranges2[secret ranges only]
//!     ranges2 --> merge
//!     mask --> out[masked text]
//! ```

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, Weak};

use anyhow::{Context as _, Result};
use regex::Regex;

/// What a masked value is replaced with.
pub const MASK: &str = "***";

/// Secret values shorter than this many characters are not registered.
pub const MIN_SECRET_LEN: usize = 4;

/// A shared set of secret values and patterns.
///
/// Cheap to clone, and clones share their state, so a value learned during a
/// run (a fetched token, say) is masked everywhere from then on.
#[derive(Clone, Default)]
pub struct Redactor(Arc<Inner>);

/// The state behind a [`Redactor`] and all its clones.
#[derive(Default)]
struct Inner {
    /// Registered secret values, each at least [`MIN_SECRET_LEN`] characters long.
    secrets: RwLock<BTreeSet<String>>,
    /// Patterns whose matches are masked, without duplicates.
    patterns: RwLock<Vec<Regex>>,
}

impl fmt::Debug for Redactor {
    /// Shows how many secrets and patterns there are, never what they are.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("secrets", &read(&self.0.secrets).len())
            .field("patterns", &read(&self.0.patterns).len())
            .finish()
    }
}

impl Redactor {
    /// Registers a secret value. Values shorter than [`MIN_SECRET_LEN`]
    /// characters are ignored.
    pub fn secret(&self, value: &str) {
        if value.chars().count() < MIN_SECRET_LEN || read(&self.0.secrets).contains(value) {
            return;
        }

        write(&self.0.secrets).insert(value.to_string());
    }

    /// Registers the value of a credential header: the whole value and, for
    /// `<scheme> <credential>` values, the credential alone.
    pub(crate) fn header_value(&self, value: &str) {
        self.secret(value);
        if let Some((_, credential)) = value.split_once(' ') {
            self.secret(credential.trim());
        }
    }

    /// Adds a regular expression; every non-empty match is masked.
    ///
    /// # Errors
    ///
    /// Fails when `re` is not a valid regular expression.
    pub fn pattern(&self, re: &str) -> Result<()> {
        let re = Regex::new(re).context("invalid redaction pattern")?;
        let mut patterns = write(&self.0.patterns);
        if !patterns.iter().any(|p| p.as_str() == re.as_str()) {
            patterns.push(re);
        }

        Ok(())
    }

    /// `text` with every secret and pattern match replaced by [`MASK`].
    ///
    /// Overlapping and adjacent matches become a single mask. The result never
    /// contains a registered secret, even where masking joins the text around
    /// a match into one.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let secrets = read(&self.0.secrets);
        let patterns = read(&self.0.patterns);
        if secrets.is_empty() && patterns.is_empty() {
            return text.to_string();
        }

        let mut ranges = secret_ranges(text, &secrets);
        ranges.extend(pattern_ranges(text, &patterns));
        let mut out = mask(text, ranges);

        // Each round replaces at least MIN_SECRET_LEN bytes with a shorter mask,
        // so it ends.
        loop {
            let ranges = secret_ranges(&out, &secrets);
            if ranges.is_empty() {
                return out;
            }

            out = mask(&out, ranges);
        }
    }

    /// Redacts `text` in place (see [`Redactor::redact`]).
    pub fn redact_in_place(&self, text: &mut String) {
        *text = self.redact(text);
    }

    /// Makes this redactor apply to output written without a run at hand:
    /// script `print`, `console.log` and `debug` (see [`console`]). Runs call
    /// this themselves; calling it again does nothing.
    ///
    /// Only a weak reference is kept: the redactor stays active while a clone of
    /// it (in a [`RunConfig`](crate::RunConfig), say) is alive, and is dropped,
    /// secrets and all, with its last clone.
    pub fn activate(&self) {
        let mut active = active().lock().unwrap_or_else(PoisonError::into_inner);
        active.retain(|w| w.strong_count() > 0);
        if !active.iter().any(|w| w.as_ptr() == Arc::as_ptr(&self.0)) {
            active.push(Arc::downgrade(&self.0));
        }
    }
}

/// Redacts `text` with every [activated](Redactor::activate) redactor that is
/// still alive.
#[must_use]
pub fn console(text: &str) -> String {
    let live: Vec<Redactor> = active()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(|w| w.upgrade().map(Redactor))
        .collect();

    live.iter().fold(text.to_string(), |t, r| r.redact(&t))
}

/// The process-wide list of activated redactors.
fn active() -> &'static Mutex<Vec<Weak<Inner>>> {
    static ACTIVE: OnceLock<Mutex<Vec<Weak<Inner>>>> = OnceLock::new();
    ACTIVE.get_or_init(Mutex::default)
}

/// A read lock that survives poisoning: the data is a plain set, valid either way.
fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// A write lock that survives poisoning (see [`read`]).
fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// Byte ranges of every occurrence of every secret in `text`.
///
/// Occurrences of one secret are found left to right without overlap; an
/// occurrence skipped that way overlaps a found one, so it is masked anyway.
fn secret_ranges(text: &str, secrets: &BTreeSet<String>) -> Vec<Range<usize>> {
    secrets
        .iter()
        .flat_map(|s| text.match_indices(s.as_str()))
        .map(|(i, m)| i..i + m.len())
        .collect()
}

/// Byte ranges of the non-empty matches of every pattern in `text`.
fn pattern_ranges(text: &str, patterns: &[Regex]) -> Vec<Range<usize>> {
    patterns
        .iter()
        .flat_map(|re| re.find_iter(text))
        .filter(|m| !m.is_empty())
        .map(|m| m.range())
        .collect()
}

/// `text` with each run of overlapping or touching `ranges` replaced by one
/// [`MASK`]. The ranges lie on character boundaries.
fn mask(text: &str, mut ranges: Vec<Range<usize>>) -> String {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = vec![];
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }

    let mut out = String::with_capacity(text.len());
    let mut kept_from = 0;
    for r in merged {
        out.push_str(&text[kept_from..r.start]);
        out.push_str(MASK);
        kept_from = r.end;
    }

    out.push_str(&text[kept_from..]);
    out
}

/// Whether a header or query parameter name is a credential's, judged by its
/// words (split at `-`, `_`, `.` and camelCase, ignoring case): one of them is
/// `token`, `secret`, `password`, `passwd`, `credential(s)`, `signature`,
/// `sig`, `session`, `cookie`, `auth`, `authorization` or `apikey`, or the
/// last word is `key` and the name isn't an idempotency key, or the last word
/// ends in `token`, `secret`, `password`, `passwd`, `signature` or `apikey`
/// (run-together names like `accesstoken`). So `accessToken`,
/// `oauth_signature`, `X-Amz-Signature` and `X-Api-Key` count, while
/// `keyword`, `author`, `monkey`, `X-Key-Id` and `Idempotency-Key` don't.
#[must_use]
pub fn is_secret_name(name: &str) -> bool {
    const WORDS: [&str; 13] = [
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "credentials",
        "signature",
        "sig",
        "session",
        "cookie",
        "auth",
        "authorization",
        "apikey",
    ];
    // Endings catch run-together names (`accesstoken`, `clientsecret`).
    const ENDINGS: [&str; 6] = [
        "token",
        "secret",
        "password",
        "passwd",
        "signature",
        "apikey",
    ];
    let words = name_words(name);

    words.iter().any(|w| WORDS.contains(&w.as_str()))
        || words
            .last()
            .is_some_and(|w| ENDINGS.iter().any(|e| w.ends_with(e)))
        || (words.last().is_some_and(|w| w == "key") && !words.iter().any(|w| w == "idempotency"))
}

/// The lowercase words of a name, split at non-alphanumeric characters and at
/// camelCase boundaries (`accessToken` → `access`, `token`).
fn name_words(name: &str) -> Vec<String> {
    let mut words = vec![];
    let mut current = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            words.extend((!current.is_empty()).then(|| std::mem::take(&mut current)));
            prev_lower = false;
            continue;
        }

        if c.is_ascii_uppercase() && prev_lower {
            words.push(std::mem::take(&mut current));
        }

        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        current.push(c.to_ascii_lowercase());
    }

    words.extend((!current.is_empty()).then_some(current));
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_credential_query_names_are_secret_params() {
        for n in [
            "access_token",
            "refresh_token",
            "id_token",
            "token",
            "api_key",
            "apikey",
            "key",
            "password",
            "client_secret",
            "secret",
            "sig",
            "Signature",
            "X-Api-Key",
            "accessToken",
            "clientSecret",
            "authToken",
            "userPassword",
            "oauth_signature",
            "X-Amz-Signature",
            "session",
            "Cookie",
            "credential",
            "Authorization",
            "X-Auth-Token",
            "accesstoken",
            "apitoken",
            "clientsecret",
            "x-apikey",
        ] {
            assert!(is_secret_name(n), "{n}");
        }

        for n in [
            "q",
            "keyword",
            "author",
            "monkey",
            "page",
            "tokens_left",
            "X-Idempotency-Key",
            "X-Key-Id",
            "oauth_nonce",
        ] {
            assert!(!is_secret_name(n), "{n}");
        }
    }

    #[test]
    fn masks_secrets_but_not_short_values() {
        let r = Redactor::default();
        r.secret("hunter2");
        r.secret("hunter2-extended");
        r.secret("pw");
        assert_eq!(
            r.redact("a hunter2-extended b hunter2 c pw"),
            "a *** b *** c pw"
        );
    }

    #[test]
    fn merges_overlapping_and_adjacent_matches() {
        let r = Redactor::default();
        r.secret("abcd");
        r.secret("cdef");
        assert_eq!(r.redact("x abcdef y abcdabcd z"), "x *** y *** z");
    }

    #[test]
    fn a_secret_formed_by_masking_is_masked_too() {
        let r = Redactor::default();
        r.secret("x***");
        r.secret("abcd");
        assert_eq!(r.redact("xabcd"), "***");
    }

    #[test]
    fn masks_header_credentials() {
        let r = Redactor::default();
        r.header_value("Bearer abc.def.ghi");
        assert_eq!(r.redact("token abc.def.ghi leaked"), "token *** leaked");
    }

    #[test]
    fn masks_patterns_and_rejects_bad_ones() {
        let r = Redactor::default();
        r.pattern(r"cust-\d+").unwrap();
        r.pattern("x*").unwrap();
        assert_eq!(r.redact("for cust-42 and cust-7"), "for *** and ***");
        assert!(r.pattern("(").is_err());
    }

    #[test]
    fn console_forgets_dropped_redactors() {
        let r = Redactor::default();
        r.secret("registry-secret-1");
        r.activate();
        r.activate();
        assert_eq!(console("x registry-secret-1"), "x ***");
        drop(r);
        assert_eq!(console("x registry-secret-1"), "x registry-secret-1");
    }

    #[test]
    fn debug_hides_values() {
        let r = Redactor::default();
        r.secret("supersecret");
        r.pattern("cust-[0-9]+").unwrap();
        let debug = format!("{r:?}");
        assert!(
            !debug.contains("supersecret") && !debug.contains("cust"),
            "{debug}"
        );
    }
}
