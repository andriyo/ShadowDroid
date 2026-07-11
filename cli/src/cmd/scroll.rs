//! `scroll-to` — scroll a list until an element matching a selector is on
//! screen, then optionally tap it. The single most common in-loop UI primitive
//! that neither raw `adb` nor a one-shot dump provides.
//!
//! Host MVP: loop `screen` → check the selector → `swipe` → repeat, stopping at
//! a match, `--max-swipes`, or an unchanged `screen_hash` (the list can't
//! scroll further). It is correct but pays a full dump per step; Phase 6 adds a
//! fast on-device `UiScrollable` path that this falls back from.
//!
//! `--direction` is the **content** scroll direction (where you're looking):
//! `down` (default) reveals items further down a vertical list. The finger
//! gesture is the opposite, handled internally.

use anyhow::Result;

use crate::device::client::ServerClient;
use crate::fusion::Outcome;
use crate::proto::{Element, ScrollResp};
use crate::selector::{Selector, SelectorArgs};

#[derive(clap::Args)]
pub struct ScrollArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,
    /// Content scroll direction: down (default) reveals items further down.
    #[arg(long, default_value = "down", value_parser = ["up", "down", "left", "right"])]
    pub direction: String,
    /// Maximum number of swipes before giving up.
    #[arg(long, default_value_t = 12)]
    pub max_swipes: u32,
    /// Restrict swiping to the bounds of the scrollable with this resource-id.
    #[arg(long)]
    pub container_rid: Option<String>,
    /// Tap the element once found.
    #[arg(long)]
    pub tap: bool,
    /// Swipe duration in milliseconds.
    #[arg(long, default_value_t = 250)]
    pub duration_ms: u32,
}

pub async fn run(client: &ServerClient, args: &ScrollArgs) -> Result<Outcome> {
    let selector = args.selector.exactly_one()?;

    // Fast path: drive a scrollable on-device. On any error (older server with
    // no /v1/scroll route, or no scrollable container) fall back to the host
    // loop below — the server returns matched=false (not an error) when the
    // container exists but the item simply isn't there.
    let server = client
        .scroll(
            args.selector.rid.as_deref(),
            args.selector.text.as_deref(),
            args.selector.desc.as_deref(),
            &args.direction,
            args.container_rid.as_deref(),
            args.max_swipes,
            args.tap,
        )
        .await;
    if let Ok(resp) = server {
        if resp.matched {
            return emit_server(&selector, &resp, args.tap);
        }
        let screen = client.screen().await?;
        return scroll_failure(&selector, resp.swipes, "server_no_match", &screen);
    }

    let swipe_dir = finger_direction(&args.direction);

    let mut swipes = 0u32;
    let mut last_hash = String::new();
    loop {
        let screen = client.screen().await?;
        if let Some(el) = screen.elements.iter().find(|e| selector.matches(e, false)) {
            let mut el = el.clone();
            if args.tap {
                el = client.find_tap(&selector.query()).await?.matched;
            }
            return emit(&selector, true, swipes, "found", Some(&el), args.tap);
        }
        if swipes >= args.max_swipes {
            return scroll_failure(&selector, swipes, "max_swipes", &screen);
        }
        // An unchanged hash since the previous swipe means the list won't move.
        if !last_hash.is_empty() && screen.screen_hash == last_hash {
            return scroll_failure(&selector, swipes, "end_reached", &screen);
        }
        last_hash = screen.screen_hash.clone();

        // Locate the scroll container fresh each iteration (its bounds are stable
        // but ids can shift as the list recycles views).
        let container = args
            .container_rid
            .as_deref()
            .and_then(|rid| find_container(&screen.elements, rid));
        swipe(client, swipe_dir, container.as_ref(), args.duration_ms).await?;
        swipes += 1;
    }
}

fn find_container<'a>(elements: &'a [Element], rid: &str) -> Option<&'a Element> {
    elements
        .iter()
        .find(|e| e.rid.as_deref().is_some_and(|r| r.contains(rid)))
}

