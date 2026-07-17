//! Loop fusion for the observe→act→observe cycle, plus self-explaining action
//! failures. The host composes the existing `/screen` pre-read with the
//! server's accessibility-event-backed `/screen/stable` observation route.
//!
//!   • `--if-screen <hash>` — optimistic concurrency for the UI: refuse to act
//!     when the screen no longer matches the hash the agent last read. The
//!     `screen_changed` error carries the fresh compact screen, so the failure
//!     *is* the re-observe.
//!   • `--observe` — wait for a real accessibility quiet period after delivery,
//!     then include the resulting compact screen in the same response.
//!   • `--expect-*` — make the post-action observation transactional: success
//!     requires a stable screen that satisfies the requested destination.
//!   • element_not_found enrichment — when a selector matches nothing, the
//!     error answers "so what IS on screen?": visible texts plus the
//!     closest-matching candidates, ranked.
//!
//! [`Outcome`] is the return currency of dispatch arms: build the result, let
//! the dispatcher attach cross-cutting extras (since-last-command events) and
//! emit exactly once.

use anyhow::Result;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

use crate::device::client::{ServerClient, ServerError};
use crate::events::{CompactElement, emit_action, emit_result};
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
                emit_result(&value);
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
    /// Required accessibility-event quiet period before the observed screen is
    /// considered stable, in milliseconds.
    #[arg(long, default_value_t = 500, value_name = "MS")]
    pub observe_delay_ms: u32,
    /// Overall deadline for stable observation and any --expect-* postcondition.
    #[arg(long = "timeout-ms", default_value_t = 3000, value_name = "MS")]
    pub observe_timeout_ms: u32,
    /// Require a stable destination containing this visible text.
    #[arg(long, value_name = "TEXT")]
    pub expect_text: Option<String>,
    /// Require a stable destination containing this content description.
    #[arg(long, value_name = "DESCRIPTION")]
    pub expect_desc: Option<String>,
    /// Require a stable destination containing this resource id.
    #[arg(long, value_name = "RESOURCE_ID")]
    pub expect_rid: Option<String>,
    /// Require this foreground package after the action.
    #[arg(long, value_name = "PACKAGE")]
    pub expect_package: Option<String>,
    /// Require this foreground activity after the action.
    #[arg(long, value_name = "ACTIVITY")]
    pub expect_activity: Option<String>,
    /// Match the --expect-* value exactly (case-insensitive) instead of as a
    /// substring.
    #[arg(long)]
    pub expect_exact: bool,
}

/// The screen no longer matches `--if-screen`. Carries the fresh screen so the
/// caller can re-plan without another read; rendered by `report_error` as
/// code=screen_changed with the compact screen in `detail`.
#[derive(Debug, thiserror::Error)]
#[error(
    "screen changed since your last read (expected hash {expected}, now {actual}) — not acting; re-plan from detail.screen"
)]
pub struct ScreenChanged {
    pub expected: String,
    pub actual: String,
    pub screen: Value,
}

/// The action was delivered, but its destination could not be proven stable
/// before the observation deadline. The freshest screen is diagnostic only:
/// callers must not reuse its ephemeral element ids for a subsequent action.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ObservationFailure {
    pub code: &'static str,
    pub message: String,
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Postcondition {
    Text(String),
    Desc(String),
    Rid(String),
    Package(String),
    Activity(String),
}

