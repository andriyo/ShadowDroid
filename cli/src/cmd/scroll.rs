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

use anyhow::{bail, Result};

use crate::device::client::ServerClient;
use crate::proto::{Element, ScrollResp};

#[derive(clap::Args)]
pub struct ScrollArgs {
    /// Match an element whose text contains this (substring, case-insensitive).
    #[arg(long)]
    pub text: Option<String>,
    /// Match by resource-id substring.
    #[arg(long)]
    pub rid: Option<String>,
    /// Match by content-description substring.
    #[arg(long)]
    pub desc: Option<String>,
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

enum Selector {
    Text(String),
    Rid(String),
    Desc(String),
}

impl Selector {
    fn from_args(a: &ScrollArgs) -> Result<Self> {
        match (&a.text, &a.rid, &a.desc) {
            (Some(t), None, None) => Ok(Selector::Text(t.clone())),
            (None, Some(r), None) => Ok(Selector::Rid(r.clone())),
            (None, None, Some(d)) => Ok(Selector::Desc(d.clone())),
            (None, None, None) => bail!("pass exactly one of --text / --rid / --desc"),
            _ => bail!("pass only one of --text / --rid / --desc"),
        }
    }

    fn matches(&self, el: &Element) -> bool {
        let (field, query) = match self {
            Selector::Text(q) => (&el.text, q),
            Selector::Rid(q) => (&el.rid, q),
            Selector::Desc(q) => (&el.desc, q),
        };
        field
            .as_deref()
            .is_some_and(|v| v.to_lowercase().contains(&query.to_lowercase()))
    }

    fn label(&self) -> serde_json::Value {
        match self {
            Selector::Text(q) => serde_json::json!({ "text": q }),
            Selector::Rid(q) => serde_json::json!({ "rid": q }),
            Selector::Desc(q) => serde_json::json!({ "desc": q }),
        }
    }
}

pub async fn run(client: &ServerClient, args: &ScrollArgs) -> Result<()> {
    let selector = Selector::from_args(args)?;

    // Fast path: drive a scrollable on-device. On any error (older server with
    // no /v1/scroll route, or no scrollable container) fall back to the host
    // loop below — the server returns matched=false (not an error) when the
    // container exists but the item simply isn't there.
    let server = client
        .scroll(
            args.rid.as_deref(),
            args.text.as_deref(),
            args.desc.as_deref(),
            &args.direction,
            args.container_rid.as_deref(),
            args.max_swipes,
            args.tap,
        )
        .await;
    if let Ok(resp) = server {
        return emit_server(&selector, &resp, args.tap);
    }

    let swipe_dir = finger_direction(&args.direction);

    let mut swipes = 0u32;
    let mut last_hash = String::new();
    loop {
        let screen = client.screen().await?;
        if let Some(el) = screen.elements.iter().find(|e| selector.matches(e)) {
            let el = el.clone();
            if args.tap {
                client.tap_xy(el.tap[0], el.tap[1]).await?;
            }
            return emit(&selector, true, swipes, "found", Some(&el), args.tap);
        }
        if swipes >= args.max_swipes {
            return emit(&selector, false, swipes, "max_swipes", None, false);
        }
        // An unchanged hash since the previous swipe means the list won't move.
        if !last_hash.is_empty() && screen.screen_hash == last_hash {
            return emit(&selector, false, swipes, "end_reached", None, false);
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
    match container {
        Some(el) => {
            let [x1, y1, x2, y2] = el.bounds;
            let (sx, sy, ex, ey) = swipe_within(x1, y1, x2, y2, finger_dir);
            client.swipe(sx, sy, ex, ey, duration_ms).await
        }
        None => client.swipe_ext(finger_dir, 0.7, duration_ms).await,
    }
}

/// Compute a swipe inside a rectangle for the given finger direction, moving
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

/// Emit the result of the fast on-device path (coords only, no element detail).
fn emit_server(selector: &Selector, resp: &ScrollResp, tap: bool) -> Result<()> {
    println!(
        "{}",
        serde_json::json!({
            "type": "action",
            "cmd": "scroll_to",
            "selector": selector.label(),
            "matched": resp.matched,
            "swipes": resp.swipes,
            "reason": "server",
            "tapped": tap && resp.matched,
            "element": resp.matched.then(|| serde_json::json!({ "tap": [resp.x, resp.y] })),
        })
    );
    Ok(())
}

fn emit(
    selector: &Selector,
    matched: bool,
    swipes: u32,
    reason: &str,
    element: Option<&Element>,
    tapped: bool,
) -> Result<()> {
    println!(
        "{}",
        serde_json::json!({
            "type": "action",
            "cmd": "scroll_to",
            "selector": selector.label(),
            "matched": matched,
            "swipes": swipes,
            "reason": reason,
            "tapped": tapped && matched,
            "element": element.map(|e| serde_json::json!({
                "id": e.id, "text": e.text, "rid": e.rid, "desc": e.desc, "tap": e.tap,
            })),
        })
    );
    Ok(())
}
