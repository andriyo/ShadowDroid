//! Typed declarative network rules.
//!
//! The public representation is intentionally structured: matchers are a
//! Boolean expression and actions state whether they delay, transform, or end
//! a flow. `RuleSpec` still accepts the pre-0.17 `kind` + positional `args`
//! shape when deserializing, but the daemon immediately converts it to this
//! representation and stores only a validated [`CompiledRule`].

use super::{Matcher, SyntheticResponseSpec};
use bytes::Bytes;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleMatchOn {
    /// Match immutable values observed before any earlier rule ran.
    #[default]
    Original,
    /// Match the values produced by earlier rules in this phase.
    Transformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WsDirection {
    C2s,
    S2c,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WsOpcode {
    Text,
    Binary,
    Ping,
    Pong,
    Close,
}

impl WsOpcode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Binary => "binary",
            Self::Ping => "ping",
            Self::Pong => "pong",
            Self::Close => "close",
        }
    }
}

/// A Boolean rule expression. String predicates state their comparison
/// explicitly instead of relying on a command-specific implicit convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleMatcher {
    All { matchers: Vec<RuleMatcher> },
    Any { matchers: Vec<RuleMatcher> },
    Not { matcher: Box<RuleMatcher> },
    Host { contains: String },
    Path { contains: String },
    Method { equals: String },
    Status { equals: u16 },
    ContentType { contains: String },
    GraphqlOperation { equals: String },
    Direction { equals: WsDirection },
    Opcode { equals: WsOpcode },
}

