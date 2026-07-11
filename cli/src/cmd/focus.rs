//! `ui focus` — move D-pad focus to an element matching a selector, then
//! optionally activate it (DPAD_CENTER). The TV analog of `ui tap` / `scroll-to`.
//!
//! On leanback / Android TV the UI is focus + D-pad driven: a coordinate or
//! selector tap doesn't move the highlight, so an agent has to walk focus with
//! the remote. This composes the existing primitives — `screen` (whose elements
//! carry the `focused` flag) and the `/key` route's `dpad_*` presses — so it
//! ships without any server change.
//!
//! Algorithm (host loop, one dump per step):
//!   1. dump → resolve the target element (selector) and the currently focused one.
//!   2. target already focused → done (press `dpad_center` if `--center`).
//!   3. else step the D-pad along the dominant axis from the focused element's
//!      center toward the target's center, re-dump, repeat.
//!
//! Bounded by `--max-steps` and a no-progress guard (focus didn't move after a
//! press → bail rather than loop).

use std::time::Duration;

use anyhow::Result;

use crate::device::client::ServerClient;
use crate::fusion::Outcome;
use crate::proto::Element;
use crate::selector::{Selector, SelectorArgs};

/// Pause after a D-pad press so the (often animated) focus move lands before we
/// re-dump. Compose-TV focus transitions both animate and drop key events sent
/// mid-transition, so pace presses generously rather than racing them.
const SETTLE_MS: u64 = 400;
/// Tolerate this many consecutive "focus didn't move" dumps before giving up,
/// to absorb a dump that races a focus animation or a key event dropped during
/// one (re-pressing the same direction on the next pass recovers).
const MAX_STALLS: u32 = 4;

#[derive(clap::Args)]
pub struct FocusArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    /// Press DPAD_CENTER once the target is focused (activate it).
    #[arg(long)]
    pub center: bool,
    /// Match the selector value exactly instead of as a case-insensitive
    /// substring. Use it to disambiguate when several elements match.
    #[arg(long)]
    pub exact: bool,
    /// Maximum D-pad presses before giving up.
    #[arg(long, default_value_t = 30)]
    pub max_steps: u32,
}

/// Resolve the selector to the single element to drive focus toward. Sole match
/// wins; if several match, a unique *exact* match disambiguates; otherwise the
/// selector is ambiguous and the agent must narrow it (consistent with `ui tap`).
fn resolve_target(
    selector: &Selector,
    elements: &[Element],
    exact: bool,
) -> Result<Option<Element>, crate::selector::AmbiguousMatch> {
    let cands: Vec<&Element> = elements
        .iter()
        .filter(|e| selector.matches(e, exact))
        .collect();
    match cands.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some((*one).clone())),
        many => {
            let exacts: Vec<&Element> = many
                .iter()
                .copied()
                .filter(|e| selector.matches(e, true))
                .collect();
            match exacts.as_slice() {
                [one] => Ok(Some((*one).clone())),
                _ => Err(crate::selector::AmbiguousMatch {
                    query: selector.describe(),
                    candidates: many.iter().map(|e| candidate_json(e)).collect(),
                }),
            }
        }
    }
}

fn candidate_json(e: &Element) -> serde_json::Value {
    serde_json::json!({ "id": e.id, "text": e.text, "rid": e.rid, "desc": e.desc, "tap": e.tap })
}

pub async fn run(client: &ServerClient, args: &FocusArgs) -> Result<Outcome> {
    let selector = args.selector.exactly_one()?;

    let mut steps = 0u32;
    let mut stalls = 0u32;
    // The effective-focused element's identity from the previous iteration, used
    // to detect a press that didn't move focus (dead end / non-navigable layout).
    let mut prev_focused: Option<String> = None;

    loop {
        let screen = client.screen().await?;

        let target = match resolve_target(&selector, &screen.elements, args.exact)? {
            Some(t) => t,
            None => {
                return focus_failure(&selector, steps, "not_found", &screen, None);
            }
        };

        // The element that currently has D-pad focus. Many Compose/TV apps put the
        // `focused` flag on a bounds-less wrapper whose visible content is the next
        // node in DFS order, so resolve through to that child for both arrival
        // detection and geometry.
        let current = effective_focused(&screen.elements);

        // Arrived when the focused content is the selector target.
        if current.is_some_and(|e| selector.matches(e, args.exact)) {
            let activated = if args.center {
                client.key("dpad_center").await?;
                true
            } else {
                false
            };
            return emit(&selector, true, steps, "focused", Some(&target), activated);
        }

        let current_key = current.map(focus_key);

        // A press that left focus unchanged (after the settle delay) means we
        // likely can't reach the target by stepping the D-pad — off-axis target,
        // non-focusable, or a focus trap. Tolerate a couple of stalls first to
        // absorb a dump that raced a focus animation.
        if steps > 0 && current_key == prev_focused {
            stalls += 1;
            if stalls >= MAX_STALLS {
                return focus_failure(&selector, steps, "no_progress", &screen, Some(&target));
            }
        } else {
            stalls = 0;
        }
        if steps >= args.max_steps {
            return focus_failure(&selector, steps, "max_steps", &screen, Some(&target));
        }

        let dir = direction_toward(current, &target);
        prev_focused = current_key;
        client.key(dir).await?;
        tokio::time::sleep(Duration::from_millis(SETTLE_MS)).await;
        steps += 1;
    }
}

