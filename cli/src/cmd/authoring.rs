//! Test-authoring helpers: turn the live screen into UI-automation scaffolding.
//!
//! These commands serve the developer who is *writing* instrumentation / Espresso
//! / Compose tests (not running them — ShadowDroid can't co-run with an
//! instrumentation test; see the UiAutomation slot note on `connect`). They read
//! the current screen and:
//!   - `ui audit` — flag interactive elements that lack a stable selector
//!     (resource-id / Compose `testTag`), so tests don't end up keyed on
//!     localized text or element index.
//!   - `ui gen` — emit a starting-point Screen Object with the stable selectors
//!     already filled in.
//!
//! The analysis is pure (`audit_elements`, `generate_screen_object`) so it is unit
//! tested without a device.

use crate::proto::Element;
use serde_json::{Value, json};

/// An element a test would realistically act on or assert against.
fn is_interactive(el: &Element) -> bool {
    el.clickable || el.long_clickable || el.input || el.checkable
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Selector quality for an interactive element, best first.
#[derive(Debug, PartialEq, Eq)]
pub enum SelectorQuality {
    /// Has a resource-id (or a Compose `testTag` surfaced as a resource-id via
    /// `testTagsAsResourceId = true`) — stable across copy/locale changes.
    Stable,
    /// No id, but has a content-description — usable, though still string-based.
    DescOnly,
    /// No id/desc, only visible text — brittle (localized, copy-driven).
    TextOnly,
    /// No id/desc/text — only matchable by index or bounds. Effectively
    /// un-targetable by a robust test.
    None,
}

pub fn selector_quality(el: &Element) -> SelectorQuality {
    if non_empty(&el.rid).is_some() {
        SelectorQuality::Stable
    } else if non_empty(&el.desc).is_some() {
        SelectorQuality::DescOnly
    } else if non_empty(&el.text).is_some() {
        SelectorQuality::TextOnly
    } else {
        SelectorQuality::None
    }
}

fn klass_short(klass: &Option<String>) -> Option<String> {
    non_empty(klass).map(|k| k.rsplit('.').next().unwrap_or(k).to_string())
}

fn describe(el: &Element, reason: &str) -> Value {
    json!({
        "id": el.id,
        "class": klass_short(&el.klass),
        "text": non_empty(&el.text),
        "desc": non_empty(&el.desc),
        "tap": el.tap,
        "reason": reason,
    })
}

/// Audit the interactive elements on a screen for selector stability. Returns the
/// analysis body (the caller adds `screen_hash` and emits it).
pub fn audit_elements(elements: &[Element]) -> Value {
    let mut interactive = 0usize;
    let mut stable = 0usize;
    let mut weak: Vec<Value> = Vec::new();
    let mut untargetable: Vec<Value> = Vec::new();

    for el in elements.iter().filter(|e| is_interactive(e)) {
        interactive += 1;
        match selector_quality(el) {
            SelectorQuality::Stable => stable += 1,
            SelectorQuality::DescOnly => {
                weak.push(describe(el, "matched only by content-description"))
            }
            SelectorQuality::TextOnly => weak.push(describe(
                el,
                "matched only by visible text (localized / copy-driven)",
            )),
            SelectorQuality::None => {
                untargetable.push(describe(el, "no resource-id, content-description, or text"))
            }
        }
    }

    let missing = weak.len() + untargetable.len();
    let recommendation = if missing == 0 {
        "Every interactive element on this screen has a stable selector.".to_string()
    } else {
        format!(
            "{missing} interactive element(s) lack a stable selector. Add a resource-id \
             (or a Compose `testTag` with `testTagsAsResourceId = true`) to each so tests \
             don't depend on localized text or element index."
        )
    };

    json!({
        "elements_total": elements.len(),
        "interactive_total": interactive,
        "stable": stable,
        "weak_count": weak.len(),
        "weak": weak,
        "untargetable_count": untargetable.len(),
        "untargetable": untargetable,
        "recommendation": recommendation,
    })
}

/// Local selector name from a resource-id: `com.app:id/login_button` ->
/// `login_button`; a bare Compose testTag is returned as-is.
fn rid_local(rid: &str) -> &str {
    rid.rsplit('/').next().unwrap_or(rid)
}

fn screaming_snake(name: &str) -> String {
    let mut out = String::new();
    let mut prev_underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
            prev_underscore = false;
        } else if !prev_underscore && !out.is_empty() {
            out.push('_');
            prev_underscore = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Generate a starting-point Kotlin Screen Object from the live screen. Stable
/// selectors (resource-ids / Compose testTags) become constants; interactive
/// elements without one are listed as TODOs so the author knows what to tag.
pub fn generate_screen_object(name: &str, elements: &[Element]) -> String {
    let mut tags: Vec<(String, String, Value)> = Vec::new(); // (const, value, label)
    let mut todos: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for el in elements.iter().filter(|e| is_interactive(e)) {
        match selector_quality(el) {
            SelectorQuality::Stable => {
                let rid = non_empty(&el.rid).unwrap();
                let local = rid_local(rid);
                let constant = screaming_snake(local);
                if constant.is_empty() || !seen.insert(constant.clone()) {
                    continue;
                }
                let label = non_empty(&el.text)
                    .or_else(|| non_empty(&el.desc))
                    .unwrap_or("");
                let klass = klass_short(&el.klass).unwrap_or_default();
                tags.push((
                    constant,
                    local.to_string(),
                    json!(format!("{label} ({klass})")),
                ));
            }
            _ => {
                let hint = non_empty(&el.text)
                    .or_else(|| non_empty(&el.desc))
                    .unwrap_or("<no text>");
                let klass = klass_short(&el.klass).unwrap_or_default();
                todos.push(format!("//   - \"{hint}\" ({klass})"));
            }
        }
    }

    let class_name = format!("{}Screen", name);
    let mut out = String::new();
    out.push_str(&format!(
        "// Generated by `shadowdroid ui gen` — a starting-point Screen Object.\n\
         // Stable selectors below are resource-ids from the live screen (Compose\n\
         // testTags appear here when the app sets testTagsAsResourceId = true).\n\
         // Wire them to your framework: Espresso onView(withId(...)) or Compose\n\
         // onNodeWithTag(...). Adjust the package and class name as needed.\n\n\
         class {class_name} {{\n"
    ));

    if tags.is_empty() {
        out.push_str("    // No stable selectors found on this screen.\n");
    } else {
        out.push_str("    object Tags {\n");
        for (constant, value, label) in &tags {
            let label = label.as_str().unwrap_or("");
            out.push_str(&format!(
                "        const val {constant} = \"{value}\"  // {label}\n"
            ));
        }
        out.push_str("    }\n");
    }

    if !todos.is_empty() {
        out.push_str(
            "\n    // Interactive elements without a stable selector — add a resource-id\n\
             \x20   // or Compose testTag in the app before relying on these:\n",
        );
        for todo in &todos {
            out.push_str(&format!("    {todo}\n"));
        }
    }

    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(rid: Option<&str>, text: Option<&str>, desc: Option<&str>, clickable: bool) -> Element {
        Element {
            id: 0,
            text: text.map(str::to_string),
            desc: desc.map(str::to_string),
            klass: Some("android.widget.Button".into()),
            rid: rid.map(str::to_string),
            bounds: None,
            tap: Some([1, 2]),
            clickable,
            long_clickable: false,
            scrollable: false,
            checkable: false,
            focusable: false,
            enabled: true,
            selected: false,
            checked: false,
            focused: false,
            password: false,
            input: false,
        }
    }

    #[test]
    fn classifies_selector_quality_best_first() {
        assert_eq!(
            selector_quality(&el(Some("app:id/go"), Some("Go"), Some("Go"), true)),
            SelectorQuality::Stable
        );
        assert_eq!(
            selector_quality(&el(None, Some("Go"), Some("Go button"), true)),
            SelectorQuality::DescOnly
        );
        assert_eq!(
            selector_quality(&el(None, Some("Go"), None, true)),
            SelectorQuality::TextOnly
        );
        assert_eq!(
            selector_quality(&el(None, None, None, true)),
            SelectorQuality::None
        );
        // Blank strings don't count as a selector.
        assert_eq!(
            selector_quality(&el(Some("  "), Some(""), None, true)),
            SelectorQuality::None
        );
    }

    #[test]
    fn audit_counts_and_flags_weak_and_untargetable() {
        let elements = vec![
            el(Some("app:id/login"), Some("Log in"), None, true), // stable
            el(None, Some("Sign up"), None, true),                // weak (text only)
            el(None, None, None, true),                           // untargetable
            el(None, Some("non-interactive label"), None, false), // ignored (not interactive)
        ];
        let v = audit_elements(&elements);
        assert_eq!(v["elements_total"], 4);
        assert_eq!(v["interactive_total"], 3);
        assert_eq!(v["stable"], 1);
        assert_eq!(v["weak_count"], 1);
        assert_eq!(v["untargetable_count"], 1);
        assert!(
            v["recommendation"]
                .as_str()
                .unwrap()
                .contains("stable selector")
        );
    }

    #[test]
    fn generates_screen_object_with_tags_and_todos() {
        let elements = vec![
            el(Some("com.app:id/login_button"), Some("Log in"), None, true),
            el(None, Some("Forgot password?"), None, true),
        ];
        let code = generate_screen_object("Login", &elements);
        assert!(code.contains("class LoginScreen"));
        assert!(
            code.contains("const val LOGIN_BUTTON = \"login_button\""),
            "{code}"
        );
        assert!(code.contains("Forgot password?"), "{code}");
    }
}
