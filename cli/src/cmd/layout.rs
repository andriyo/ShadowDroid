//! Agent-facing layout artifacts.
//!
//! This starts with ShadowDroid's deterministic accessibility/UIAutomator tree.
//! Android Studio Layout Inspector enrichment is added when the plugin bridge
//! and active inspector model are available.

use crate::cmd::debugger::BridgeClient;
use crate::cmd::studio_contract::{query, route, value};
use crate::device::client::ServerClient;
use crate::ids::Serial;
use crate::proto::{AppRef, Element, ScreenResponse};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_LAYOUT_STUDIO_WAIT_MS: u64 = 5_000;

#[derive(Debug, Clone, Serialize)]
pub struct UiFallbackElement {
    pub id: String,
    pub draw_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub bounds: [i32; 4],
    pub tap: [i32; 2],
    pub source: &'static str,
    pub confidence: f64,
    pub stable_selector: bool,
    pub actionability: &'static str,
    pub requires_coordinate_fallback: bool,
    pub semantics: Value,
}

#[derive(Debug)]
pub struct UiFallbackDiscovery {
    pub available: bool,
    pub elements: Vec<UiFallbackElement>,
    pub inspector: Value,
}

impl UiFallbackDiscovery {
    pub fn to_value(&self) -> Value {
        json!({
            "requested": true,
            "available": self.available,
            "source": "android_studio_layout_inspector",
            "element_count": self.elements.len(),
            "elements": self.elements,
            "android_studio_layout": self.inspector,
            "ocr": {
                "available": false,
                "automatic": false,
                "reason": "OCR is not run automatically; coordinate actions remain explicit and can be protected with --if-screen"
            }
        })
    }
}

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
    /// Override the app package/process selected in Android Studio Layout Inspector.
    #[arg(long)]
    pub app: Option<String>,
    /// Override the process id selected in Android Studio Layout Inspector.
    #[arg(long)]
    pub pid: Option<i32>,
    /// How long to wait for Android Studio Layout Inspector to produce a model.
    #[arg(long, default_value_t = DEFAULT_LAYOUT_STUDIO_WAIT_MS)]
    pub studio_wait_ms: u64,
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
    /// Override the app package/process selected in Android Studio Layout Inspector.
    #[arg(long)]
    pub app: Option<String>,
    /// Override the process id selected in Android Studio Layout Inspector.
    #[arg(long)]
    pub pid: Option<i32>,
    /// How long to wait for Android Studio Layout Inspector to produce a model.
    #[arg(long, default_value_t = DEFAULT_LAYOUT_STUDIO_WAIT_MS)]
    pub studio_wait_ms: u64,
}

#[derive(Args)]
pub struct LayoutSourceArgs {
    /// Element id from `screen` or `layout snapshot`.
    #[arg(long)]
    pub id: Option<u32>,
    /// Android Studio Layout Inspector draw id from `layout snapshot`.
    #[arg(long)]
    pub draw_id: Option<i64>,
    #[command(flatten)]
    pub selector: crate::selector::SelectorArgs,
    /// Android Studio plugin bridge URL.
    #[arg(long, env = "SHADOWDROID_STUDIO_DEBUGGER_URL")]
    pub studio_url: Option<String>,
    /// Override the app package/process selected in Android Studio Layout Inspector.
    #[arg(long)]
    pub app: Option<String>,
    /// Override the process id selected in Android Studio Layout Inspector.
    #[arg(long)]
    pub pid: Option<i32>,
    /// How long to wait for Android Studio Layout Inspector to produce a model.
    #[arg(long, default_value_t = DEFAULT_LAYOUT_STUDIO_WAIT_MS)]
    pub studio_wait_ms: u64,
}

pub async fn run(serial: &Serial, client: &ServerClient, args: LayoutArgs) -> Result<()> {
    match args.cmd {
        LayoutCmd::Snapshot(args) => snapshot_cmd(serial, client, args).await,
        LayoutCmd::Diff(args) => diff_cmd(args),
        LayoutCmd::Recompositions(args) => recompositions_cmd(serial, client, args).await,
        LayoutCmd::Source(args) => source_cmd(serial, client, args).await,
    }
}

