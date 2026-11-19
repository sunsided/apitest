//! Test data: values unique per run, and links pulled out of e-mails or pages.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};

/// Sequence number for [`unique`]; shared by all threads of the process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A short hex tag (at most 6 digits) identifying this process, computed once.
///
/// Mixes the start time with the process id, so two runs started at the same
/// time on one machine still get different tags.
fn run_tag() -> &'static str {
    static TAG: OnceLock<String> = OnceLock::new();
    TAG.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();

        format!(
            "{:x}",
            (nanos ^ (u128::from(std::process::id()) << 64)) & 0xff_ffff
        )
    })
}

/// A value unique across this run and concurrent runs: `prefix-<run>-<n>`.
#[must_use]
pub fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        run_tag(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A unique e-mail address on the reserved `.test` TLD.
#[must_use]
pub fn unique_email(prefix: &str) -> String {
    format!("{}@example.test", unique(prefix))
}

/// All `http(s)://` links in a text (plain text, HTML or JSON-escaped).
///
/// A link ends at whitespace, a quote, `<`, `>`, `)`, `]` or a backslash;
/// trailing `.`, `,` and `;` are dropped and `&amp;` is decoded.
#[must_use]
pub fn links(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = text;
    while let Some(start) = next_link_start(rest) {
        let tail = &rest[start..];
        let end = tail.find(ends_link).unwrap_or(tail.len());
        out.push(
            tail[..end]
                .trim_end_matches(['.', ',', ';'])
                .replace("&amp;", "&"),
        );
        rest = &tail[end..];
    }

    out
}

/// Byte offset of the first `http://` or `https://` in `text`.
fn next_link_start(text: &str) -> Option<usize> {
    match (text.find("http://"), text.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Whether `c` terminates a link in running text or markup.
fn ends_link(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '\\')
}

/// The first link in `text` containing `needle`.
///
/// # Errors
///
/// Fails when no link contains `needle`; the error shows the first 300
/// characters of `text`.
pub fn link_containing(text: &str, needle: &str) -> Result<String> {
    links(text)
        .into_iter()
        .find(|l| l.contains(needle))
        .ok_or_else(|| {
            anyhow!(
                "no link containing '{needle}' in: {}",
                text.chars().take(300).collect::<String>()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_extraction() {
        let mail = r#"<p>Hi! <a href="http://127.0.0.1:4000/confirm?token=abc&amp;x=1">Confirm</a>.</p> Or visit https://example.test/help."#;
        assert_eq!(
            links(mail),
            vec![
                "http://127.0.0.1:4000/confirm?token=abc&x=1",
                "https://example.test/help"
            ]
        );
        assert_eq!(
            link_containing(mail, "/confirm").unwrap(),
            "http://127.0.0.1:4000/confirm?token=abc&x=1"
        );
        assert!(link_containing(mail, "/nope").is_err());
    }

    #[test]
    fn unique_values_differ() {
        assert_ne!(unique("a"), unique("a"));
        assert!(unique_email("p").ends_with("@example.test"));
    }
}
