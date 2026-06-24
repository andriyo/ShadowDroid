//! Agent-facing layout artifacts.
//!
//! This starts with ShadowDroid's deterministic accessibility/UIAutomator tree.
//! Android Studio Layout Inspector enrichment is added when the plugin bridge
//! and active inspector model are available.

use crate::cmd::debugger::BridgeClient;
use crate::cmd::studio_contract::{query, route, value};
use crate::device::client::ServerClient;
use crate::ids::Serial;
use crate::proto::{Element, ScreenResponse};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Args)]
pub struct LayoutArgs {
    #[command(subcommand)]
    pub cmd: LayoutCmd,
}

#[derive(Subcommand)]
pub enum LayoutCmd {
    /// Capture a layout snapshot from the current UI tree.
    Snapshot(LayoutSnapshotArgs),
    /// Diff two layout snapshot JSON files.
    Diff(LayoutDiffArgs),
    /// Compose recomposition counters from Android Studio Layout Inspector.
    Recompositions(RecompositionArgs),
    /// Resolve a current UI node and map it to source when source mapping is available.
    Source(LayoutSourceArgs),
}

#[derive(Args)]
pub struct LayoutSnapshotArgs {
    /// Write JSON to a file instead of stdout.
    #[arg(short = 'o', long)]
    pub out: Option<PathBuf>,
    /// Request Compose-specific enrichment when available.
    #[arg(long)]
    pub compose: bool,
    /// Request semantic/accessibility enrichment when available.
    #[arg(long)]
    pub semantics: bool,
    /// Request source mapping when available.
    #[arg(long)]
    pub source_map: bool,
    /// Include a screenshot artifact next to the snapshot.
    #[arg(long)]
    pub screenshot: bool,
    /// Android Studio plugin bridge URL for Layout Inspector enrichment.
    #[arg(long, env = "SHADOWDROID_STUDIO_DEBUGGER_URL")]
    pub studio_url: Option<String>,
}

#[derive(Args)]
pub struct LayoutDiffArgs {
    pub before: PathBuf,
    pub after: PathBuf,
}

#[derive(Args)]
pub struct RecompositionArgs {
    /// Reset recomposition counters when supported by an Android Studio bridge.
    #[arg(long)]
    pub reset: bool,
    /// Android Studio plugin bridge URL.
    #[arg(long, env = "SHADOWDROID_STUDIO_DEBUGGER_URL")]
    pub studio_url: Option<String>,
}

#[derive(Args)]
pub struct LayoutSourceArgs {
    /// Element id from `screen` or `layout snapshot`.
    #[arg(long)]
    pub id: Option<u32>,
    /// Android Studio Layout Inspector draw id from `layout snapshot`.
    #[arg(long)]
    pub draw_id: Option<i64>,
    /// Match text substring.
    #[arg(long)]
    pub text: Option<String>,
    /// Match resource-id substring.
    #[arg(long)]
    pub rid: Option<String>,
    /// Match content-description substring.
    #[arg(long)]
    pub desc: Option<String>,
    /// Android Studio plugin bridge URL.
    #[arg(long, env = "SHADOWDROID_STUDIO_DEBUGGER_URL")]
    pub studio_url: Option<String>,
}

pub async fn run(serial: &Serial, client: &ServerClient, args: LayoutArgs) -> Result<()> {
    match args.cmd {
        LayoutCmd::Snapshot(args) => snapshot_cmd(serial, client, args).await,
        LayoutCmd::Diff(args) => diff_cmd(args),
        LayoutCmd::Recompositions(args) => recompositions_cmd(args).await,
        LayoutCmd::Source(args) => source_cmd(serial, client, args).await,
    }
}