async fn snapshot_cmd(
    serial: &Serial,
    client: &ServerClient,
    args: LayoutSnapshotArgs,
) -> Result<()> {
    let screen = client.screen().await.context("reading screen tree")?;
    let screen_for_sample = screen.clone();
    let current_app = screen.current_app.clone();
    let screenshot = if args.screenshot {
        Some(write_screenshot(client, args.out.as_deref()).await?)
    } else {
        None
    };
    let mut value = layout_snapshot_value(serial, screen, screenshot, &args);
    let mut target_for_sample = None;
    if args.compose || args.semantics || args.source_map {
        let target = LayoutStudioTarget::from_ref(
            serial,
            &current_app,
            args.app.as_deref(),
            args.pid,
            args.studio_wait_ms,
        );
        let studio = studio_layout_snapshot(args.studio_url.as_deref(), &target).await;
        merge_studio_layout(&mut value, studio);
        target_for_sample = Some(target);
    }
    annotate_layout_sample(
        &mut value,
        &screen_for_sample,
        target_for_sample.as_ref(),
        args.compose || args.semantics || args.source_map,
    );
    if let Some(path) = args.out {
        crate::cmd::artifact::write_json_and_emit("layout_snapshot", &path, &value)?;
    } else {
        crate::events::emit_result(&value);
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
        "screen_hash": screen.screen_hash.clone(),
        "screen_hash_version": screen.screen_hash_version,
        "viewport": screen.viewport,
        "current_app": screen.current_app,
        "element_count": screen.element_count,
        "elements": screen.elements,
        "screenshot": screenshot,
        "features": {
            "compose_requested": args.compose,
            "semantics_requested": args.semantics,
            "source_map_requested": args.source_map,
            "compose": {"available": false, "source": "android_studio_layout_inspector"},
            "semantics": {"available": false, "source": "uiautomator_accessibility_fields_only"},
            "source_map": {"available": false, "source": "android_studio_layout_inspector"}
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

    crate::events::emit_result(&json!({
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
    }));
    Ok(())
}

async fn recompositions_cmd(
    serial: &Serial,
    client: &ServerClient,
    args: RecompositionArgs,
) -> Result<()> {
    let screen = client.screen().await.context("reading screen tree")?;
    let target = LayoutStudioTarget::from_ref(
        serial,
        &screen.current_app,
        args.app.as_deref(),
        args.pid,
        args.studio_wait_ms,
    );
    let reset_s = args.reset.to_string();
    let value = match BridgeClient::new(args.studio_url.as_deref()) {
        Ok(bridge) => {
            let params = target.params_with(&[(query::RESET, Some(reset_s.as_str()))]);
            match bridge.get(route::LAYOUT_RECOMPOSITIONS, &params).await {
                Ok(value) => value,
                Err(err) => studio_layout_unavailable(args.reset, err.to_string()),
            }
        }
        Err(err) => studio_layout_unavailable(args.reset, err.to_string()),
    };
    let mut value = value;
    annotate_layout_sample(&mut value, &screen, Some(&target), true);
    let available = value
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
    if !available || !ok {
        return Err(crate::diagnostic::DiagnosticError::new(
            "layout_recompositions_unavailable",
            "layout",
            if args.reset {
                "recomposition counters were not reset because Layout Inspector is unavailable"
            } else {
                "recomposition counters are unavailable from Layout Inspector"
            },
        )
        .retryable(true)
        .detail(value)
        .next_actions([
            "run `shadowdroid studio status --json` and start/install the Studio plugin if needed",
            "attach Layout Inspector to the requested app/process, then retry",
        ])
        .into());
    }
    crate::events::emit_result(&value);
    Ok(())
}

async fn source_cmd(serial: &Serial, client: &ServerClient, args: LayoutSourceArgs) -> Result<()> {
    if args.id.is_none()
        && args.draw_id.is_none()
        && args.selector.text.is_none()
        && args.selector.rid.is_none()
        && args.selector.desc.is_none()
    {
        return Err(crate::diagnostic::DiagnosticError::new(
            "layout_source_selector_required",
            "input",
            "layout source needs --id, --draw-id, --text, --rid, or --desc",
        )
        .next_actions([
            "run `shadowdroid layout snapshot --source-map` and choose an element id/draw id",
            "rerun `layout source` with one stable selector",
        ])
        .into());
    }
    let screen = client.screen().await.context("reading screen tree")?;
    let needs_element_match = args.id.is_some()
        || args.selector.text.is_some()
        || args.selector.rid.is_some()
        || args.selector.desc.is_some();
    let element = if needs_element_match {
        screen
            .elements
            .iter()
            .find(|element| element_matches(element, &args))
            .cloned()
    } else {
        None
    };
    if needs_element_match && element.is_none() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "element_not_found",
            "layout",
            "layout source selector did not match an element on the current screen",
        )
        .retryable(true)
        .detail(json!({
            "id": args.id,
            "text": args.selector.text,
            "rid": args.selector.rid,
            "desc": args.selector.desc,
            "screen_hash": screen.screen_hash,
            "screen_hash_version": screen.screen_hash_version,
            "current_app": screen.current_app,
            "top_texts": crate::fusion::top_screen_texts(&screen.elements, 12),
        }))
        .next_actions([
            "inspect detail.top_texts/current_app and choose an id or selector from a fresh layout snapshot",
            "wait for the intended screen, then retry `layout source`",
        ])
        .into());
    }
    let target = LayoutStudioTarget::from_ref(
        serial,
        &screen.current_app,
        args.app.as_deref(),
        args.pid,
        args.studio_wait_ms,
    );
    let studio =
        studio_layout_source(args.studio_url.as_deref(), element.as_ref(), &args, &target).await;
    let source = studio
        .get("source")
        .cloned()
        .or_else(|| {
            studio
                .get("error")
                .map(|error| json!({"available": false, "reason": error}))
        })
        .or_else(|| {
            studio
                .get("reason")
                .map(|reason| json!({"available": false, "reason": reason}))
        })
        .unwrap_or_else(|| {
            json!({
                "available": false,
                "reason": "Android Studio Layout Inspector source mapping is unavailable"
            })
        });
    let mut output = json!({
            "type": value::LAYOUT_SOURCE,
            "schema_version": 1,
            "device": serial,
            "screen_hash": screen.screen_hash,
            "screen_hash_version": screen.screen_hash_version,
            "matched": element,
            "source": source,
            "android_studio_layout": studio
    });
    annotate_layout_sample(&mut output, &screen, Some(&target), true);
    let source_available = output["source"]
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let sample_valid = output
        .get("sample_valid")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !source_available || !sample_valid {
        return Err(crate::diagnostic::DiagnosticError::new(
            "layout_source_unavailable",
            "layout",
            "Android Studio did not produce a valid source mapping for the requested element",
        )
        .retryable(true)
        .detail(output)
        .next_actions([
            "run `shadowdroid studio status --json` and attach Layout Inspector to the target process",
            "take a fresh `layout snapshot --source-map`, choose a reported id/draw id, and retry",
        ])
        .into());
    }
    crate::events::emit_result(&output);
    Ok(())
}

