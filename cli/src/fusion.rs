//! Loop fusion for the observe→act→observe cycle, plus self-explaining action
//! failures. Everything here is host-side — it composes the existing `/screen`
//! read (~25 ms) around actions, with no server change.
//!
//!   • `--if-screen <hash>` — optimistic concurrency for the UI: refuse to act
//!     when the screen no longer matches the hash the agent last read. The
//!     `screen_changed` error carries the fresh compact screen, so the failure
//!     *is* the re-observe.
//!   • `--observe` — after the action settles, include the resulting compact
//!     screen in the same response, collapsing act + re-dump into one call.
//!   • element_not_found enrichment — when a selector matches nothing, the
//!     error answers "so what IS on screen?": visible texts plus the
//!     closest-matching candidates, ranked.
//!
//! [`Outcome`] is the return currency of dispatch arms: build the result, let
//! the dispatcher attach cross-cutting extras (since-last-command events) and
//! emit exactly once.

use anyhow::Result;
use serde_json::{json, Value};

use crate::device::client::{ServerClient, ServerError};
use crate::events::{emit, emit_action, CompactElement};
use crate::proto::{Element, ScreenResponse};

/// What a dispatch arm produced. Emission happens centrally so cross-cutting
/// keys (`events`, `screen`) attach in one place.
pub enum Outcome {
    /// `{"type":"action","cmd":<0>, …<1>}`.
    Action(&'static str, Value),
    /// A raw top-level object (e.g. the `ui dump` payload).
    Raw(Value),
    /// The arm already wrote its own output (non-JSON, e.g. `ui gen`).
    Done,
}

impl Outcome {
    pub fn emit(self, events: Vec<Value>) {
        crate::events::stash_events(events);
        match self {
            Outcome::Action(cmd, body) => emit_action(cmd, &body),
            Outcome::Raw(mut value) => {
                crate::events::attach_events_to(&mut value);
                emit(&value);
            }
            Outcome::Done => {}
        }
    }
}

/// The act+observe fusion flags, flattened into every UI action verb.
#[derive(Debug, Default, clap::Args)]
pub struct FusionArgs {
    /// Only act if the screen still matches this screen_hash (from your last
    /// `ui dump`/`ui wait`). On mismatch the command fails with
    /// code=screen_changed and the fresh compact screen in `detail` — use that
    /// instead of re-dumping.
    #[arg(long, value_name = "HASH")]
    pub if_screen: Option<String>,
    /// Include the post-action compact screen in this same response (saves the
    /// follow-up `ui dump` round-trip).
    #[arg(long)]
    pub observe: bool,
    /// Settle delay before the --observe dump, in milliseconds.
    #[arg(long, default_value_t = 150, value_name = "MS")]
    pub observe_delay_ms: u32,
}

/// The screen no longer matches `--if-screen`. Carries the fresh screen so the
/// caller can re-plan without another read; rendered by `report_error` as
/// code=screen_changed with the compact screen in `detail`.
#[derive(Debug, thiserror::Error)]
#[error("screen changed since your last read (expected hash {expected}, now {actual}) — not acting; re-plan from detail.screen")]
pub struct ScreenChanged {
    pub expected: String,
    pub actual: String,
    pub screen: Value,
}

/// A selector hint for element_not_found enrichment: the value the caller was
/// looking for, so "closest on-screen candidates" can be ranked against it.
#[derive(Debug, Default, Clone)]
pub struct SelectorHint {
    pub text: Option<String>,
    pub rid: Option<String>,
    pub desc: Option<String>,
}

impl SelectorHint {
    pub fn wanted(&self) -> Option<&str> {
        self.text
            .as_deref()
            .or(self.desc.as_deref())
            .or(self.rid.as_deref())
    }
}

/// Run one action with the fusion contract:
///   1. `--if-screen` precondition (fresh screen on mismatch),
///   2. the action itself (a lazy future producing `(cmd, body)`),
///   3. element_not_found enrichment on failure,
///   4. `--observe` post-state attach on success.
pub async fn run_fused<F>(
    client: &ServerClient,
    fusion: &FusionArgs,
    hint: Option<SelectorHint>,
    act: F,
) -> Result<Outcome>
where
    F: std::future::Future<Output = Result<(&'static str, Value)>>,
{
    if let Some(expected) = &fusion.if_screen {
        let screen = client.screen().await?;
        if &screen.screen_hash != expected {
            let actual = screen.screen_hash.clone();
            return Err(ScreenChanged {
                expected: expected.clone(),
                actual,
                screen: compact_screen_value(&screen),
            }
            .into());
        }
    }

    let (cmd, mut body) = match act.await {
        Ok(result) => result,
        Err(err) => return Err(enrich_not_found(client, hint, err).await),
    };

    if fusion.observe {
        tokio::time::sleep(std::time::Duration::from_millis(
            fusion.observe_delay_ms as u64,
        ))
        .await;
        // The action already succeeded — a failed observe dump must not turn
        // the response into an error. Report it as a field instead.
        match client.screen().await {
            Ok(screen) => {
                body["screen"] = compact_screen_value(&screen);
            }
            Err(err) => {
                body["observe_error"] = json!(err.to_string());
            }
        }
    }
    Ok(Outcome::Action(cmd, body))
}

/// The compact screen payload — same shape as `ui dump`'s default output.
pub fn compact_screen_value(screen: &ScreenResponse) -> Value {
    let elements: Vec<CompactElement> = screen
        .elements
        .iter()
        .cloned()
        .map(CompactElement::from)
        .collect();
    json!({
        "screen_hash": screen.screen_hash,
        "viewport": screen.viewport,
        "current_app": screen.current_app,
        "element_count": screen.element_count,
        "ime": crate::events::CompactIme::from(screen.ime.clone()),
        "elements": elements,
    })
}

/// Up to `n` distinct, non-empty visible texts in document order — the "what
/// IS on screen" answer attached to timeouts, screen_changed, and not-found
/// errors.
pub fn top_screen_texts(elements: &[Element], n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for el in elements {
        if let Some(t) = el.text.as_deref().map(str::trim) {
            if !t.is_empty() && !out.iter().any(|x| x == t) {
                out.push(t.to_string());
                if out.len() >= n {
                    break;
                }
            }
        }
    }
    out
}

/// If `err` is the server's element_not_found, answer "so what IS there?" in
/// the same error: one screen read → visible texts + closest candidates ranked
/// against what the caller searched for. Any enrichment failure returns the
/// original error untouched.
async fn enrich_not_found(
    client: &ServerClient,
    hint: Option<SelectorHint>,
    err: anyhow::Error,
) -> anyhow::Error {
    let Some(hint) = hint else { return err };
    let is_not_found = err
        .chain()
        .find_map(|e| e.downcast_ref::<ServerError>())
        .is_some_and(|se| se.code == "element_not_found");
    if !is_not_found {
        return err;
    }
    let Ok(screen) = client.screen().await else {
        return err;
    };
    let se = err
        .chain()
        .find_map(|e| e.downcast_ref::<ServerError>())
        .expect("checked above");

    let mut detail = se.detail.clone().unwrap_or_else(|| json!({}));
    if let Some(obj) = detail.as_object_mut() {
        obj.insert(
            "top_texts".into(),
            json!(top_screen_texts(&screen.elements, 12)),
        );
        if let Some(wanted) = hint.wanted() {
            let closest = closest_elements(wanted, &screen.elements, 3);
            if !closest.is_empty() {
                obj.insert("closest".into(), json!(closest));
            }
        }
        obj.insert("current_app".into(), json!(screen.current_app));
        obj.insert("screen_hash".into(), json!(screen.screen_hash));
        obj.insert(
            "hint".into(),
            json!(
                "selector matched nothing on the current screen; `closest` ranks similar \
                 candidates, `top_texts` shows what IS visible — the screen may not be the \
                 one you expect (`ui wait` for it), or the wording may differ"
            ),
        );
    }
    ServerError {
        status: se.status,
        code: se.code.clone(),
        message: se.message.clone(),
        detail: Some(detail),
    }
    .into()
}

/// Rank on-screen elements by similarity of their text/desc/rid to `wanted`;
/// return the top `n` as compact JSON with a `score` in (0, 1].
fn closest_elements(wanted: &str, elements: &[Element], n: usize) -> Vec<Value> {
    let mut scored: Vec<(f64, &Element)> = elements
        .iter()
        .filter_map(|el| {
            let best = [el.text.as_deref(), el.desc.as_deref(), el.rid.as_deref()]
                .into_iter()
                .flatten()
                .map(|s| similarity(wanted, s))
                .fold(0.0f64, f64::max);
            (best > 0.15).then_some((best, el))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored
        .into_iter()
        .take(n)
        .map(|(score, el)| {
            let mut v = json!(CompactElement::from(el.clone()));
            v["score"] = json!((score * 100.0).round() / 100.0);
            v
        })
        .collect()
}

/// Text similarity in [0, 1]: containment of the normalized needle scores
/// high; otherwise character-bigram Dice coefficient. Normalization reuses the
/// canonical selector rules so "closest" agrees with how matching works.
pub fn similarity(wanted: &str, actual: &str) -> f64 {
    let w = crate::selector::normalize(wanted).to_lowercase();
    let a = crate::selector::normalize(actual).to_lowercase();
    if w.is_empty() || a.is_empty() {
        return 0.0;
    }
    if a == w {
        return 1.0;
    }
    // The on-screen text containing the whole query ("Sign in to continue" for
    // "Sign in") is stronger evidence than the query merely containing an
    // on-screen fragment ("Sign") — an agent almost always meant the superset.
    if a.contains(&w) {
        return 0.7 + 0.3 * (w.len() as f64 / a.len() as f64);
    }
    if w.contains(&a) {
        return 0.5 + 0.3 * (a.len() as f64 / w.len() as f64);
    }
    let wb = bigrams(&w);
    let ab = bigrams(&a);
    if wb.is_empty() || ab.is_empty() {
        return 0.0;
    }
    let mut overlap = 0usize;
    let mut ab = ab;
    for g in &wb {
        if let Some(pos) = ab.iter().position(|x| x == g) {
            ab.swap_remove(pos);
            overlap += 1;
        }
    }
    (2.0 * overlap as f64) / (wb.len() + ab.len() + overlap) as f64
}

fn bigrams(s: &str) -> Vec<[char; 2]> {
    let chars: Vec<char> = s.chars().collect();
    chars.windows(2).map(|w| [w[0], w[1]]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(id: u32, text: &str) -> Element {
        Element {
            id,
            text: Some(text.into()),
            desc: None,
            klass: None,
            rid: None,
            bounds: None,
            tap: Some([1, 1]),
            clickable: true,
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
    fn similarity_orders_sensibly() {
        // Exact > containment > fuzzy > unrelated.
        let exact = similarity("Sign in", "Sign in");
        let contained = similarity("Sign in", "Sign in to continue");
        let fuzzy = similarity("Sign in", "Signing");
        let unrelated = similarity("Sign in", "Weather");
        assert_eq!(exact, 1.0);
        assert!(contained > fuzzy, "{contained} vs {fuzzy}");
        assert!(fuzzy > unrelated, "{fuzzy} vs {unrelated}");
        assert!(unrelated < 0.2, "{unrelated}");
    }

    #[test]
    fn similarity_normalizes_before_comparing() {
        // Curly apostrophe + case + whitespace fold away.
        assert_eq!(similarity("don't", "  Don’t "), 1.0);
    }

    #[test]
    fn closest_ranks_and_caps() {
        let elements = vec![
            el(1, "Sign in to continue"),
            el(2, "Weather"),
            el(3, "Sign"),
            el(4, "Settings"),
        ];
        let out = closest_elements("Sign in", &elements, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["text"], "Sign in to continue");
        assert_eq!(out[1]["text"], "Sign");
        assert!(out[0]["score"].as_f64().unwrap() >= out[1]["score"].as_f64().unwrap());
    }

    #[test]
    fn top_texts_dedupes_and_caps() {
        let elements = vec![el(1, "A"), el(2, "A"), el(3, "B"), el(4, " "), el(5, "C")];
        assert_eq!(top_screen_texts(&elements, 2), vec!["A", "B"]);
    }

    #[test]
    fn screen_changed_error_reads_well() {
        let e = ScreenChanged {
            expected: "abc".into(),
            actual: "def".into(),
            screen: json!({}),
        };
        let msg = e.to_string();
        assert!(msg.contains("abc") && msg.contains("def"), "{msg}");
    }
}
