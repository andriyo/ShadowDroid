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
//!
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
    if exact { a == e } else { a.contains(&e) }
}

/// Raised when an *action* selector (tap / focus / type-into-field) matches more
/// than one element and no single one matches exactly — the agent must narrow
/// with `--exact`, `--rid`, or `--clickable`. Carries the candidates it saw.
/// Surfaced by `cli::report_error` as `{"type":"error","code":"ambiguous_match",
/// "detail":{"candidates":[…]}}`. Mirrors the server's `ambiguous_match`.
#[derive(Debug, thiserror::Error)]
#[error("selector {query} matched {} elements; narrow with --exact, --rid, or --clickable", self.candidates.len())]
pub struct AmbiguousMatch {
    pub query: String,
    pub candidates: Vec<serde_json::Value>,
}

/// The `--text/--rid/--desc` selector flags, flattened into every verb that
/// targets an element by field. Verbs that require a single selector (`ui
/// focus`, `ui scroll-to`) validate with [`SelectorArgs::exactly_one`]; verbs
/// where the fields are independent optional filters (`layout source`) read
/// them directly.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct SelectorArgs {
    /// Match an element whose text contains this (substring, case-insensitive).
    #[arg(long)]
    pub text: Option<String>,
    /// Match by resource-id substring.
    #[arg(long)]
    pub rid: Option<String>,
    /// Match by content-description substring.
    #[arg(long)]
    pub desc: Option<String>,
}

impl SelectorArgs {
    pub fn exactly_one(&self) -> anyhow::Result<Selector> {
        match (&self.text, &self.rid, &self.desc) {
            (Some(t), None, None) => Ok(Selector::Text(t.clone())),
            (None, Some(r), None) => Ok(Selector::Rid(r.clone())),
            (None, None, Some(d)) => Ok(Selector::Desc(d.clone())),
            (None, None, None) => Err(crate::diagnostic::DiagnosticError::new(
                "selector_required",
                "input",
                "pass exactly one selector: --text, --rid, or --desc",
            )
            .detail(serde_json::json!({"provided": []}))
            .next_actions(["rerun the command with exactly one of --text, --rid, or --desc"])
            .into()),
            _ => {
                let provided = [
                    self.text.as_ref().map(|_| "text"),
                    self.rid.as_ref().map(|_| "rid"),
                    self.desc.as_ref().map(|_| "desc"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                Err(crate::diagnostic::DiagnosticError::new(
                    "selector_conflict",
                    "input",
                    "pass only one selector: --text, --rid, or --desc",
                )
                .detail(serde_json::json!({"provided": provided}))
                .next_actions([
                    "choose the most stable single selector (prefer --rid), remove the others, and retry",
                ])
                .into())
            }
        }
    }
}

/// One concrete field selector — the validated exactly-one form of
/// [`SelectorArgs`].
pub enum Selector {
    Text(String),
    Rid(String),
    Desc(String),
}

impl Selector {
    pub fn matches(&self, el: &crate::proto::Element, exact: bool) -> bool {
        let (field, query) = match self {
            Selector::Text(q) => (&el.text, q),
            Selector::Rid(q) => (&el.rid, q),
            Selector::Desc(q) => (&el.desc, q),
        };
        text_matches(field.as_deref(), Some(query), exact)
    }

    /// The `{"text": …}`-style object used in action output.
    pub fn label(&self) -> serde_json::Value {
        match self {
            Selector::Text(q) => serde_json::json!({ "text": q }),
            Selector::Rid(q) => serde_json::json!({ "rid": q }),
            Selector::Desc(q) => serde_json::json!({ "desc": q }),
        }
    }

    /// The equivalent on-device query for server-side find/tap routes.
    pub fn query(&self) -> crate::proto::SelectorQuery {
        match self {
            Selector::Text(q) => crate::proto::SelectorQuery {
                text: Some(q.clone()),
                ..Default::default()
            },
            Selector::Rid(q) => crate::proto::SelectorQuery {
                rid: Some(q.clone()),
                ..Default::default()
            },
            Selector::Desc(q) => crate::proto::SelectorQuery {
                desc: Some(q.clone()),
                ..Default::default()
            },
        }
    }

    /// How to spell this selector back on the command line (for hint text and
    /// `ambiguous_match` queries).
    pub fn describe(&self) -> String {
        match self {
            Selector::Text(q) => format!("--text {q:?}"),
            Selector::Rid(q) => format!("--rid {q:?}"),
            Selector::Desc(q) => format!("--desc {q:?}"),
        }
    }
}

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