async fn studio_layout_snapshot(
    studio_url: Option<&str>,
    target: &LayoutStudioTarget,
) -> Result<Value> {
    let bridge = BridgeClient::new(studio_url)?;
    let params = target.params();
    bridge.get(route::LAYOUT_SNAPSHOT, &params).await
}

pub async fn discover_ui_fallbacks(
    serial: &Serial,
    studio_url: Option<&str>,
    studio_wait_ms: u64,
    screen: &ScreenResponse,
) -> UiFallbackDiscovery {
    let target =
        LayoutStudioTarget::from_ref(serial, &screen.current_app, None, None, studio_wait_ms);
    match studio_layout_snapshot(studio_url, &target).await {
        Ok(inspector) => {
            let available = inspector
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| {
                    inspector
                        .get("ok")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                });
            let elements = if available {
                compose_fallback_elements(&inspector, &screen.elements)
            } else {
                Vec::new()
            };
            UiFallbackDiscovery {
                available,
                elements,
                inspector,
            }
        }
        Err(err) => UiFallbackDiscovery {
            available: false,
            elements: Vec::new(),
            inspector: json!({
                "available": false,
                "error": err.to_string(),
                "next_actions": [
                    "run `shadowdroid studio status --json` and install/start the Studio plugin if needed",
                    "attach Android Studio Layout Inspector to the foreground app, then retry `ui dump --deep`"
                ]
            }),
        },
    }
}