impl Postcondition {
    fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Desc(_) => "desc",
            Self::Rid(_) => "rid",
            Self::Package(_) => "package",
            Self::Activity(_) => "activity",
        }
    }

    fn expected(&self) -> &str {
        match self {
            Self::Text(value)
            | Self::Desc(value)
            | Self::Rid(value)
            | Self::Package(value)
            | Self::Activity(value) => value,
        }
    }

    fn evaluate(&self, screen: &ScreenResponse, exact: bool) -> Value {
        let expected = self.expected();
        let matching_element = match self {
            Self::Text(_) => screen.elements.iter().find(|element| {
                crate::selector::text_matches(element.text.as_deref(), Some(expected), exact)
            }),
            Self::Desc(_) => screen.elements.iter().find(|element| {
                crate::selector::text_matches(element.desc.as_deref(), Some(expected), exact)
            }),
            Self::Rid(_) => screen.elements.iter().find(|element| {
                crate::selector::text_matches(element.rid.as_deref(), Some(expected), exact)
            }),
            Self::Package(_) | Self::Activity(_) => None,
        };
        let actual = match self {
            Self::Package(_) => screen.current_app.package.as_deref(),
            Self::Activity(_) => screen.current_app.activity.as_deref(),
            Self::Text(_) | Self::Desc(_) | Self::Rid(_) => None,
        };
        let matched = matching_element.is_some()
            || actual.is_some_and(|value| {
                crate::selector::text_matches(Some(value), Some(expected), exact)
            });
        let mut report = json!({
            "kind": self.kind(),
            "expected": expected,
            "exact": exact,
            "matched": matched,
        });
        if let Some(element) = matching_element {
            report["element"] = json!(CompactElement::from(element.clone()));
        }
        if actual.is_some() {
            report["actual"] = json!(actual);
        }
        report
    }
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
///   4. accessibility-idle observation + destination postcondition on success.
pub async fn run_fused<F>(
    client: &ServerClient,
    fusion: &FusionArgs,
    hint: Option<SelectorHint>,
    act: F,
) -> Result<Outcome>
where
    F: std::future::Future<Output = Result<(&'static str, Value)>>,
{
    let postcondition = requested_postcondition(fusion)?;
    validate_observation_args(fusion, postcondition.is_some())?;
    let observe = fusion.observe || postcondition.is_some();
    let pre_screen = if fusion.if_screen.is_some() || observe {
        Some(client.screen().await?)
    } else {
        None
    };
    if let Some(expected) = &fusion.if_screen {
        let screen = pre_screen.as_ref().expect("pre-screen requested above");
        if &screen.screen_hash != expected {
            let actual = screen.screen_hash.clone();
            return Err(ScreenChanged {
                expected: expected.clone(),
                actual,
                screen: compact_screen_value(screen),
            }
            .into());
        }
    }

    let (cmd, mut body) = match act.await {
        Ok(result) => result,
        Err(err) => return Err(enrich_not_found(client, hint, err).await),
    };
    // Settlement begins only after the action endpoint confirms delivery;
    // selector resolution and injection latency are a separate phase.
    let action_delivered_at = Instant::now();

    if observe {
        let action_delivered = body
            .get("input_delivered")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        body["action_delivered"] = json!(action_delivered);
        let observation =
            observe_stable_destination(client, fusion, postcondition.as_ref(), action_delivered_at)
                .await;
        match observation {
            Ok(observation) => {
                if let Some(pre) = &pre_screen {
                    attach_screen_change(
                        &mut body,
                        &pre.screen_hash,
                        &observation.screen.screen_hash,
                    );
                }
                body["stable"] = json!(true);
                body["settle_ms"] = json!(observation.settle_ms);
                body["quiet_period_ms"] = json!(observation.quiet_period_ms);
                body["postcondition"] = observation.postcondition.unwrap_or(Value::Null);
                body["postcondition_satisfied"] = if postcondition.is_some() {
                    json!(true)
                } else {
                    Value::Null
                };
                body["screen"] = compact_screen_value(&observation.screen);
            }
            Err(mut failure) => {
                failure.detail["action_delivered"] = json!(action_delivered);
                failure.detail["action_result"] = body;
                if let Some(pre) = &pre_screen {
                    failure.detail["pre_screen_hash"] = json!(pre.screen_hash);
                    if let Some(post_hash) = failure
                        .detail
                        .get("screen")
                        .and_then(|screen| screen.get("screen_hash"))
                        .cloned()
                    {
                        failure.detail["post_screen_hash"] = post_hash.clone();
                        failure.detail["screen_changed"] =
                            json!(post_hash.as_str() != Some(pre.screen_hash.as_str()));
                    }
                }
                return Err(failure.into());
            }
        }
    }
    Ok(Outcome::Action(cmd, body))
}

struct StableObservation {
    screen: ScreenResponse,
    settle_ms: u64,
    quiet_period_ms: u64,
    postcondition: Option<Value>,
}

async fn observe_stable_destination(
    client: &ServerClient,
    fusion: &FusionArgs,
    postcondition: Option<&Postcondition>,
    started: Instant,
) -> std::result::Result<StableObservation, ObservationFailure> {
    let deadline = started + Duration::from_millis(fusion.observe_timeout_ms as u64);
    let mut latest_screen: Option<ScreenResponse> = None;
    let mut latest_report: Option<Value> = None;
    let mut latest_quiet_ms = fusion.observe_delay_ms as u64;
    let mut saw_stable_screen = false;
    let mut last_observe_error: Option<String> = None;

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .clamp(1, u32::MAX as u128) as u32;
        match client
            .stable_screen(fusion.observe_delay_ms, remaining_ms)
            .await
        {
            Ok(response) => {
                latest_quiet_ms = response.quiet_period_ms;
                let report = postcondition
                    .map(|condition| condition.evaluate(&response.screen, fusion.expect_exact));
                let condition_matched = report
                    .as_ref()
                    .is_none_or(|value| value["matched"].as_bool() == Some(true));
                saw_stable_screen |= response.stable;
                latest_report = report.clone();
                latest_screen = Some(response.screen.clone());
                if response.stable && condition_matched {
                    return Ok(StableObservation {
                        screen: response.screen,
                        settle_ms: started.elapsed().as_millis() as u64,
                        quiet_period_ms: response.quiet_period_ms,
                        postcondition: report,
                    });
                }
            }
            Err(error) => {
                last_observe_error = Some(error.to_string());
            }
        }
        // A stable source tree can precede a delayed destination. Keep polling
        // until the requested postcondition is proven, but avoid a hot loop if
        // UiAutomation already considered the device idle on entry.
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let code = if postcondition.is_some() && saw_stable_screen {
        "postcondition_timeout"
    } else {
        "observation_unstable"
    };
    let message = if code == "postcondition_timeout" {
        format!(
            "action was delivered, but the {} postcondition was not satisfied on a stable screen within {} ms",
            postcondition.expect("code requires a postcondition").kind(),
            fusion.observe_timeout_ms,
        )
    } else {
        format!(
            "action was delivered, but no stable post-action screen was observed within {} ms",
            fusion.observe_timeout_ms,
        )
    };
    let mut detail = json!({
        "stable": false,
        // The stability deadline is authoritative even when serializing the
        // freshest diagnostic tree takes additional time after it expires.
        "settle_ms": (started.elapsed().as_millis() as u64)
            .min(fusion.observe_timeout_ms as u64),
        "quiet_period_ms": latest_quiet_ms,
        "postcondition": latest_report,
    });
    if let Some(screen) = latest_screen {
        detail["screen"] = compact_screen_value(&screen);
        detail["stable"] = json!(saw_stable_screen);
    }
    if let Some(error) = last_observe_error {
        detail["observe_error"] = json!(error);
    }
    Err(ObservationFailure {
        code,
        message,
        detail,
    })
}

fn requested_postcondition(fusion: &FusionArgs) -> Result<Option<Postcondition>> {
    let conditions = [
        fusion.expect_text.clone().map(Postcondition::Text),
        fusion.expect_desc.clone().map(Postcondition::Desc),
        fusion.expect_rid.clone().map(Postcondition::Rid),
        fusion.expect_package.clone().map(Postcondition::Package),
        fusion.expect_activity.clone().map(Postcondition::Activity),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    match conditions.as_slice() {
        [] if fusion.expect_exact => Err(crate::diagnostic::DiagnosticError::new(
            "postcondition_required",
            "input",
            "--expect-exact requires exactly one --expect-* postcondition",
        )
        .next_actions(["add one of --expect-text, --expect-desc, --expect-rid, --expect-package, or --expect-activity"])
        .into()),
        [] => Ok(None),
        [condition] => Ok(Some(condition.clone())),
        _ => Err(crate::diagnostic::DiagnosticError::new(
            "postcondition_conflict",
            "input",
            "pass only one --expect-* postcondition",
        )
        .detail(json!({
            "provided": conditions.iter().map(Postcondition::kind).collect::<Vec<_>>(),
        }))
        .next_actions(["choose the single destination condition that best proves the action completed, then retry"])
        .into()),
    }
}

fn validate_observation_args(fusion: &FusionArgs, has_postcondition: bool) -> Result<()> {
    let observation_requested = fusion.observe || has_postcondition;
    if !observation_requested {
        return Ok(());
    }
    if fusion.observe_delay_ms == 0
        || fusion.observe_timeout_ms == 0
        || fusion.observe_timeout_ms < fusion.observe_delay_ms
    {
        return Err(crate::diagnostic::DiagnosticError::new(
            "invalid_observation_timing",
            "input",
            "observation timing requires 0 < --observe-delay-ms <= --timeout-ms",
        )
        .detail(json!({
            "observe_delay_ms": fusion.observe_delay_ms,
            "timeout_ms": fusion.observe_timeout_ms,
        }))
        .next_actions(["increase --timeout-ms or reduce --observe-delay-ms, then retry"])
        .into());
    }
    Ok(())
}

fn attach_screen_change(body: &mut Value, pre_hash: &str, post_hash: &str) {
    body["pre_screen_hash"] = json!(pre_hash);
    body["post_screen_hash"] = json!(post_hash);
    body["screen_changed"] = json!(pre_hash != post_hash);
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
        "screen_hash_version": screen.screen_hash_version,
        "snapshot_state": screen.snapshot_state,
        "captured_at_ms": screen.captured_at_ms,
        "viewport": screen.viewport,
        "current_app": screen.current_app,
        "ui_tree": screen.ui_tree,
        "warning": screen.warning,
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
        if let Some(t) = el.text.as_deref().map(str::trim)
            && !t.is_empty()
            && !out.iter().any(|x| x == t)
        {
            out.push(t.to_string());
            if out.len() >= n {
                break;
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
            "screen_hash_version".into(),
            json!(screen.screen_hash_version),
        );
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
            range: None,
            actions: Vec::new(),
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
    fn compact_screen_preserves_snapshot_freshness_metadata() {
        let screen = ScreenResponse {
            screen_hash: "screen-1".into(),
            screen_hash_version: 2,
            snapshot_state: "transitioning".into(),
            captured_at_ms: Some(123),
            viewport: crate::proto::Viewport { w: 1080, h: 1920 },
            current_app: crate::proto::AppRef {
                package: Some("com.example".into()),
                activity: None,
                pid: None,
                sampled_at_ms: Some(120),
            },
            ui_tree: Some(crate::proto::UiTreeSnapshot {
                sampled_at_ms: 121,
                age_ms: 2,
                package: Some("com.example".into()),
                window_id: Some(7),
            }),
            warning: Some("still converging".into()),
            element_count: 1,
            ime: crate::proto::ImeState::default(),
            elements: vec![el(1, "Loading")],
        };

        let compact = compact_screen_value(&screen);
        assert_eq!(compact["snapshot_state"], "transitioning");
        assert_eq!(compact["captured_at_ms"], 123);
        assert_eq!(compact["current_app"]["sampled_at_ms"], 120);
        assert_eq!(compact["ui_tree"]["window_id"], 7);
        assert_eq!(compact["warning"], "still converging");
    }

    #[test]
    fn observed_outcome_distinguishes_noop_from_screen_change() {
        let mut noop = json!({});
        attach_screen_change(&mut noop, "same", "same");
        assert_eq!(noop["screen_changed"], false);
        assert_eq!(noop["pre_screen_hash"], "same");
        assert_eq!(noop["post_screen_hash"], "same");

        let mut changed = json!({});
        attach_screen_change(&mut changed, "before", "after");
        assert_eq!(changed["screen_changed"], true);
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

    fn postcondition_screen() -> ScreenResponse {
        ScreenResponse {
            screen_hash: "destination".into(),
            screen_hash_version: 3,
            snapshot_state: "consistent".into(),
            captured_at_ms: Some(500),
            viewport: crate::proto::Viewport { w: 1080, h: 1920 },
            current_app: crate::proto::AppRef {
                package: Some("io.example.app".into()),
                activity: Some("io.example.app.DetailActivity".into()),
                pid: Some(42),
                sampled_at_ms: Some(499),
            },
            ui_tree: None,
            warning: None,
            element_count: 1,
            ime: crate::proto::ImeState::default(),
            elements: vec![Element {
                rid: Some("io.example.app:id/detail_message".into()),
                desc: Some("Detail destination message".into()),
                ..el(8, "Destination ready")
            }],
        }
    }

    #[test]
    fn destination_postconditions_share_canonical_matching() {
        let screen = postcondition_screen();
        for condition in [
            Postcondition::Text("destination ready".into()),
            Postcondition::Desc("destination message".into()),
            Postcondition::Rid("id/detail_message".into()),
            Postcondition::Package("io.example".into()),
            Postcondition::Activity("DetailActivity".into()),
        ] {
            let report = condition.evaluate(&screen, false);
            assert_eq!(report["matched"], true, "{condition:?}: {report}");
        }
        assert_eq!(
            Postcondition::Text("Destination".into()).evaluate(&screen, true)["matched"],
            false,
        );
    }

    #[test]
    fn postcondition_conflicts_and_invalid_timing_are_typed_input_errors() {
        let conflict = FusionArgs {
            expect_text: Some("Ready".into()),
            expect_activity: Some("Detail".into()),
            ..FusionArgs::default()
        };
        let error = requested_postcondition(&conflict).unwrap_err();
        assert_eq!(crate::cli::error_code_of(&error), "postcondition_conflict");

        let invalid_timing = FusionArgs {
            observe: true,
            observe_delay_ms: 500,
            observe_timeout_ms: 499,
            ..FusionArgs::default()
        };
        let error = validate_observation_args(&invalid_timing, false).unwrap_err();
        assert_eq!(
            crate::cli::error_code_of(&error),
            "invalid_observation_timing"
        );
    }

    #[test]
    fn observation_failures_keep_machine_identity_out_of_generic_fallback() {
        for code in ["observation_unstable", "postcondition_timeout"] {
            let error = anyhow::Error::new(ObservationFailure {
                code,
                message: "destination not proven".into(),
                detail: json!({"screen": {"elements": [{"id": 8}]}}),
            });
            assert_eq!(crate::cli::error_code_of(&error), code);
            assert_eq!(crate::cli::error_stage_of(&error), "observe");
            assert!(crate::cli::error_retryable_of(&error));
            assert!(!crate::cli::error_uses_fallback(&error));
        }
    }
}