/// Content direction → finger swipe direction (the opposite).
fn finger_direction(content: &str) -> &'static str {
    match content {
        "down" => "up",
        "up" => "down",
        "left" => "right",
        "right" => "left",
        _ => "up",
    }
}

async fn swipe(
    client: &ServerClient,
    finger_dir: &str,
    container: Option<&&Element>,
    duration_ms: u32,
) -> Result<()> {
    match container.and_then(|el| el.bounds) {
        Some([x1, y1, x2, y2]) => {
            let (sx, sy, ex, ey) = swipe_within(x1, y1, x2, y2, finger_dir);
            client.swipe(sx, sy, ex, ey, duration_ms).await
        }
        None => client.swipe_ext(finger_dir, 0.7, duration_ms).await,
    }
}

/// Compute a swipe inside a rectangle for the given finger direction, crossing
/// across the middle 60% so it stays clear of the edges.
fn swipe_within(x1: i32, y1: i32, x2: i32, y2: i32, finger_dir: &str) -> (i32, i32, i32, i32) {
    let cx = (x1 + x2) / 2;
    let cy = (y1 + y2) / 2;
    let lo_y = y1 + (y2 - y1) * 3 / 10;
    let hi_y = y1 + (y2 - y1) * 7 / 10;
    let lo_x = x1 + (x2 - x1) * 3 / 10;
    let hi_x = x1 + (x2 - x1) * 7 / 10;
    match finger_dir {
        "up" => (cx, hi_y, cx, lo_y),
        "down" => (cx, lo_y, cx, hi_y),
        "left" => (hi_x, cy, lo_x, cy),
        _ => (lo_x, cy, hi_x, cy), // right
    }
}

/// Result of the fast on-device path (coords only, no element detail).
fn emit_server(selector: &Selector, resp: &ScrollResp, tap: bool) -> Result<Outcome> {
    Ok(Outcome::Action(
        "scroll_to",
        serde_json::json!({
            "selector": selector.label(),
            "matched": resp.matched,
            "swipes": resp.swipes,
            "reason": "server",
            "tapped": tap && resp.matched,
            "element": resp.matched.then(|| serde_json::json!({ "tap": [resp.x, resp.y] })),
        }),
    ))
}

fn scroll_failure(
    selector: &Selector,
    swipes: u32,
    reason: &str,
    screen: &crate::proto::ScreenResponse,
) -> Result<Outcome> {
    Err(crate::diagnostic::DiagnosticError::new(
        "element_not_found",
        "ui",
        format!(
            "scroll-to did not find {} after {swipes} swipe(s): {reason}",
            selector.label()
        ),
    )
    .retryable(true)
    .detail(serde_json::json!({
        "selector": selector.label(),
        "reason": reason,
        "swipes": swipes,
        "screen_hash": screen.screen_hash,
        "screen_hash_version": screen.screen_hash_version,
        "current_app": screen.current_app,
        "top_texts": crate::fusion::top_screen_texts(&screen.elements, 12),
    }))
    .next_actions([
        "inspect detail.top_texts/current_app and confirm the expected list is visible",
        "refine the selector or container/direction, increase --max-swipes when appropriate, then retry",
    ])
    .into())
}

fn emit(
    selector: &Selector,
    matched: bool,
    swipes: u32,
    reason: &str,
    element: Option<&Element>,
    tapped: bool,
) -> Result<Outcome> {
    Ok(Outcome::Action(
        "scroll_to",
        serde_json::json!({
            "selector": selector.label(),
            "matched": matched,
            "swipes": swipes,
            "reason": reason,
            "tapped": tapped && matched,
            "element": element.map(|e| serde_json::json!({
                "id": e.id, "text": e.text, "rid": e.rid, "desc": e.desc, "tap": e.tap,
            })),
        }),
    ))
}