impl Default for RuleMatcher {
    fn default() -> Self {
        Self::All {
            matchers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleTransform {
    MapRemote {
        target: String,
    },
    SetRequestHeader {
        name: String,
        value: String,
    },
    SetStatus {
        status: u16,
    },
    SetResponseHeader {
        name: String,
        value: String,
    },
    ReplaceBody {
        pattern: String,
        replacement: String,
    },
    SetWebsocketText {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleTerminal {
    Block { status: u16 },
    MapLocal { path: PathBuf },
    Respond { response: SyntheticResponseSpec },
    DropWebsocket,
}

/// The top-level category is part of the wire contract. An agent never needs
/// to infer whether an unfamiliar action continues evaluation or terminates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "category", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleAction {
    Delay { milliseconds: u32 },
    Transform { transform: RuleTransform },
    Terminal { terminal: RuleTerminal },
}

/// Agent-facing action value. Synthetic bodies are summarized by length so
/// list/explain output stays bounded and does not echo configured payloads.
pub fn action_summary(action: &RuleAction) -> Value {
    let mut value = serde_json::to_value(action).unwrap_or_else(|_| json!({}));
    if let Value::Object(action_fields) = &mut value
        && let Some(Value::Object(terminal)) = action_fields.get_mut("terminal")
        && let Some(Value::Object(response)) = terminal.get_mut("response")
        && let Some(body) = response.remove("body")
    {
        let body_bytes = body.as_array().map(Vec::len).unwrap_or_default();
        response.insert("body_bytes".into(), json!(body_bytes));
        response.insert("upstream_bypassed".into(), json!(true));
    }
    value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RulePhase {
    Request,
    Response,
    Websocket,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleSpec {
    #[serde(default)]
    pub match_on: RuleMatchOn,
    #[serde(default)]
    pub matcher: RuleMatcher,
    pub action: RuleAction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalRuleSpec {
    #[serde(default)]
    match_on: RuleMatchOn,
    #[serde(default)]
    matcher: RuleMatcher,
    action: RuleAction,
}

/// Decoder for rule files and old CLI/server pairs. Serialization is canonical
/// only, so copying `net rule list` output naturally migrates old rules.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRuleSpec {
    kind: String,
    #[serde(default)]
    matcher: Matcher,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    operation_name: Option<String>,
    #[serde(default)]
    response: Option<SyntheticResponseSpec>,
    #[serde(default)]
    ws_dir: Option<String>,
    #[serde(default)]
    ws_opcode: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

impl<'de> Deserialize<'de> for RuleSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        if value.get("action").is_some() {
            let spec: CanonicalRuleSpec =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            Ok(Self {
                match_on: spec.match_on,
                matcher: spec.matcher,
                action: spec.action,
            })
        } else {
            let legacy: LegacyRuleSpec =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            Self::from_legacy_parts(
                legacy.kind,
                legacy.matcher,
                legacy.content_type,
                legacy.operation_name,
                legacy.response,
                legacy.ws_dir,
                legacy.ws_opcode,
                legacy.args,
            )
            .map_err(serde::de::Error::custom)
        }
    }
}

impl RuleSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn from_legacy_parts(
        kind: String,
        matcher: Matcher,
        content_type: Option<String>,
        operation_name: Option<String>,
        response: Option<SyntheticResponseSpec>,
        ws_dir: Option<String>,
        ws_opcode: Option<String>,
        args: Vec<String>,
    ) -> Result<Self, String> {
        let mut matchers = Vec::new();
        if let Some(contains) = matcher.host {
            matchers.push(RuleMatcher::Host { contains });
        }
        if let Some(contains) = matcher.path {
            matchers.push(RuleMatcher::Path { contains });
        }
        if let Some(equals) = matcher.method {
            matchers.push(RuleMatcher::Method { equals });
        }
        if let Some(equals) = matcher.status {
            matchers.push(RuleMatcher::Status { equals });
        }
        if let Some(contains) = content_type {
            matchers.push(RuleMatcher::ContentType { contains });
        }
        if let Some(equals) = operation_name {
            matchers.push(RuleMatcher::GraphqlOperation { equals });
        }
        if let Some(dir) = ws_dir {
            let equals = match dir.as_str() {
                "c2s" => WsDirection::C2s,
                "s2c" => WsDirection::S2c,
                _ => return Err(format!("invalid WebSocket direction {dir:?}")),
            };
            matchers.push(RuleMatcher::Direction { equals });
        }
        if let Some(opcode) = ws_opcode {
            let equals = match opcode.as_str() {
                "text" => WsOpcode::Text,
                "binary" => WsOpcode::Binary,
                "ping" => WsOpcode::Ping,
                "pong" => WsOpcode::Pong,
                "close" => WsOpcode::Close,
                _ => return Err(format!("invalid WebSocket opcode {opcode:?}")),
            };
            matchers.push(RuleMatcher::Opcode { equals });
        }
        let action = legacy_action(&kind, response, &args)?;
        Ok(Self {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::All { matchers },
            action,
        })
    }

    pub fn phase(&self) -> RulePhase {
        match &self.action {
            RuleAction::Delay { .. } => RulePhase::Request,
            RuleAction::Transform { transform } => match transform {
                RuleTransform::MapRemote { .. } | RuleTransform::SetRequestHeader { .. } => {
                    RulePhase::Request
                }
                RuleTransform::SetStatus { .. }
                | RuleTransform::SetResponseHeader { .. }
                | RuleTransform::ReplaceBody { .. } => RulePhase::Response,
                RuleTransform::SetWebsocketText { .. } => RulePhase::Websocket,
            },
            RuleAction::Terminal { terminal } => match terminal {
                RuleTerminal::Block { .. }
                | RuleTerminal::MapLocal { .. }
                | RuleTerminal::Respond { .. } => RulePhase::Request,
                RuleTerminal::DropWebsocket => RulePhase::Websocket,
            },
        }
    }

    pub fn action_kind(&self) -> &'static str {
        match &self.action {
            RuleAction::Delay { .. } => "delay",
            RuleAction::Transform { transform } => match transform {
                RuleTransform::MapRemote { .. } => "map_remote",
                RuleTransform::SetRequestHeader { .. } => "set_request_header",
                RuleTransform::SetStatus { .. } => "set_status",
                RuleTransform::SetResponseHeader { .. } => "set_response_header",
                RuleTransform::ReplaceBody { .. } => "replace_body",
                RuleTransform::SetWebsocketText { .. } => "set_websocket_text",
            },
            RuleAction::Terminal { terminal } => match terminal {
                RuleTerminal::Block { .. } => "block",
                RuleTerminal::MapLocal { .. } => "map_local",
                RuleTerminal::Respond { .. } => "respond",
                RuleTerminal::DropWebsocket => "drop_websocket",
            },
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.action, RuleAction::Terminal { .. })
    }
}

fn legacy_action(
    kind: &str,
    response: Option<SyntheticResponseSpec>,
    args: &[String],
) -> Result<RuleAction, String> {
    let exact = |wanted: usize| {
        if args.len() == wanted {
            Ok(())
        } else {
            Err(format!(
                "rule `{kind}` needs exactly {wanted} arg(s), got {}",
                args.len()
            ))
        }
    };
    let final_status = |raw: &str| {
        raw.parse::<u16>()
            .map_err(|_| format!("invalid final HTTP status {raw:?}; expected 200..=599"))
    };
    match kind {
        "respond" => {
            exact(0)?;
            Ok(RuleAction::Terminal {
                terminal: RuleTerminal::Respond {
                    response: response.ok_or_else(|| {
                        "respond rule is missing its synthetic response".to_string()
                    })?,
                },
            })
        }
        "block" => {
            if args.len() > 1 {
                return Err(format!(
                    "rule `block` needs zero or one arg, got {}",
                    args.len()
                ));
            }
            Ok(RuleAction::Terminal {
                terminal: RuleTerminal::Block {
                    status: args
                        .first()
                        .map(|s| final_status(s))
                        .transpose()?
                        .unwrap_or(444),
                },
            })
        }
        "delay" => {
            exact(1)?;
            Ok(RuleAction::Delay {
                milliseconds: args[0]
                    .parse()
                    .map_err(|_| format!("invalid delay {:?}; expected a u32", args[0]))?,
            })
        }
        "map-local" => {
            exact(1)?;
            Ok(RuleAction::Terminal {
                terminal: RuleTerminal::MapLocal {
                    path: PathBuf::from(&args[0]),
                },
            })
        }
        "map-remote" => {
            exact(1)?;
            Ok(RuleAction::Transform {
                transform: RuleTransform::MapRemote {
                    target: args[0].clone(),
                },
            })
        }
        "set-status" => {
            exact(1)?;
            Ok(RuleAction::Transform {
                transform: RuleTransform::SetStatus {
                    status: final_status(&args[0])?,
                },
            })
        }
        "set-request-header" => {
            exact(2)?;
            Ok(RuleAction::Transform {
                transform: RuleTransform::SetRequestHeader {
                    name: args[0].clone(),
                    value: args[1].clone(),
                },
            })
        }
        "set-response-header" => {
            exact(2)?;
            Ok(RuleAction::Transform {
                transform: RuleTransform::SetResponseHeader {
                    name: args[0].clone(),
                    value: args[1].clone(),
                },
            })
        }
        "replace" => {
            exact(2)?;
            Ok(RuleAction::Transform {
                transform: RuleTransform::ReplaceBody {
                    pattern: args[0].clone(),
                    replacement: args[1].clone(),
                },
            })
        }
        "ws-drop" => {
            exact(0)?;
            Ok(RuleAction::Terminal {
                terminal: RuleTerminal::DropWebsocket,
            })
        }
        "ws-set-text" => {
            exact(1)?;
            Ok(RuleAction::Transform {
                transform: RuleTransform::SetWebsocketText {
                    text: args[0].clone(),
                },
            })
        }
        other => Err(format!("unknown rule kind {other:?}")),
    }
}

#[derive(Debug, Clone)]
pub struct CompiledLocalResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

/// Runtime form stored by the daemon. Expensive/fallible work is done once,
/// before this value is published into the active rule set.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub spec: RuleSpec,
    pub replace_regex: Option<Regex>,
    pub local_response: Option<CompiledLocalResponse>,
}

pub fn compile_rule(spec: RuleSpec) -> Result<CompiledRule, String> {
    validate_matcher(&spec.matcher)?;
    validate_phase_matchers(&spec)?;
    let mut replace_regex = None;
    let mut local_response = None;
    match &spec.action {
        RuleAction::Delay { .. } => {}
        RuleAction::Transform { transform } => match transform {
            RuleTransform::MapRemote { target } => validate_map_remote_target(target)?,
            RuleTransform::SetRequestHeader { name, value } => {
                validate_header(name, value)?;
                if is_managed_request_header(name) {
                    return Err(format!("request header {name:?} is managed by the proxy"));
                }
            }
            RuleTransform::SetStatus { status } => validate_final_status(*status)?,
            RuleTransform::SetResponseHeader { name, value } => {
                validate_header(name, value)?;
                if is_managed_response_header(name) {
                    return Err(format!(
                        "response framing header {name:?} is managed by the proxy"
                    ));
                }
            }
            RuleTransform::ReplaceBody { pattern, .. } => {
                replace_regex =
                    Some(Regex::new(pattern).map_err(|error| {
                        format!("invalid replacement regex {pattern:?}: {error}")
                    })?);
            }
            RuleTransform::SetWebsocketText { .. } => {}
        },
        RuleAction::Terminal { terminal } => match terminal {
            RuleTerminal::Block { status } => validate_final_status(*status)?,
            RuleTerminal::MapLocal { path } => {
                let metadata = std::fs::metadata(path).map_err(|error| {
                    format!("cannot read map-local file {}: {error}", path.display())
                })?;
                if !metadata.is_file() {
                    return Err(format!(
                        "map-local path is not a regular file: {}",
                        path.display()
                    ));
                }
                let body = std::fs::read(path).map_err(|error| {
                    format!("cannot read map-local file {}: {error}", path.display())
                })?;
                local_response = Some(CompiledLocalResponse {
                    status: 200,
                    headers: vec![("content-type".into(), guess_content_type(path).to_string())],
                    body: Bytes::from(body),
                });
            }
            RuleTerminal::Respond { response } => {
                validate_final_status(response.status)?;
                if response.body.len() > 8 * 1024 * 1024 {
                    return Err(format!(
                        "respond rule body is {} bytes; maximum is 8388608",
                        response.body.len()
                    ));
                }
                for (name, value) in &response.headers {
                    validate_header(name, value)?;
                    if is_managed_response_header(name) {
                        return Err(format!(
                            "response framing header {name:?} is managed by the proxy"
                        ));
                    }
                }
            }
            RuleTerminal::DropWebsocket => {}
        },
    }
    Ok(CompiledRule {
        spec,
        replace_regex,
        local_response,
    })
}

fn validate_matcher(matcher: &RuleMatcher) -> Result<(), String> {
    match matcher {
        RuleMatcher::All { matchers } => {
            for matcher in matchers {
                validate_matcher(matcher)?;
            }
        }
        RuleMatcher::Any { matchers } => {
            if matchers.is_empty() {
                return Err("an `any` matcher must contain at least one matcher".into());
            }
            for matcher in matchers {
                validate_matcher(matcher)?;
            }
        }
        RuleMatcher::Not { matcher } => validate_matcher(matcher)?,
        RuleMatcher::Host { contains }
        | RuleMatcher::Path { contains }
        | RuleMatcher::ContentType { contains } => {
            if contains.trim().is_empty() {
                return Err("substring matcher values must not be empty".into());
            }
        }
        RuleMatcher::Method { equals } | RuleMatcher::GraphqlOperation { equals } => {
            if equals.trim().is_empty() {
                return Err("exact matcher values must not be empty".into());
            }
        }
        RuleMatcher::Status { equals } => {
            if !(100..=599).contains(equals) {
                return Err(format!(
                    "invalid status matcher {equals}; expected 100..=599"
                ));
            }
        }
        RuleMatcher::Direction { .. } | RuleMatcher::Opcode { .. } => {}
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatcherField {
    Host,
    Path,
    Method,
    Status,
    ContentType,
    GraphqlOperation,
    Direction,
    Opcode,
}

fn matcher_fields(matcher: &RuleMatcher, fields: &mut Vec<MatcherField>) {
    match matcher {
        RuleMatcher::All { matchers } | RuleMatcher::Any { matchers } => {
            for matcher in matchers {
                matcher_fields(matcher, fields);
            }
        }
        RuleMatcher::Not { matcher } => matcher_fields(matcher, fields),
        RuleMatcher::Host { .. } => fields.push(MatcherField::Host),
        RuleMatcher::Path { .. } => fields.push(MatcherField::Path),
        RuleMatcher::Method { .. } => fields.push(MatcherField::Method),
        RuleMatcher::Status { .. } => fields.push(MatcherField::Status),
        RuleMatcher::ContentType { .. } => fields.push(MatcherField::ContentType),
        RuleMatcher::GraphqlOperation { .. } => fields.push(MatcherField::GraphqlOperation),
        RuleMatcher::Direction { .. } => fields.push(MatcherField::Direction),
        RuleMatcher::Opcode { .. } => fields.push(MatcherField::Opcode),
    }
}

fn validate_phase_matchers(spec: &RuleSpec) -> Result<(), String> {
    let mut fields = Vec::new();
    matcher_fields(&spec.matcher, &mut fields);
    let invalid = fields.iter().copied().find(|field| match spec.phase() {
        RulePhase::Request => matches!(
            field,
            MatcherField::Status
                | MatcherField::ContentType
                | MatcherField::Direction
                | MatcherField::Opcode
        ),
        RulePhase::Response => matches!(
            field,
            MatcherField::GraphqlOperation | MatcherField::Direction | MatcherField::Opcode
        ),
        RulePhase::Websocket => !matches!(
            field,
            MatcherField::Host | MatcherField::Direction | MatcherField::Opcode
        ),
    });
    if let Some(field) = invalid {
        return Err(format!(
            "matcher field {field:?} is not available in the {:?} phase",
            spec.phase()
        ));
    }
    Ok(())
}

fn validate_final_status(status: u16) -> Result<(), String> {
    if (200..=599).contains(&status) {
        Ok(())
    } else {
        Err(format!(
            "invalid final HTTP status {status}; expected 200..=599"
        ))
    }
}

fn validate_header(name: &str, value: &str) -> Result<(), String> {
    name.parse::<http::header::HeaderName>()
        .map_err(|_| format!("invalid HTTP header name {name:?}"))?;
    http::header::HeaderValue::from_str(value)
        .map_err(|_| format!("invalid HTTP header value for {name:?}"))?;
    Ok(())
}

fn is_managed_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn is_managed_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length"
            | "content-encoding"
            | "transfer-encoding"
            | "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn validate_map_remote_target(target: &str) -> Result<(), String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("map-remote target must not be empty".into());
    }
    let candidate = if target.contains("://") {
        target.to_string()
    } else {
        format!("http://{target}")
    };
    let parsed = reqwest::Url::parse(&candidate)
        .map_err(|error| format!("invalid map-remote target {target:?}: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(format!(
            "map-remote target must contain an http(s) host: {target:?}"
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("map-remote target must not embed credentials".into());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("map-remote target must not contain a query or fragment".into());
    }
    Ok(())
}

fn guess_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("json") => "application/json",
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("xml") => "application/xml",
        Some("txt") => "text/plain",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

pub struct MatchContext<'a> {
    pub host: &'a str,
    pub path: &'a str,
    pub method: &'a str,
    pub status: Option<u16>,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
    pub direction: Option<WsDirection>,
    pub opcode: Option<&'a str>,
}

impl RuleMatcher {
    pub fn matches(&self, context: &MatchContext<'_>) -> bool {
        match self {
            Self::All { matchers } => matchers.iter().all(|matcher| matcher.matches(context)),
            Self::Any { matchers } => matchers.iter().any(|matcher| matcher.matches(context)),
            Self::Not { matcher } => !matcher.matches(context),
            Self::Host { contains } => contains_ci(context.host, contains),
            Self::Path { contains } => contains_ci(context.path, contains),
            Self::Method { equals } => context.method.eq_ignore_ascii_case(equals),
            Self::Status { equals } => context.status == Some(*equals),
            Self::ContentType { contains } => context
                .content_type
                .is_some_and(|value| contains_ci(value, contains)),
            Self::GraphqlOperation { equals } => {
                graphql_operation_matches(equals, context.path, context.body)
            }
            Self::Direction { equals } => context.direction == Some(*equals),
            Self::Opcode { equals } => context
                .opcode
                .is_some_and(|opcode| opcode.eq_ignore_ascii_case(equals.as_str())),
        }
    }

    pub fn explain(&self, context: &MatchContext<'_>) -> Value {
        match self {
            Self::All { matchers } => {
                let children: Vec<_> = matchers
                    .iter()
                    .map(|matcher| matcher.explain(context))
                    .collect();
                json!({"type": "all", "matched": children.iter().all(|v| v["matched"] == true), "children": children})
            }
            Self::Any { matchers } => {
                let children: Vec<_> = matchers
                    .iter()
                    .map(|matcher| matcher.explain(context))
                    .collect();
                json!({"type": "any", "matched": children.iter().any(|v| v["matched"] == true), "children": children})
            }
            Self::Not { matcher } => {
                let child = matcher.explain(context);
                json!({"type": "not", "matched": child["matched"] != true, "child": child})
            }
            _ => json!({"predicate": self, "matched": self.matches(context)}),
        }
    }

    /// Whether a WebSocket rule could match some opcode in this direction.
    /// Used to decide whether the tap must enter managed-frame mode before the
    /// next frame's opcode is known.
    pub fn could_match_websocket_direction(&self, host: &str, direction: WsDirection) -> bool {
        // Opcode is a finite domain. Evaluate the complete Boolean expression
        // for each legal value instead of negating existential projections:
        // `not(opcode=text)` still matches binary, ping, pong, and close.
        ["text", "binary", "ping", "pong", "close"]
            .into_iter()
            .any(|opcode| {
                self.matches(&MatchContext {
                    host,
                    path: "",
                    method: "",
                    status: None,
                    content_type: None,
                    body: &[],
                    direction: Some(direction),
                    opcode: Some(opcode),
                })
            })
    }
}

fn contains_ci(value: &str, needle: &str) -> bool {
    value.to_lowercase().contains(&needle.to_lowercase())
}

fn graphql_operation_matches(wanted: &str, path: &str, body: &[u8]) -> bool {
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if reqwest::Url::parse(&format!("http://shadowdroid.invalid{path}"))
        .ok()
        .into_iter()
        .flat_map(|url| {
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>()
        })
        .any(|(key, value)| key == "operationName" && value == wanted)
    {
        return true;
    }
    fn json_matches(value: &Value, wanted: &str) -> bool {
        match value {
            Value::Object(object) => {
                object.get("operationName").and_then(Value::as_str) == Some(wanted)
            }
            Value::Array(values) => values.iter().any(|value| json_matches(value, wanted)),
            _ => false,
        }
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .is_some_and(|value| json_matches(&value, wanted))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleLintIssue {
    pub severity: &'static str,
    pub code: &'static str,
    pub rule_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_rule_index: Option<usize>,
    pub message: String,
}

/// Structural lint deliberately reports only relationships it can prove. It
/// never labels two arbitrary substring expressions as overlapping based on a
/// guess.
pub fn lint_rules(specs: &[RuleSpec]) -> Vec<RuleLintIssue> {
    let mut issues = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        if let Err(message) = compile_rule(spec.clone()) {
            issues.push(RuleLintIssue {
                severity: "error",
                code: "invalid_rule",
                rule_index: index,
                related_rule_index: None,
                message,
            });
        }
        if matcher_is_impossible(&spec.matcher) {
            issues.push(RuleLintIssue {
                severity: "error",
                code: "impossible_matcher",
                rule_index: index,
                related_rule_index: None,
                message: "the Boolean matcher can never be true".into(),
            });
        }
        for (earlier_index, earlier) in specs[..index].iter().enumerate() {
            if earlier.phase() != spec.phase() {
                continue;
            }
            let same_matcher = earlier.matcher == spec.matcher && earlier.match_on == spec.match_on;
            if earlier.is_terminal()
                && (same_matcher || matcher_matches_everything(&earlier.matcher))
            {
                issues.push(RuleLintIssue {
                    severity: "warning",
                    code: "shadowed_rule",
                    rule_index: index,
                    related_rule_index: Some(earlier_index),
                    message: format!(
                        "terminal rule {earlier_index} runs first and makes this rule unreachable"
                    ),
                });
            } else if same_matcher && earlier.action == spec.action {
                issues.push(RuleLintIssue {
                    severity: "warning",
                    code: "duplicate_rule",
                    rule_index: index,
                    related_rule_index: Some(earlier_index),
                    message: format!("rule {earlier_index} has the same matcher and action"),
                });
            } else if same_matcher {
                issues.push(RuleLintIssue {
                    severity: "warning",
                    code: "order_sensitive_rules",
                    rule_index: index,
                    related_rule_index: Some(earlier_index),
                    message: format!(
                        "rule {earlier_index} has the same matcher; changing their order may change the result"
                    ),
                });
            }
        }
    }
    issues
}

fn matcher_matches_everything(matcher: &RuleMatcher) -> bool {
    matches!(matcher, RuleMatcher::All { matchers } if matchers.is_empty())
}

fn matcher_is_impossible(matcher: &RuleMatcher) -> bool {
    match matcher {
        RuleMatcher::Any { matchers } => {
            matchers.is_empty() || matchers.iter().all(matcher_is_impossible)
        }
        RuleMatcher::All { matchers } => {
            if matchers.iter().any(matcher_is_impossible) {
                return true;
            }
            for (index, left) in matchers.iter().enumerate() {
                for right in &matchers[index + 1..] {
                    if matches!(right, RuleMatcher::Not { matcher } if matcher.as_ref() == left)
                        || matches!(left, RuleMatcher::Not { matcher } if matcher.as_ref() == right)
                    {
                        return true;
                    }
                    match (left, right) {
                        (RuleMatcher::Method { equals: a }, RuleMatcher::Method { equals: b })
                            if !a.eq_ignore_ascii_case(b) =>
                        {
                            return true;
                        }
                        (RuleMatcher::Status { equals: a }, RuleMatcher::Status { equals: b })
                            if a != b =>
                        {
                            return true;
                        }
                        (
                            RuleMatcher::Direction { equals: a },
                            RuleMatcher::Direction { equals: b },
                        ) if a != b => return true,
                        (RuleMatcher::Opcode { equals: a }, RuleMatcher::Opcode { equals: b })
                            if a != b =>
                        {
                            return true;
                        }
                        _ => {}
                    }
                }
            }
            false
        }
        RuleMatcher::Not { matcher } => matcher_matches_everything(matcher),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_rule_decodes_to_canonical_typed_shape() {
        let spec: RuleSpec = serde_json::from_value(json!({
            "kind": "set-status",
            "matcher": {"host": "api", "status": 200},
            "args": ["503"]
        }))
        .unwrap();
        assert_eq!(spec.phase(), RulePhase::Response);
        assert_eq!(spec.action_kind(), "set_status");
        let value = serde_json::to_value(spec).unwrap();
        assert!(value.get("action").is_some());
        assert!(value.get("kind").is_none());
    }

    #[test]
    fn canonical_rule_round_trips_and_rejects_unknown_fields() {
        let spec = RuleSpec {
            match_on: RuleMatchOn::Transformed,
            matcher: RuleMatcher::Any {
                matchers: vec![
                    RuleMatcher::Host {
                        contains: "api".into(),
                    },
                    RuleMatcher::Not {
                        matcher: Box::new(RuleMatcher::Path {
                            contains: "/health".into(),
                        }),
                    },
                ],
            },
            action: RuleAction::Transform {
                transform: RuleTransform::MapRemote {
                    target: "mirror.test".into(),
                },
            },
        };
        let value = serde_json::to_value(&spec).unwrap();
        assert_eq!(serde_json::from_value::<RuleSpec>(value).unwrap(), spec);

        let unknown = json!({
            "matcher": {"type": "host", "contains": "api", "typo": true},
            "action": {"category": "delay", "milliseconds": 1}
        });
        assert!(serde_json::from_value::<RuleSpec>(unknown).is_err());
    }

    #[test]
    fn boolean_matchers_are_executable_and_explainable() {
        let matcher = RuleMatcher::All {
            matchers: vec![
                RuleMatcher::Host {
                    contains: "api".into(),
                },
                RuleMatcher::Not {
                    matcher: Box::new(RuleMatcher::Path {
                        contains: "/health".into(),
                    }),
                },
                RuleMatcher::Any {
                    matchers: vec![
                        RuleMatcher::Method {
                            equals: "POST".into(),
                        },
                        RuleMatcher::Method {
                            equals: "PUT".into(),
                        },
                    ],
                },
            ],
        };
        let context = MatchContext {
            host: "api.example.com",
            path: "/v1/users",
            method: "post",
            status: None,
            content_type: None,
            body: &[],
            direction: None,
            opcode: None,
        };
        assert!(matcher.matches(&context));
        assert_eq!(matcher.explain(&context)["matched"], true);
    }

    #[test]
    fn compiler_resolves_regex_and_local_file_before_publish() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("response.json");
        std::fs::write(&file, br#"{"ok":true}"#).unwrap();
        let local = RuleSpec {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::default(),
            action: RuleAction::Terminal {
                terminal: RuleTerminal::MapLocal { path: file.clone() },
            },
        };
        let compiled = compile_rule(local).unwrap();
        std::fs::write(&file, br#"{"changed":true}"#).unwrap();
        assert_eq!(
            compiled.local_response.unwrap().body,
            br#"{"ok":true}"#[..],
            "the active rule must use bytes compiled before publication"
        );

        let replace = RuleSpec {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::default(),
            action: RuleAction::Transform {
                transform: RuleTransform::ReplaceBody {
                    pattern: "(".into(),
                    replacement: "x".into(),
                },
            },
        };
        assert!(compile_rule(replace).is_err());
    }

    #[test]
    fn compiler_rejects_content_encoding_response_transform() {
        let spec = RuleSpec {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::default(),
            action: RuleAction::Transform {
                transform: RuleTransform::SetResponseHeader {
                    name: "Content-Encoding".into(),
                    value: "gzip".into(),
                },
            },
        };

        let error = compile_rule(spec).unwrap_err();
        assert!(error.contains("managed by the proxy"), "{error}");
    }

    #[test]
    fn compiler_rejects_content_encoding_on_terminal_response() {
        let spec = RuleSpec {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::default(),
            action: RuleAction::Terminal {
                terminal: RuleTerminal::Respond {
                    response: SyntheticResponseSpec {
                        status: 200,
                        headers: vec![("content-encoding".into(), "gzip".into())],
                        body: b"uncompressed".to_vec(),
                    },
                },
            },
        };

        let error = compile_rule(spec).unwrap_err();
        assert!(error.contains("managed by the proxy"), "{error}");
    }

    #[test]
    fn lint_proves_shadowing_and_contradictions() {
        let terminal = RuleSpec {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::default(),
            action: RuleAction::Terminal {
                terminal: RuleTerminal::Block { status: 503 },
            },
        };
        let impossible = RuleSpec {
            match_on: RuleMatchOn::Original,
            matcher: RuleMatcher::All {
                matchers: vec![
                    RuleMatcher::Method {
                        equals: "GET".into(),
                    },
                    RuleMatcher::Method {
                        equals: "POST".into(),
                    },
                ],
            },
            action: RuleAction::Delay { milliseconds: 1 },
        };
        let issues = lint_rules(&[terminal, impossible]);
        assert!(issues.iter().any(|issue| issue.code == "shadowed_rule"));
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "impossible_matcher")
        );
    }

    #[test]
    fn websocket_direction_projection_handles_nested_boolean_opcode_logic() {
        let matcher = RuleMatcher::All {
            matchers: vec![
                RuleMatcher::Host {
                    contains: "chat".into(),
                },
                RuleMatcher::Direction {
                    equals: WsDirection::C2s,
                },
                RuleMatcher::Not {
                    matcher: Box::new(RuleMatcher::Any {
                        matchers: vec![
                            RuleMatcher::Opcode {
                                equals: WsOpcode::Text,
                            },
                            RuleMatcher::Opcode {
                                equals: WsOpcode::Close,
                            },
                        ],
                    }),
                },
            ],
        };
        assert!(matcher.could_match_websocket_direction("chat.example", WsDirection::C2s));
        assert!(!matcher.could_match_websocket_direction("chat.example", WsDirection::S2c));
        assert!(!matcher.could_match_websocket_direction("api.example", WsDirection::C2s));

        let impossible = RuleMatcher::All {
            matchers: vec![
                RuleMatcher::Opcode {
                    equals: WsOpcode::Text,
                },
                RuleMatcher::Not {
                    matcher: Box::new(RuleMatcher::Opcode {
                        equals: WsOpcode::Text,
                    }),
                },
            ],
        };
        assert!(!impossible.could_match_websocket_direction("chat.example", WsDirection::C2s));
    }
}
