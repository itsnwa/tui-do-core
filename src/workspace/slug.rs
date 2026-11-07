//! Slugs name workspace files (`tasks/<slug>.md`).

use super::store::{Dir, Store};
use super::{Error, Result};
use crate::format::js;

/// The app's `slugify`: lowercase, drop everything but ASCII letters, digits,
/// `_`, `-` and whitespace, then join words with `-`. Non-ASCII letters are
/// dropped (`"æøå task"` → `"task"`), as in the app.
pub fn slugify(text: &str) -> String {
    let lower = text.to_lowercase();
    let kept: String = js::trim(&lower)
        .chars()
        .filter(|&c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || js::is_js_space(c))
        .collect();
    let mut out = String::with_capacity(kept.len());
    let mut pending_dash = false;
    for c in kept.chars() {
        if js::is_js_space(c) || c == '_' || c == '-' {
            pending_dash = true;
        } else {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        }
    }
    out
}

/// Reject slugs that would escape their directory or hide the file.
pub fn validate(slug: &str) -> Result<()> {
    let ok = !slug.is_empty()
        && slug.len() <= 255
        && !slug.starts_with('.')
        && !slug.contains(['/', '\\', '\0']);
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("invalid slug {slug:?}")))
    }
}

/// `base`, or `base-2`, `base-3`, … if that name is taken.
pub fn unique(store: &Store, dir: Dir, base: &str) -> String {
    let base = if base.is_empty() { "untitled" } else { base };
    if !store.exists(dir, base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !store.exists(dir, candidate))
        .unwrap_or_else(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_should_match_the_app() {
        assert_eq!(
            slugify("TUI-DO Core: background daemon"),
            "tui-do-core-background-daemon"
        );
        assert_eq!(slugify("  Foo!  "), "foo");
        assert_eq!(slugify("æøå task"), "task");
        assert_eq!(slugify("a__b -- c"), "a-b-c");
    }

    #[test]
    fn validate_should_reject_traversal() {
        assert!(validate("../x").is_err());
        assert!(validate(".hidden").is_err());
        assert!(validate("2026-01-12T00:00:00.000Z").is_ok());
    }
}