fn compose_fallback_elements(
    inspector: &Value,
    accessibility: &[Element],
) -> Vec<UiFallbackElement> {
    let mut elements = Vec::new();
    let mut seen = BTreeSet::new();
    let windows = inspector
        .get("windows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for window in windows {
        let nodes = window
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for node in nodes {
            if node.get("kind").and_then(Value::as_str) != Some("compose") {
                continue;
            }
            let Some(draw_id) = node.get("draw_id").and_then(Value::as_i64) else {
                continue;
            };
            if !seen.insert(draw_id) {
                continue;
            }
            let Some(bounds) = inspector_bounds(node.get("bounds")) else {
                continue;
            };
            let text = node
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            let semantics = node
                .get("semantics")
                .cloned()
                .unwrap_or_else(|| json!({"merged": false, "unmerged": false}));
            let has_semantics = semantics.get("merged").and_then(Value::as_bool) == Some(true)
                || semantics.get("unmerged").and_then(Value::as_bool) == Some(true);
            let has_draw_modifier = node
                .get("compose")
                .and_then(|compose| compose.get("has_draw_modifier"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || node
                    .get("compose")
                    .and_then(|compose| compose.get("has_child_draw_modifier"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            if !has_semantics && text.is_none() && !has_draw_modifier {
                continue;
            }
            if represented_by_accessibility(text.as_deref(), bounds, accessibility) {
                continue;
            }
            let (prefix, source, confidence, requires_coordinate_fallback) = if has_semantics {
                ("cs", "compose_semantics", 1.0, false)
            } else {
                ("cl", "compose_layout", 0.75, true)
            };
            elements.push(UiFallbackElement {
                id: format!("{prefix}:{draw_id}"),
                draw_id,
                text,
                bounds,
                tap: [(bounds[0] + bounds[2]) / 2, (bounds[1] + bounds[3]) / 2],
                source,
                confidence,
                stable_selector: false,
                actionability: "unverified",
                requires_coordinate_fallback,
                semantics,
            });
        }
    }
    elements.sort_by_key(|element| (element.bounds[1], element.bounds[0], element.draw_id));
    elements
}

fn inspector_bounds(value: Option<&Value>) -> Option<[i32; 4]> {
    let value = value?;
    let number = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|number| i32::try_from(number).ok())
    };
    let bounds = [
        number("left")?,
        number("top")?,
        number("right")?,
        number("bottom")?,
    ];
    (bounds[2] > bounds[0] && bounds[3] > bounds[1]).then_some(bounds)
}

fn represented_by_accessibility(
    text: Option<&str>,
    bounds: [i32; 4],
    accessibility: &[Element],
) -> bool {
    accessibility.iter().any(|element| {
        let same_text = text.is_some_and(|text| {
            element
                .text
                .as_deref()
                .or(element.desc.as_deref())
                .is_some_and(|candidate| candidate.trim().eq_ignore_ascii_case(text))
        });
        let same_bounds = element.bounds == Some(bounds);
        let same_region = element
            .bounds
            .is_some_and(|candidate| bounds_overlap(candidate, bounds) >= 0.8);
        same_bounds || (same_text && same_region)
    })
}

fn bounds_overlap(a: [i32; 4], b: [i32; 4]) -> f64 {
    let left = a[0].max(b[0]);
    let top = a[1].max(b[1]);
    let right = a[2].min(b[2]);
    let bottom = a[3].min(b[3]);
    let intersection = i64::from((right - left).max(0)) * i64::from((bottom - top).max(0));
    let area_a = i64::from((a[2] - a[0]).max(0)) * i64::from((a[3] - a[1]).max(0));
    let area_b = i64::from((b[2] - b[0]).max(0)) * i64::from((b[3] - b[1]).max(0));
    let union = area_a + area_b - intersection;
    if union <= 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

async fn studio_layout_source(
    studio_url: Option<&str>,
    element: Option<&Element>,
    args: &LayoutSourceArgs,
    target: &LayoutStudioTarget,
) -> Value {
    let bridge = match BridgeClient::new(studio_url) {
        Ok(bridge) => bridge,
        Err(err) => return json!({"available": false, "error": err.to_string()}),
    };
    let text = args
        .selector
        .text
        .as_deref()
        .or_else(|| element.and_then(|element| element.text.as_deref()));
    let rid = args
        .selector
        .rid
        .as_deref()
        .or_else(|| element.and_then(|element| element.rid.as_deref()));
    let class = element.and_then(|element| element.klass.as_deref());
    let draw_id = args.draw_id.map(|draw_id| draw_id.to_string());
    let bounds = element.and_then(|element| {
        element
            .bounds
            .map(|bounds| format!("{},{},{},{}", bounds[0], bounds[1], bounds[2], bounds[3]))
    });
    let params = [
        (query::DRAW_ID, draw_id.as_deref()),
        (query::TEXT, text),
        (query::RID, rid),
        (query::CLASS, class),
        (query::DESC, args.selector.desc.as_deref()),
        (query::BOUNDS, bounds.as_deref()),
    ];
    let params = target.params_with(&params);
    match bridge.get(route::LAYOUT_SOURCE, &params).await {
        Ok(value) => value,
        Err(err) => json!({"available": false, "error": err.to_string()}),
    }
}

fn merge_studio_layout(value: &mut Value, studio: Result<Value>) {
    match studio {
        Ok(studio) => {
            if let Some(features) = studio.get("features").cloned() {
                value["features"] = features;
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
        "ok": false,
        "available": false,
        "reset_requested": reset,
        "reason": reason
    })
}

fn annotate_layout_sample(
    value: &mut Value,
    screen: &ScreenResponse,
    target: Option<&LayoutStudioTarget>,
    studio_required: bool,
) {
    let sample = layout_sample_value(value, screen, target, studio_required);
    value["sample_valid"] = sample.get("valid").cloned().unwrap_or(Value::Bool(false));
    value["sample"] = sample;
}

fn layout_sample_value(
    value: &Value,
    screen: &ScreenResponse,
    target: Option<&LayoutStudioTarget>,
    studio_required: bool,
) -> Value {
    let mut reasons = Vec::<Value>::new();
    let mut next_commands = BTreeSet::<String>::new();
    if screen.element_count == 0 {
        reasons.push(json!({
            "code": "empty_uiautomator_tree",
            "detail": "UiAutomation returned no actionable elements for the active window"
        }));
        next_commands.insert("shadowdroid doctor --fix".into());
        next_commands.insert("shadowdroid ui dump".into());
    }
    if let Some(target) = target {
        if let Some(expected) = target.package.as_deref()
            && screen.current_app.package.as_deref() != Some(expected)
        {
            reasons.push(json!({
                    "code": "foreground_app_mismatch",
                    "expected_package": expected,
                    "actual_package": screen.current_app.package.clone(),
                    "detail": "The sampled UI is not from the package selected for the Layout Inspector target"
                }));
            next_commands.insert(format!("shadowdroid app start {expected}"));
            next_commands.insert(format!("shadowdroid ui wait --pkg {expected}"));
        }
        if let Some(expected) = target.pid.as_deref() {
            let actual = screen.current_app.pid.map(|pid| pid.to_string());
            if actual.as_deref() != Some(expected) {
                reasons.push(json!({
                    "code": "foreground_pid_mismatch",
                    "expected_pid": expected,
                    "actual_pid": actual,
                    "detail": "The app process changed after the Layout Inspector target was selected"
                }));
                next_commands.insert("shadowdroid layout snapshot --compose".into());
            }
        }
    }

    let studio = studio_payload(value);
    if studio_required {
        match studio {
            Some(studio) => {
                let available = studio
                    .get("available")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| studio.get("ok").and_then(Value::as_bool).unwrap_or(false));
                if !available {
                    reasons.push(json!({
                        "code": "layout_inspector_unavailable",
                        "detail": studio_problem_detail(studio)
                    }));
                    next_commands.insert("shadowdroid studio status --json".into());
                    next_commands.insert("shadowdroid init".into());
                    next_commands.insert("shadowdroid doctor".into());
                } else if inspector_node_count(studio) == Some(0) {
                    reasons.push(json!({
                        "code": "empty_layout_inspector_sample",
                        "detail": "Android Studio Layout Inspector returned zero nodes"
                    }));
                    next_commands.insert("shadowdroid layout snapshot --compose".into());
                    next_commands.insert("shadowdroid studio status --json".into());
                }
            }
            None => {
                reasons.push(json!({
                    "code": "layout_inspector_missing",
                    "detail": "This command requires Android Studio Layout Inspector data, but no inspector payload was attached"
                }));
                next_commands.insert("shadowdroid studio status --json".into());
                next_commands.insert("shadowdroid init".into());
            }
        }
    }

    json!({
        "valid": reasons.is_empty(),
        "reasons": reasons,
        "screen_hash": screen.screen_hash,
        "screen_hash_version": screen.screen_hash_version,
        "element_count": screen.element_count,
        "current_app": screen.current_app.clone(),
        "target": target.map(LayoutStudioTarget::to_value),
        "studio_required": studio_required,
        "next_actions": next_commands.into_iter().collect::<Vec<_>>(),
    })
}

fn studio_payload(value: &Value) -> Option<&Value> {
    value.get("android_studio_layout").or_else(|| {
        if value.get("available").is_some() || value.get("activation").is_some() {
            Some(value)
        } else {
            None
        }
    })
}

fn studio_problem_detail(studio: &Value) -> Value {
    studio
        .get("reason")
        .or_else(|| studio.get("error"))
        .or_else(|| studio.get("message"))
        .cloned()
        .unwrap_or_else(|| json!("Android Studio Layout Inspector data is unavailable"))
}

fn inspector_node_count(studio: &Value) -> Option<u64> {
    if let Some(nodes) = studio
        .get("summary")
        .and_then(|summary| summary.get("nodes"))
        .and_then(Value::as_u64)
    {
        return Some(nodes);
    }
    if let Some(nodes) = studio.get("node_count").and_then(Value::as_u64) {
        return Some(nodes);
    }
    if let Some(nodes) = studio.get("nodes").and_then(Value::as_array) {
        return Some(nodes.len() as u64);
    }
    None
}

#[derive(Debug, Clone)]
struct LayoutStudioTarget {
    device: String,
    package: Option<String>,
    pid: Option<String>,
    timeout_ms: String,
}

impl LayoutStudioTarget {
    fn from_ref(
        serial: &Serial,
        app: &AppRef,
        package_override: Option<&str>,
        pid_override: Option<i32>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            device: serial.to_string(),
            package: package_override
                .map(str::to_string)
                .or_else(|| app.package.clone()),
            pid: pid_override.or(app.pid).map(|pid| pid.to_string()),
            timeout_ms: timeout_ms.to_string(),
        }
    }

    fn params(&self) -> Vec<(&str, Option<&str>)> {
        self.params_with(&[])
    }

    fn params_with<'a>(
        &'a self,
        extra: &[(&'a str, Option<&'a str>)],
    ) -> Vec<(&'a str, Option<&'a str>)> {
        let mut params = Vec::with_capacity(4 + extra.len());
        params.push((query::DEVICE, Some(self.device.as_str())));
        params.push((query::PACKAGE, self.package.as_deref()));
        params.push((query::PID, self.pid.as_deref()));
        params.push((query::TIMEOUT_MS, Some(self.timeout_ms.as_str())));
        params.extend(extra.iter().copied());
        params
    }

    fn to_value(&self) -> Value {
        json!({
            "device": self.device.clone(),
            "package": self.package.clone(),
            "pid": self.pid.clone(),
            "timeout_ms": self.timeout_ms.clone(),
        })
    }
}

fn element_matches(element: &Element, args: &LayoutSourceArgs) -> bool {
    if let Some(id) = args.id {
        return element.id == id;
    }
    crate::selector::text_matches(
        element.text.as_deref(),
        args.selector.text.as_deref(),
        false,
    ) && crate::selector::text_matches(element.rid.as_deref(), args.selector.rid.as_deref(), false)
        && crate::selector::text_matches(
            element.desc.as_deref(),
            args.selector.desc.as_deref(),
            false,
        )
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{AppRef, ImeState, Viewport};

    fn screen(package: &str, pid: i32, element_count: u32) -> ScreenResponse {
        ScreenResponse {
            screen_hash: "abc123".into(),
            screen_hash_version: 2,
            content_hash: Some("c:abc123".into()),
            interaction_hash: Some("i:1111111111111111".into()),
            interaction_hash_version: 1,
            snapshot_state: "consistent".into(),
            captured_at_ms: Some(1),
            viewport: Viewport { w: 1, h: 2 },
            current_app: AppRef {
                package: Some(package.into()),
                activity: Some(format!("{package}/.MainActivity")),
                pid: Some(pid),
                sampled_at_ms: Some(1),
            },
            ui_tree: None,
            warning: None,
            element_count,
            ime: ImeState::default(),
            elements: Vec::new(),
        }
    }

    fn accessibility_element(text: &str, bounds: [i32; 4]) -> Element {
        Element {
            id: 1,
            handle: None,
            text: Some(text.into()),
            desc: None,
            klass: Some("android.widget.TextView".into()),
            rid: None,
            bounds: Some(bounds),
            tap: None,
            range: None,
            actions: Vec::new(),
            clickable: false,
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
    fn layout_sample_marks_target_and_inspector_failures_invalid() {
        let target = LayoutStudioTarget {
            device: "emulator-5554".into(),
            package: Some("com.expected".into()),
            pid: Some("42".into()),
            timeout_ms: "5000".into(),
        };
        let value = json!({
            "available": false,
            "reason": "Android Studio Layout Inspector process model is unavailable"
        });
        let sample = layout_sample_value(&value, &screen("com.actual", 24, 3), Some(&target), true);
        assert_eq!(sample["valid"], false);
        let codes = sample["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|reason| reason["code"].as_str())
            .collect::<Vec<_>>();
        assert!(codes.contains(&"foreground_app_mismatch"), "{codes:?}");
        assert!(codes.contains(&"foreground_pid_mismatch"), "{codes:?}");
        assert!(codes.contains(&"layout_inspector_unavailable"), "{codes:?}");
    }

    #[test]
    fn layout_sample_accepts_valid_uiautomator_only_snapshot() {
        let sample = layout_sample_value(&json!({}), &screen("com.app", 7, 2), None, false);
        assert_eq!(sample["valid"], true);
        assert!(sample["reasons"].as_array().unwrap().is_empty());
    }

    #[test]
    fn compose_fallback_extracts_merged_lazy_grid_cards_without_resource_ids() {
        let inspector = json!({
            "windows": [{
                "nodes": [
                    {
                        "draw_id": 101,
                        "kind": "compose",
                        "text": "Profile A",
                        "bounds": {"left": 20, "top": 200, "right": 300, "bottom": 420},
                        "semantics": {"merged": true, "unmerged": false},
                        "compose": {"has_draw_modifier": true}
                    },
                    {
                        "draw_id": 102,
                        "kind": "compose",
                        "text": "Profile B",
                        "bounds": {"left": 320, "top": 200, "right": 600, "bottom": 420},
                        "semantics": {"merged": false, "unmerged": true},
                        "compose": {"has_draw_modifier": false}
                    }
                ]
            }]
        });
        let elements = compose_fallback_elements(&inspector, &[]);
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].id, "cs:101");
        assert_eq!(elements[0].source, "compose_semantics");
        assert_eq!(elements[0].confidence, 1.0);
        assert_eq!(elements[0].tap, [160, 310]);
        assert!(!elements[0].stable_selector);
        assert!(!elements[0].requires_coordinate_fallback);
        assert_eq!(elements[1].id, "cs:102");
    }

    #[test]
    fn compose_fallback_marks_custom_drawing_as_guarded_layout_target() {
        let inspector = json!({
            "windows": [{"nodes": [{
                "draw_id": 900,
                "kind": "compose",
                "text": null,
                "bounds": {"left": 50, "top": 70, "right": 250, "bottom": 270},
                "semantics": {"merged": false, "unmerged": false},
                "compose": {"has_draw_modifier": true}
            }]}]
        });
        let elements = compose_fallback_elements(&inspector, &[]);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].id, "cl:900");
        assert_eq!(elements[0].source, "compose_layout");
        assert_eq!(elements[0].confidence, 0.75);
        assert!(elements[0].requires_coordinate_fallback);
    }

    #[test]
    fn compose_fallback_omits_nodes_already_exported_to_accessibility() {
        let inspector = json!({
            "windows": [{"nodes": [{
                "draw_id": 7,
                "kind": "compose",
                "text": "Edit",
                "bounds": {"left": 10, "top": 10, "right": 80, "bottom": 60},
                "semantics": {"merged": true, "unmerged": false}
            }]}]
        });
        let accessibility = [accessibility_element("Edit", [10, 10, 80, 60])];
        assert!(compose_fallback_elements(&inspector, &accessibility).is_empty());
    }

    #[test]
    fn compose_card_remains_when_only_its_inner_label_is_accessible() {
        let inspector = json!({
            "windows": [{"nodes": [{
                "draw_id": 44,
                "kind": "compose",
                "text": "Profile A",
                "bounds": {"left": 20, "top": 200, "right": 500, "bottom": 600},
                "semantics": {"merged": true, "unmerged": false}
            }]}]
        });
        let accessibility = [accessibility_element("Profile A", [60, 240, 220, 290])];
        let elements = compose_fallback_elements(&inspector, &accessibility);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].id, "cs:44");
    }
}