async fn snapshot_cmd(
    serial: &Serial,
    client: &ServerClient,
    args: LayoutSnapshotArgs,
) -> Result<()> {
    let screen = client.screen().await.context("reading screen tree")?;
    let screenshot = if args.screenshot {
        Some(write_screenshot(client, args.out.as_deref()).await?)
    } else {
        None
    };
    let mut value = layout_snapshot_value(serial, screen, screenshot, &args);
    if args.compose || args.semantics || args.source_map {
        let studio = studio_layout_snapshot(args.studio_url.as_deref()).await;
        merge_studio_layout(&mut value, studio);
    }
    if let Some(path) = args.out {
        write_json_file(&path, &value)?;
    } else {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(())
}

fn layout_snapshot_value(
    serial: &Serial,
    screen: ScreenResponse,
    screenshot: Option<Value>,
    args: &LayoutSnapshotArgs,
) -> Value {
    json!({
        "type": value::LAYOUT_SNAPSHOT,
        "schema_version": 1,
        "ts": now_ms(),
        "device": serial,
        "screen_hash": screen.screen_hash,
        "viewport": screen.viewport,
        "current_app": screen.current_app,
        "element_count": screen.element_count,
        "elements": screen.elements,
        "screenshot": screenshot,
        "features": {
            "compose_requested": args.compose,
            "semantics_requested": args.semantics,
            "source_map_requested": args.source_map,
            "compose": {"available": false, "source": "android_studio_layout_inspector_not_wired_yet"},
            "semantics": {"available": false, "source": "uiautomator_accessibility_fields_only"},
            "source_map": {"available": false, "source": "android_studio_layout_inspector_not_wired_yet"}
        }
    })
}

fn diff_cmd(args: LayoutDiffArgs) -> Result<()> {
    let before = read_snapshot(&args.before)?;
    let after = read_snapshot(&args.after)?;
    let before_map = element_map(&before);
    let after_map = element_map(&after);
    let before_keys = before_map.keys().cloned().collect::<BTreeSet<_>>();
    let after_keys = after_map.keys().cloned().collect::<BTreeSet<_>>();

    let added = after_keys
        .difference(&before_keys)
        .filter_map(|key| after_map.get(key))
        .cloned()
        .collect::<Vec<_>>();
    let removed = before_keys
        .difference(&after_keys)
        .filter_map(|key| before_map.get(key))
        .cloned()
        .collect::<Vec<_>>();
    let mut changed = Vec::new();
    for key in before_keys.intersection(&after_keys) {
        let Some(before_el) = before_map.get(key) else {
            continue;
        };
        let Some(after_el) = after_map.get(key) else {
            continue;
        };
        if before_el != after_el {
            changed.push(json!({"key": key, "before": before_el, "after": after_el}));
        }
    }

    println!(
        "{}",
        serde_json::to_string(&json!({
            "type": "layout_diff",
            "schema_version": 1,
            "before": args.before,
            "after": args.after,
            "counts": {
                "added": added.len(),
                "removed": removed.len(),
                "changed": changed.len(),
            },
            "added": added,
            "removed": removed,
            "changed": changed,
        }))?
    );
    Ok(())
}

async fn recompositions_cmd(args: RecompositionArgs) -> Result<()> {
    let reset_s = args.reset.to_string();
    let value = match BridgeClient::new(args.studio_url.as_deref()) {
        Ok(bridge) => match bridge
            .get(
                route::LAYOUT_RECOMPOSITIONS,
                &[(query::RESET, Some(reset_s.as_str()))],
            )
            .await
        {
            Ok(value) => value,
            Err(err) => studio_layout_unavailable(args.reset, err.to_string()),
        },
        Err(err) => studio_layout_unavailable(args.reset, err.to_string()),
    };
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

async fn source_cmd(serial: &Serial, client: &ServerClient, args: LayoutSourceArgs) -> Result<()> {
    if args.id.is_none()
        && args.draw_id.is_none()
        && args.text.is_none()
        && args.rid.is_none()
        && args.desc.is_none()
    {
        anyhow::bail!("layout source needs --id, --draw-id, --text, --rid, or --desc");
    }
    let screen = client.screen().await.context("reading screen tree")?;
    let needs_element_match =
        args.id.is_some() || args.text.is_some() || args.rid.is_some() || args.desc.is_some();
    let element = if needs_element_match {
        screen
            .elements
            .iter()
            .find(|element| element_matches(element, &args))
            .cloned()
    } else {
        None
    };
    let studio = studio_layout_source(args.studio_url.as_deref(), element.as_ref(), &args).await;
    let source = studio
        .get("source")
        .cloned()
        .or_else(|| {
            studio
                .get("error")
                .map(|error| json!({"available": false, "reason": error}))
        })
        .unwrap_or_else(|| {
            json!({
                "available": false,
                "reason": "Android Studio Layout Inspector source mapping is unavailable"
            })
        });
    println!(
        "{}",
        serde_json::to_string(&json!({
            "type": value::LAYOUT_SOURCE,
            "schema_version": 1,
            "device": serial,
            "screen_hash": screen.screen_hash,
            "matched": element,
            "source": source,
            "android_studio_layout": studio
        }))?
    );
    Ok(())
}

async fn studio_layout_snapshot(studio_url: Option<&str>) -> Result<Value> {
    let bridge = BridgeClient::new(studio_url)?;
    bridge.get(route::LAYOUT_SNAPSHOT, &[]).await
}

async fn studio_layout_source(
    studio_url: Option<&str>,
    element: Option<&Element>,
    args: &LayoutSourceArgs,
) -> Value {
    let bridge = match BridgeClient::new(studio_url) {
        Ok(bridge) => bridge,
        Err(err) => return json!({"available": false, "error": err.to_string()}),
    };
    let text = args
        .text
        .as_deref()
        .or_else(|| element.and_then(|element| element.text.as_deref()));
    let rid = args
        .rid
        .as_deref()
        .or_else(|| element.and_then(|element| element.rid.as_deref()));
    let class = element.and_then(|element| element.klass.as_deref());
    let draw_id = args.draw_id.map(|draw_id| draw_id.to_string());
    let params = [
        (query::DRAW_ID, draw_id.as_deref()),
        (query::TEXT, text),
        (query::RID, rid),
        (query::CLASS, class),
        (query::DESC, args.desc.as_deref()),
    ];
    match bridge.get(route::LAYOUT_SOURCE, &params).await {
        Ok(value) => value,
        Err(err) => json!({"available": false, "error": err.to_string()}),
    }
}

fn merge_studio_layout(value: &mut Value, studio: Result<Value>) {
    match studio {
        Ok(studio) => {
            if studio
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                if let Some(features) = studio.get("features").cloned() {
                    value["features"] = features;
                }
            }
            value["android_studio_layout"] = studio;
        }
        Err(err) => {
            value["android_studio_layout"] = json!({
                "available": false,
                "error": err.to_string()
            });
        }
    }
}

fn studio_layout_unavailable(reset: bool, reason: String) -> Value {
    json!({
        "type": value::LAYOUT_RECOMPOSITIONS,
        "ok": true,
        "available": false,
        "reset_requested": reset,
        "reason": reason
    })
}

fn element_matches(element: &Element, args: &LayoutSourceArgs) -> bool {
    if let Some(id) = args.id {
        return element.id == id;
    }
    crate::selector::text_matches(element.text.as_deref(), args.text.as_deref(), false)
        && crate::selector::text_matches(element.rid.as_deref(), args.rid.as_deref(), false)
        && crate::selector::text_matches(element.desc.as_deref(), args.desc.as_deref(), false)
}

fn read_snapshot(path: &Path) -> Result<Vec<Element>> {
    let body =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    let elements = value
        .get("elements")
        .cloned()
        .or_else(|| value.get("screen").and_then(|s| s.get("elements")).cloned())
        .context("snapshot has no elements array")?;
    serde_json::from_value(elements)
        .with_context(|| format!("reading elements from {}", path.display()))
}

fn element_map(elements: &[Element]) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for element in elements {
        let key = element_key(element);
        out.insert(
            key,
            serde_json::to_value(element).unwrap_or_else(|_| json!({})),
        );
    }
    out
}

fn element_key(element: &Element) -> String {
    if let Some(rid) = element.rid.as_deref().filter(|s| !s.is_empty()) {
        return format!("rid:{rid}");
    }
    if let Some(desc) = element.desc.as_deref().filter(|s| !s.is_empty()) {
        return format!("desc:{desc}:bounds:{:?}", element.bounds);
    }
    if let Some(text) = element.text.as_deref().filter(|s| !s.is_empty()) {
        return format!("text:{text}:bounds:{:?}", element.bounds);
    }
    format!(
        "class:{}:bounds:{:?}",
        element.klass.as_deref().unwrap_or(""),
        element.bounds
    )
}

async fn write_screenshot(client: &ServerClient, out: Option<&Path>) -> Result<Value> {
    let bytes = client
        .screenshot_png()
        .await
        .context("capturing screenshot")?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let dir = out
        .and_then(Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join("shadowdroid-layout"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("layout-{}-{}.png", now_ms(), &hash[..12]));
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(json!({
        "path": path.display().to_string(),
        "bytes": bytes.len() as u64,
        "hash": hash,
        "hash_algorithm": "blake3",
    }))
}

fn write_json_file(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("writing {}", path.display()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
