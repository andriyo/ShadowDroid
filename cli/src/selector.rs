//! Canonical text-selector matching, shared by every host-side matcher
//! (`ui wait`, `ui focus`, the `scroll-to` fallback loop, `layout source`, and
//! watcher rules). The on-device server's Kotlin `normalizeForMatch` /
//! `matchString` mirror this exact spec for `ui find`/`tap`/`text`/`scroll`, so
//! a selector behaves identically whether it's evaluated on the host or on the
//! device. Keep the two in lockstep — the cross-checked vectors in this file's
//! tests and the server's `ShadowDroidServerTest` guard the parity.
//!
//! ## Spec
//! Compare a *normalized* form of both the candidate's value and the query.
//! [`normalize`]:
//!   1. drops zero-width and bidirectional control characters (they hide in
//!      localized strings and silently break otherwise-equal text),
//!   2. folds typographic punctuation to ASCII — curly quotes/apostrophes/primes
//!      and the ellipsis — but **not** dashes (an en/em dash is semantically not
//!      a hyphen, so folding it would cause false matches),
//!   3. collapses every run of Unicode whitespace (NBSP, tabs, newlines, thin
//!      spaces, …) to one ASCII space and trims the ends.
//! Matching is then **case-insensitive**: a substring test by default, full
//! equality under `exact`.

/// Fold one string to its canonical match form. Idempotent.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for c in s.chars() {
        if is_zero_width_or_bidi(c) {
            continue;
        }
        if c.is_whitespace() {
            // Defer the space: emit at most one, and only before a subsequent
            // real character — so leading, trailing, and repeated whitespace
            // all collapse away.
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        match c {
            '\u{2018}' | '\u{2019}' | '\u{02BC}' | '\u{2032}' | '\u{201B}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{2033}' | '\u{201F}' => out.push('"'),
            '\u{2026}' => out.push_str("..."),
            _ => out.push(c),
        }
    }
    out
}

/// Zero-width and bidi control characters that are visually absent but break a
/// naive byte/char comparison. `char::is_whitespace` is *false* for these (they
/// lack the Unicode White_Space property), so they're handled separately.
fn is_zero_width_or_bidi(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}'   // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | '\u{202A}'..='\u{202E}' // bidi embeddings / overrides
        | '\u{2060}'              // word joiner
        | '\u{FEFF}'              // zero-width no-break space / BOM
    )
}

/// Match a selector value against an element's value. `expected = None` is a
/// wildcard (no constraint); a `None` actual never matches a concrete query.
/// Both sides are normalized, then compared case-insensitively — substring by
/// default, full equality under `exact`.
pub fn text_matches(actual: Option<&str>, expected: Option<&str>, exact: bool) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let Some(actual) = actual else {
        return false;
    };
    let a = normalize(actual).to_lowercase();
    let e = normalize(expected).to_lowercase();
    if exact {
        a == e
    } else {
        a.contains(&e)
    }
}

/// Raised when an *action* selector (tap / focus / type-into-field) matches more
/// than one element and no single one matches exactly — the agent must narrow
/// with `--exact`, `--rid`, or `--clickable`. Carries the candidates it saw.
/// Surfaced by `cli::report_error` as `{"type":"error","code":"ambiguous_match",
/// "detail":{"candidates":[…]}}`. Mirrors the server's `ambiguous_match`.
#[derive(Debug)]
pub struct AmbiguousMatch {
    pub query: String,
    pub candidates: Vec<serde_json::Value>,
}

impl std::fmt::Display for AmbiguousMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "selector {} matched {} elements; narrow with --exact, --rid, or --clickable",
            self.query,
            self.candidates.len()
        )
    }
}

impl std::error::Error for AmbiguousMatch {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_and_trims_unicode_whitespace() {
        // NBSP, doubled spaces, a newline, leading/trailing space → single spaces.
        assert_eq!(
            normalize("  Sign\u{00A0}\u{00A0}in \n now  "),
            "Sign in now"
        );
        assert_eq!(normalize("Done"), "Done");
    }

    #[test]
    fn normalize_folds_typographic_punctuation_but_not_dashes() {
        assert_eq!(normalize("Don\u{2019}t allow"), "Don't allow"); // curly apostrophe
        assert_eq!(normalize("\u{201C}Hi\u{201D}"), "\"Hi\""); // curly double quotes
        assert_eq!(normalize("Loading\u{2026}"), "Loading..."); // ellipsis
        assert_eq!(normalize("A\u{2013}B"), "A\u{2013}B"); // en dash preserved
    }

    #[test]
    fn normalize_strips_zero_width_and_bidi() {
        assert_eq!(normalize("ab\u{200B}c\u{FEFF}"), "abc");
        assert_eq!(normalize("\u{202A}left\u{202C}"), "left");
    }

    #[test]
    fn text_matches_treats_metacharacters_literally() {
        // Matching is literal substring/equality — NOT glob or regex. Every
        // special symbol matches only itself.
        assert!(text_matches(Some("3 * 4 = 12"), Some("3 * 4"), false)); // '*' is literal
        assert!(!text_matches(Some("Basket"), Some("Bask*t"), false)); // not a wildcard
        assert!(text_matches(Some("a.b.c"), Some("a.b"), false)); // '.' is literal
        assert!(!text_matches(Some("axbxc"), Some("a.b"), false)); // not "any char"
        assert!(text_matches(
            Some("Price: $5.00 (USD)"),
            Some("$5.00"),
            false
        )); // $ ( ) literal
        assert!(text_matches(Some("[Draft] Report"), Some("[draft]"), false)); // [] literal + ci
        assert!(text_matches(Some("a+b?c|d^e"), Some("+b?c|d^"), false)); // regex metas literal
        assert!(text_matches(Some("C:\\Users"), Some("\\users"), false)); // backslash literal
        assert!(text_matches(Some("100%"), Some("100%"), true)); // exact, literal '%'
    }

    #[test]
    fn text_matches_substring_is_case_insensitive_and_normalized() {
        // straight query matches curly-rendered text, case-folded, substring.
        assert!(text_matches(
            Some("Don\u{2019}t allow"),
            Some("don't"),
            false
        ));
        assert!(!text_matches(Some("Done"), Some("on"), true)); // exact rejects substring
        assert!(text_matches(Some("Done"), Some("on"), false)); // substring accepts
        assert!(text_matches(Some("Allow"), Some("allow"), true)); // exact, case-insensitive
        assert!(text_matches(None, None, false)); // wildcard
        assert!(!text_matches(None, Some("x"), false)); // no actual, concrete query
    }
}