fn focus_failure(
    selector: &Selector,
    steps: u32,
    reason: &str,
    screen: &crate::proto::ScreenResponse,
    target: Option<&Element>,
) -> Result<Outcome> {
    let code = if reason == "not_found" {
        "element_not_found"
    } else {
        "focus_unreachable"
    };
    let next_actions = if reason == "not_found" {
        vec![
            "inspect detail.top_texts/current_app and wait for the intended screen",
            "refine the selector, then retry `ui focus`",
        ]
    } else {
        vec![
            "inspect detail.target and the currently focused element in a fresh `ui dump --full`",
            "choose a reachable focusable selector or navigate an intermediate element before retrying",
        ]
    };
    Err(crate::diagnostic::DiagnosticError::new(
        code,
        "ui",
        format!(
            "could not focus {} after {steps} D-pad step(s): {reason}",
            selector.label()
        ),
    )
    .retryable(reason == "not_found")
    .detail(serde_json::json!({
        "selector": selector.label(),
        "reason": reason,
        "steps": steps,
        "target": target.map(candidate_json),
        "screen_hash": screen.screen_hash,
        "screen_hash_version": screen.screen_hash_version,
        "current_app": screen.current_app,
        "top_texts": crate::fusion::top_screen_texts(&screen.elements, 12),
    }))
    .next_actions(next_actions)
    .into())
}

/// The element that effectively holds D-pad focus. If the `focused` node has
/// usable bounds it *is* the focused content. Otherwise it's a wrapper (common
/// in Compose/TV), and the visible content is the next element in DFS order — so
/// fall through to it for selector-match and geometry.
fn effective_focused(elements: &[Element]) -> Option<&Element> {
    let idx = elements.iter().position(|e| e.focused)?;
    let node = &elements[idx];
    if node.bounds.is_some() && (node.text.is_some() || node.desc.is_some() || node.rid.is_some()) {
        Some(node)
    } else {
        elements.get(idx + 1).or(Some(node))
    }
}

/// Dominant-axis D-pad direction from the focused element's center toward the
/// target's center. With no current focus, kick with `dpad_down` to establish one.
fn direction_toward(focused: Option<&Element>, target: &Element) -> &'static str {
    let Some(focused) = focused else {
        return "dpad_down";
    };
    let (fx, fy) = center(focused);
    let (tx, ty) = center(target);
    let dx = tx - fx;
    let dy = ty - fy;
    if dx.abs() >= dy.abs() {
        if dx >= 0 {
            "dpad_right"
        } else {
            "dpad_left"
        }
    } else if dy >= 0 {
        "dpad_down"
    } else {
        "dpad_up"
    }
}

fn center(el: &Element) -> (i32, i32) {
    match el.bounds {
        Some([x1, y1, x2, y2]) => ((x1 + x2) / 2, (y1 + y2) / 2),
        None => (0, 0),
    }
}

/// Stable-enough identity for "is this the same focused element as last step?".
/// Element ids are per-dump DFS order, so we key off the durable fields instead.
fn focus_key(el: &Element) -> String {
    format!(
        "{}|{}|{}|{:?}",
        el.rid.as_deref().unwrap_or(""),
        el.text.as_deref().unwrap_or(""),
        el.desc.as_deref().unwrap_or(""),
        el.bounds,
    )
}

fn emit(
    selector: &Selector,
    matched: bool,
    steps: u32,
    reason: &str,
    element: Option<&Element>,
    activated: bool,
) -> Result<Outcome> {
    Ok(Outcome::Action(
        "focus",
        serde_json::json!({
            "selector": selector.label(),
            "matched": matched,
            "steps": steps,
            "reason": reason,
            "activated": activated && matched,
            "element": element.map(|e| serde_json::json!({
                "id": e.id, "text": e.text, "rid": e.rid, "desc": e.desc,
                "focused": e.focused, "tap": e.tap,
            })),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(id: u32, bounds: [i32; 4], focused: bool) -> Element {
        Element {
            id,
            text: Some(format!("e{id}")),
            desc: None,
            klass: None,
            rid: None,
            bounds: Some(bounds),
            tap: None,
            clickable: false,
            long_clickable: false,
            scrollable: false,
            checkable: false,
            focusable: true,
            enabled: true,
            selected: false,
            checked: false,
            focused,
            password: false,
            input: false,
        }
    }

    #[test]
    fn steps_right_toward_a_target_on_the_same_row() {
        let focused = el(0, [0, 100, 100, 200], true);
        let target = el(1, [400, 100, 500, 200], false);
        assert_eq!(direction_toward(Some(&focused), &target), "dpad_right");
    }

    #[test]
    fn steps_up_toward_a_target_above() {
        let focused = el(0, [0, 400, 100, 500], true);
        let target = el(1, [0, 0, 100, 100], false);
        assert_eq!(direction_toward(Some(&focused), &target), "dpad_up");
    }

    #[test]
    fn kicks_down_when_nothing_is_focused() {
        let target = el(1, [0, 0, 100, 100], false);
        assert_eq!(direction_toward(None, &target), "dpad_down");
    }

    fn text_el(id: u32, text: &str) -> Element {
        let mut e = el(id, [0, id as i32 * 20, 10, id as i32 * 20 + 10], false);
        e.text = Some(text.into());
        e
    }

    #[test]
    fn resolve_target_prefers_exact_then_errors_on_ambiguity() {
        let sel = Selector::Text("Allow".into());
        // "Allow" substring-matches both, but exactly one is an exact match →
        // the exact one wins (the agent's "Allow" vs "Allow all" case).
        let got = resolve_target(
            &sel,
            &[text_el(0, "Allow"), text_el(1, "Allow all the time")],
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.text.as_deref(), Some("Allow"));

        // Two substring matches and no exact → ambiguous; the agent must narrow.
        let err = resolve_target(
            &sel,
            &[text_el(0, "Allow now"), text_el(1, "Allow all the time")],
            false,
        )
        .unwrap_err();
        assert_eq!(err.candidates.len(), 2);
    }
}
