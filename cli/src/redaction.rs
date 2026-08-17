//! Deterministic output/capture redaction shared by every command.
//!
//! The policy operates only on copies destined for stdout or diagnostic
//! artifacts, plus completed network capture records. It never mutates bytes
//! forwarded between the app and its upstream server.

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::net::Ipv6Addr;
use std::str::FromStr;
use std::sync::{LazyLock, OnceLock, RwLock};

pub const POLICY_VERSION: u32 = 2;

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,63}\b").expect("email regex")
});
static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]{2,}\.[A-Za-z0-9_-]{2,}\.[A-Za-z0-9_-]{2,}\b")
        .expect("JWT regex")
});
static BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+").expect("bearer regex"));
static HEADER_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)\b(authorization|proxy-authorization|cookie|set-cookie)\s*:\s*([^\r\n]+)")
        .expect("header-line regex")
});
static QUERY_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(access_token|refresh_token|token|session_id|device_id|transaction_id|password|passcode|api_key|email)=([^&\s]+)")
        .expect("query secret regex")
});
static URLISH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:https?|wss?)://[^\s\"'<>]+"#).expect("URL-like text regex")
});
static JSON_STRING_PAIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)"([A-Za-z0-9_.-]+)"\s*:\s*"((?:\\.|[^"\\])*)""#)
        .expect("JSON string-pair regex")
});
static ESCAPED_JSON_STRING_PAIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\\"([A-Za-z0-9_.-]+)\\"\s*:\s*\\"((?:\\\\.|[^\\"])*)\\""#)
        .expect("escaped JSON string-pair regex")
});
static IPV4: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])){3}\b",
    )
    .expect("IPv4 regex")
});
static IPV6_CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[0-9A-Fa-f]*:[0-9A-Fa-f:]+").expect("IPv6 candidate regex"));
static REDACTION_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:<redacted:[a-z][a-z0-9_-]*>|%3Credacted%3A[a-z][a-z0-9_-]*%3E)")
        .expect("redaction placeholder regex")
});

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySpec {
    #[serde(default)]
    pub json_keys: Vec<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Policy {
    spec: PolicySpec,
    custom_keys: BTreeSet<String>,
    custom_patterns: Vec<Regex>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PixelRedactionReport {
    pub method: &'static str,
    pub regions_redacted: usize,
    pub potentially_sensitive: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Change {
    values: usize,
    body: bool,
}

impl Change {
    fn merge(&mut self, other: Self) {
        self.values += other.values;
        self.body |= other.body;
    }
}

fn split_redaction_metadata(prior: Option<Value>) -> (serde_json::Map<String, Value>, Value) {
    let Some(Value::Object(mut existing)) = prior else {
        return (serde_json::Map::new(), Value::Object(Default::default()));
    };

    // These fields are regenerated below. Removing them also prevents an
    // already-redacted record from counting its own policy label on pass two.
    for key in [
        "enabled",
        "policy",
        "version",
        "redacted_values",
        "custom_json_keys",
        "custom_patterns",
    ] {
        existing.remove(key);
    }

    let mut safe = serde_json::Map::new();
    for key in ["screenshot_pixels_requested", "video_pixels"] {
        if existing.get(key).is_some_and(Value::is_boolean) {
            safe.insert(
                key.to_string(),
                existing.remove(key).expect("checked metadata field exists"),
            );
        }
    }
    if existing
        .get("metadata")
        .and_then(Value::as_str)
        .is_some_and(|value| {
            matches!(
                value,
                "marker_labels_only" | "marker_labels_builtin_or_configured_v1" | "not_requested"
            )
        })
    {
        safe.insert(
            "metadata".to_string(),
            existing
                .remove("metadata")
                .expect("checked metadata field exists"),
        );
    }

    (safe, Value::Object(existing))
}

impl Policy {
    pub fn new(mut spec: PolicySpec) -> Result<Self> {
        dedupe(&mut spec.json_keys);
        dedupe(&mut spec.patterns);
        let custom_keys = spec
            .json_keys
            .iter()
            .map(|key| normalize_key(key))
            .filter(|key| !key.is_empty())
            .collect();
        let mut custom_patterns = Vec::with_capacity(spec.patterns.len());
        for (index, pattern) in spec.patterns.iter().enumerate() {
            let compiled = Regex::new(pattern).map_err(|error| {
                crate::diagnostic::DiagnosticError::new(
                    "invalid_redaction_pattern",
                    "config",
                    format!("redaction.patterns[{index}] is not a valid regular expression"),
                )
                .detail(json!({"pattern_index": index, "error": error.to_string()}))
                .next_actions([
                    "fix or remove the invalid redaction pattern",
                    "shadowdroid config validate --json",
                ])
            })?;
            let minimum_len = regex_syntax::Parser::new()
                .parse(pattern)
                .ok()
                .and_then(|hir| hir.properties().minimum_len());
            if minimum_len.is_none_or(|length| length == 0) {
                let reason = if minimum_len == Some(0) {
                    "pattern_can_match_empty_text"
                } else {
                    "pattern_minimum_length_unknown"
                };
                return Err(crate::diagnostic::DiagnosticError::new(
                    "invalid_redaction_pattern",
                    "config",
                    format!("redaction.patterns[{index}] must consume at least one character"),
                )
                .detail(json!({
                    "pattern_index": index,
                    "reason": reason
                }))
                .next_actions([
                    "make the pattern match the sensitive text itself, not only a boundary",
                    "shadowdroid config validate --json",
                ])
                .into());
            }
            custom_patterns.push(compiled);
        }
        Ok(Self {
            spec,
            custom_keys,
            custom_patterns,
        })
    }

    pub fn builtin() -> Self {
        Self::new(PolicySpec::default()).expect("built-in redaction policy is valid")
    }

    pub fn spec(&self) -> &PolicySpec {
        &self.spec
    }

    pub fn label(&self) -> &'static str {
        if self.spec.json_keys.is_empty() && self.spec.patterns.is_empty() {
            "builtin"
        } else {
            "builtin+config"
        }
    }

    pub fn fingerprint(&self) -> String {
        let bytes = serde_json::to_vec(&(POLICY_VERSION, &self.spec))
            .expect("redaction policy spec is serializable");
        blake3::hash(&bytes).to_hex().to_string()
    }

    pub fn redact_output(&self, mut value: Value) -> Value {
        // `/redaction` is a reserved metadata namespace. Canonical fields and
        // explicitly typed privacy flags stay outside configured matching so a
        // pattern such as `builtin` cannot rewrite metadata on every pass.
        // Every unknown member is still untrusted output and goes through the
        // complete redaction pipeline before it is restored.
        let prior_redaction = match &mut value {
            Value::Object(map) => map.remove("redaction"),
            _ => None,
        };
        let previously_redacted = prior_redaction
            .as_ref()
            .and_then(|metadata| metadata.get("redacted_values"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let (mut metadata, mut untrusted_metadata) = split_redaction_metadata(prior_redaction);
        let mut literals = BTreeMap::new();
        self.collect_sensitive_literals(&value, &mut literals);
        self.collect_sensitive_literals(&untrusted_metadata, &mut literals);
        let mut change = self.redact_value(&mut value);
        change.merge(self.redact_value(&mut untrusted_metadata));
        change.values += redact_known_literals(&mut value, &literals);
        change.values += redact_known_literals(&mut untrusted_metadata, &literals);
        let redacted_values = previously_redacted.saturating_add(change.values as u64);
        if let Value::Object(map) = &mut value {
            if let Value::Object(untrusted_metadata) = untrusted_metadata {
                metadata.extend(untrusted_metadata);
            }
            metadata.insert("enabled".into(), true.into());
            metadata.insert("policy".into(), self.label().into());
            metadata.insert("version".into(), POLICY_VERSION.into());
            metadata.insert("redacted_values".into(), redacted_values.into());
            metadata.insert("custom_json_keys".into(), self.spec.json_keys.len().into());
            metadata.insert("custom_patterns".into(), self.spec.patterns.len().into());
            map.insert("redaction".into(), Value::Object(metadata));
            if change.body {
                map.insert("body_redacted".into(), true.into());
            }
        }
        value
    }

    pub fn redact_json_value(&self, value: &Value) -> Value {
        self.redact_output(value.clone())
    }

    pub fn redact_text(&self, text: &str) -> String {
        self.redact_string(text).0
    }

    /// Redact absolute URLs embedded in diagnostic text, including
    /// percent-encoded query names that the generic text patterns cannot
    /// classify without decoding.
    pub fn redact_urlish_text(&self, text: &str) -> String {
        let with_urls = URLISH
            .replace_all(text, |captures: &regex::Captures<'_>| {
                let url = &captures[0];
                let Some((scheme, rest)) = url.split_once("://") else {
                    return url.to_string();
                };
                let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
                let (authority, target) = rest.split_at(split);
                let authority = self.redact_text(authority);
                let (target, _) = self.redact_request_target(target);
                format!("{scheme}://{authority}{target}")
            })
            .into_owned();
        self.redact_text(&with_urls)
    }

    pub fn text_is_sensitive(&self, text: &str) -> bool {
        self.redact_string(text).1 > 0
    }

    pub fn redact_header_value(&self, name: &str, value: &str) -> String {
        if is_redaction_placeholder(value) {
            return value.to_string();
        }
        sensitive_kind(name, &self.custom_keys)
            .map(|kind| placeholder(kind, Some(value)).to_string())
            .unwrap_or_else(|| self.redact_text(value))
    }

    pub fn redact_body(&self, body: &str) -> (String, bool) {
        let (redacted, count) = self.redact_string(body);
        (redacted, count > 0)
    }

    /// Redact a persisted HTTP/WebSocket request target without changing the
    /// bytes sent on the wire. Query names are decoded before classification so
    /// encoded spellings such as `access%5Ftoken` cannot bypass the policy;
    /// unchanged components retain their original encoding and order.
    pub fn redact_request_target(&self, target: &str) -> (String, bool) {
        let (path_and_query, fragment) = target
            .split_once('#')
            .map_or((target, None), |(head, tail)| (head, Some(tail)));
        let (path, query) = path_and_query
            .split_once('?')
            .map_or((path_and_query, None), |(head, tail)| (head, Some(tail)));

        let mut changed = false;
        let redacted_path = path
            .split('/')
            .map(|segment| self.redact_encoded_component(segment, &mut changed))
            .collect::<Vec<_>>()
            .join("/");
        let mut output = redacted_path;

        if let Some(query) = query {
            output.push('?');
            output.push_str(
                &query
                    .split('&')
                    .map(|parameter| {
                        let Some((raw_name, raw_value)) = parameter.split_once('=') else {
                            return self.redact_encoded_component(parameter, &mut changed);
                        };
                        let decoded_name = decode_url_component(raw_name);
                        let redacted_value = if let Some(kind) =
                            sensitive_kind(decoded_name.as_ref(), &self.custom_keys)
                        {
                            if raw_value.is_empty() {
                                String::new()
                            } else {
                                let decoded_value = decode_url_component(raw_value);
                                if is_redaction_placeholder(decoded_value.as_ref()) {
                                    raw_value.to_string()
                                } else {
                                    let replacement = urlencoding::encode(placeholder(
                                        kind,
                                        Some(decoded_value.as_ref()),
                                    ))
                                    .into_owned();
                                    changed |= replacement != raw_value;
                                    replacement
                                }
                            }
                        } else {
                            self.redact_encoded_component(raw_value, &mut changed)
                        };
                        format!("{raw_name}={redacted_value}")
                    })
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }

        if let Some(fragment) = fragment {
            output.push('#');
            output.push_str(&self.redact_encoded_component(fragment, &mut changed));
        }
        (output, changed)
    }

    fn redact_encoded_component(&self, raw: &str, changed: &mut bool) -> String {
        let decoded = decode_url_component(raw);
        let redacted = self.redact_text(decoded.as_ref());
        if redacted == decoded {
            raw.to_string()
        } else {
            *changed = true;
            urlencoding::encode(&redacted).into_owned()
        }
    }

    pub fn redact_flow_record(&self, flow: &mut crate::net::flow::FlowRecord) {
        let host = self.redact_text(&flow.host);
        flow.host_redacted |= host != flow.host;
        flow.host = host;

        let (path, path_changed) = self.redact_request_target(&flow.path);
        flow.path_redacted |= path_changed;
        flow.path = path;

        if let Some(error) = flow.error.as_mut() {
            let redacted = self.redact_urlish_text(error);
            flow.error_redacted |= redacted != *error;
            *error = redacted;
        }
        for (name, value) in &mut flow.req_headers {
            *value = self.redact_header_value(name, value);
        }
        for (name, value) in &mut flow.resp_headers {
            *value = self.redact_header_value(name, value);
        }
        if let Some(body) = &mut flow.req_body {
            let (redacted, changed) = self.redact_body(body);
            *body = redacted;
            flow.req_body_redacted |= changed;
        }
        if let Some(body) = &mut flow.resp_body {
            let (redacted, changed) = self.redact_body(body);
            *body = redacted;
            flow.resp_body_redacted |= changed;
        }
        flow.redaction_policy = Some(self.label().to_string());
        flow.redaction_policy_version = Some(POLICY_VERSION);
    }

    fn redact_value(&self, value: &mut Value) -> Change {
        match value {
            Value::Object(map) => {
                let keys = map.keys().cloned().collect::<Vec<_>>();
                let mut total = Change::default();
                let mut body_flags = Vec::new();
                for key in keys {
                    let Some(entry) = map.get_mut(&key) else {
                        continue;
                    };
                    if let Some(kind) = sensitive_kind(&key, &self.custom_keys) {
                        if kind == "id" && matches!(entry, Value::Object(_) | Value::Array(_)) {
                            total.merge(self.redact_value(entry));
                            continue;
                        }
                        if !is_placeholder(entry) {
                            let hint = entry.as_str();
                            *entry = Value::String(placeholder(kind, hint).to_string());
                            total.values += 1;
                        }
                        continue;
                    }
                    let nested = self.redact_value(entry);
                    if is_body_key(&key) && nested.values > 0 {
                        body_flags.push(format!("{key}_redacted"));
                        total.body = true;
                    }
                    total.merge(nested);
                }
                if !body_flags.is_empty() {
                    map.insert("body_redacted".into(), true.into());
                    for flag in body_flags {
                        map.insert(flag, true.into());
                    }
                }
                total
            }
            Value::Array(values) => {
                let mut total = Change::default();
                for value in values {
                    total.merge(self.redact_value(value));
                }
                total
            }
            Value::String(text) => {
                let (redacted, count) = self.redact_string(text);
                if count > 0 {
                    *text = redacted;
                }
                Change {
                    values: count,
                    body: false,
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => Change::default(),
        }
    }

    fn collect_sensitive_literals(
        &self,
        value: &Value,
        literals: &mut BTreeMap<String, &'static str>,
    ) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if let Some(kind) = sensitive_kind(key, &self.custom_keys)
                        && let Some(value) = value.as_str()
                        && value.len() >= 4
                        && !is_redaction_placeholder(value)
                    {
                        literals.insert(value.to_string(), placeholder(kind, Some(value)));
                    }
                    self.collect_sensitive_literals(value, literals);
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.collect_sensitive_literals(value, literals);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    fn redact_string(&self, input: &str) -> (String, usize) {
        if is_redaction_placeholder(input) {
            return (input.to_string(), 0);
        }
        let trimmed = input.trim();
        let mut output = input.to_string();
        let mut changes = 0usize;

        // Logcat and GraphQL bodies frequently carry a complete JSON document
        // inside one string. Redact its nested values structurally and return
        // the serialized document immediately. Running the line-oriented
        // header patterns over serialized JSON can consume closing quotes and
        // the rest of the object (for example when a value contains
        // `Authorization: Bearer ...`), which would make stdout invalid JSON.
        // `redact_value` already applies all generic and configured string
        // patterns recursively to the document's string values.
        if (trimmed.starts_with('{') || trimmed.starts_with('['))
            && let Ok(mut nested) = serde_json::from_str::<Value>(trimmed)
        {
            let nested_change = self.redact_value(&mut nested);
            if let Ok(serialized) = serde_json::to_string(&nested) {
                return (serialized, nested_change.values);
            }
        }

        output = redact_json_string_pairs(
            &output,
            &self.custom_keys,
            &JSON_STRING_PAIR,
            false,
            &mut changes,
        );
        output = redact_json_string_pairs(
            &output,
            &self.custom_keys,
            &ESCAPED_JSON_STRING_PAIR,
            true,
            &mut changes,
        );
        output = replace_count(&EMAIL, &output, "<redacted:email>", &mut changes);
        output = replace_count(&JWT, &output, "<redacted:jwt>", &mut changes);
        output = replace_count(&BEARER, &output, "<redacted:token>", &mut changes);
        output = HEADER_LINE
            .replace_all(&output, |captures: &regex::Captures<'_>| {
                let original = &captures[0];
                if is_redaction_placeholder(captures[2].trim()) {
                    return original.to_string();
                }
                let replacement = format!("{}: <redacted:token>", &captures[1]);
                if replacement != original {
                    changes += 1;
                }
                replacement
            })
            .into_owned();
        output = QUERY_SECRET
            .replace_all(&output, |captures: &regex::Captures<'_>| {
                let original = &captures[0];
                if is_redaction_placeholder(&captures[2]) {
                    return original.to_string();
                }
                let replacement = format!("{}=<redacted:secret>", &captures[1]);
                if replacement != original {
                    changes += 1;
                }
                replacement
            })
            .into_owned();
        output = replace_count(&IPV4, &output, "<redacted:ip>", &mut changes);
        output = IPV6_CANDIDATE
            .replace_all(&output, |captures: &regex::Captures<'_>| {
                let candidate = &captures[0];
                if Ipv6Addr::from_str(candidate).is_ok() {
                    changes += 1;
                    "<redacted:ip>".to_string()
                } else {
                    candidate.to_string()
                }
            })
            .into_owned();
        for pattern in &self.custom_patterns {
            output = replace_count(pattern, &output, "<redacted:pattern>", &mut changes);
        }
        (output, changes)
    }
}

fn decode_url_component(input: &str) -> Cow<'_, str> {
    urlencoding::decode(input).unwrap_or(Cow::Borrowed(input))
}

fn redact_known_literals(value: &mut Value, literals: &BTreeMap<String, &'static str>) -> usize {
    match value {
        Value::Object(map) => map
            .values_mut()
            .map(|value| redact_known_literals(value, literals))
            .sum(),
        Value::Array(values) => values
            .iter_mut()
            .map(|value| redact_known_literals(value, literals))
            .sum(),
        Value::String(text) => {
            let mut replacements = 0usize;
            let mut ordered = literals.iter().collect::<Vec<_>>();
            ordered.sort_by_key(|(literal, _)| std::cmp::Reverse(literal.len()));
            for (literal, placeholder) in ordered {
                let (redacted, count) =
                    replace_literal_preserving_placeholders(text, literal, placeholder);
                if count > 0 {
                    *text = redacted;
                    replacements += count;
                }
            }
            replacements
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

fn redact_json_string_pairs(
    input: &str,
    custom_keys: &BTreeSet<String>,
    regex: &Regex,
    escaped: bool,
    changes: &mut usize,
) -> String {
    regex
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let key = &captures[1];
            let value = &captures[2];
            let Some(kind) = sensitive_kind(key, custom_keys) else {
                return captures[0].to_string();
            };
            if is_redaction_placeholder(value) {
                return captures[0].to_string();
            }
            *changes += 1;
            let replacement = placeholder(kind, Some(value));
            if escaped {
                format!(r#"\"{key}\":\"{replacement}\""#)
            } else {
                format!(r#""{key}":"{replacement}""#)
            }
        })
        .into_owned()
}

fn replace_count(regex: &Regex, input: &str, replacement: &str, changes: &mut usize) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0usize;
    for placeholder in REDACTION_PLACEHOLDER.find_iter(input) {
        output.push_str(&replace_count_in_unredacted_span(
            regex,
            &input[cursor..placeholder.start()],
            replacement,
            changes,
        ));
        output.push_str(placeholder.as_str());
        cursor = placeholder.end();
    }
    output.push_str(&replace_count_in_unredacted_span(
        regex,
        &input[cursor..],
        replacement,
        changes,
    ));
    output
}

fn replace_count_in_unredacted_span(
    regex: &Regex,
    input: &str,
    replacement: &str,
    changes: &mut usize,
) -> String {
    regex
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let matched = captures.get(0).expect("regex capture zero exists");
            if matched.as_str() == replacement {
                return matched.as_str().to_string();
            }
            *changes += 1;
            replacement.to_string()
        })
        .into_owned()
}

fn replace_literal_preserving_placeholders(
    input: &str,
    literal: &str,
    replacement: &str,
) -> (String, usize) {
    if literal.is_empty() || literal == replacement {
        return (input.to_string(), 0);
    }
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0usize;
    let mut changes = 0usize;
    for placeholder in REDACTION_PLACEHOLDER.find_iter(input) {
        let span = &input[cursor..placeholder.start()];
        changes += span.matches(literal).count();
        output.push_str(&span.replace(literal, replacement));
        output.push_str(placeholder.as_str());
        cursor = placeholder.end();
    }
    let span = &input[cursor..];
    changes += span.matches(literal).count();
    output.push_str(&span.replace(literal, replacement));
    if changes == 0 {
        return (input.to_string(), 0);
    }
    (output, changes)
}

fn is_redaction_placeholder(value: &str) -> bool {
    REDACTION_PLACEHOLDER
        .find(value)
        .is_some_and(|matched| matched.start() == 0 && matched.end() == value.len())
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn sensitive_kind<'a>(key: &str, custom: &BTreeSet<String>) -> Option<&'a str> {
    let key = normalize_key(key);
    if custom.contains(&key) {
        return Some("configured");
    }
    if matches!(
        key.as_str(),
        "password" | "passwd" | "pwd" | "passcode" | "pin" | "secret" | "clientsecret"
    ) || key.ends_with("password")
        || key.ends_with("secret")
    {
        return Some("secret");
    }
    if matches!(
        key.as_str(),
        "authorization" | "proxyauthorization" | "auth"
    ) {
        return Some("token");
    }
    if key.contains("token") || matches!(key.as_str(), "apikey" | "apiaccesskey") {
        return Some("token");
    }
    if key.contains("cookie") {
        return Some("cookie");
    }
    if key == "email" || key.ends_with("email") {
        return Some("email");
    }
    if matches!(
        key.as_str(),
        "username" | "userlogin" | "phone" | "phonenumber"
    ) || key.ends_with("username")
    {
        return Some("pii");
    }
    if matches!(
        key.as_str(),
        "session" | "sessionid" | "device" | "deviceid" | "androidid" | "transactionid" | "serial"
    ) || key.ends_with("sessionid")
        || key.ends_with("deviceid")
        || key.ends_with("transactionid")
    {
        return Some("id");
    }
    if matches!(key.as_str(), "ip" | "ipaddress" | "clientip" | "remoteip") {
        return Some("ip");
    }
    None
}

fn placeholder(kind: &str, value: Option<&str>) -> &'static str {
    if matches!(kind, "token") && value.is_some_and(|value| JWT.is_match(value)) {
        return "<redacted:jwt>";
    }
    match kind {
        "secret" => "<redacted:secret>",
        "token" => "<redacted:token>",
        "cookie" => "<redacted:cookie>",
        "email" => "<redacted:email>",
        "pii" => "<redacted:pii>",
        "id" => "<redacted:id>",
        "ip" => "<redacted:ip>",
        _ => "<redacted:configured>",
    }
}

fn is_placeholder(value: &Value) -> bool {
    value.as_str().is_some_and(is_redaction_placeholder)
}

fn is_body_key(key: &str) -> bool {
    normalize_key(key).contains("body")
}

fn dedupe(values: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

static ACTIVE: OnceLock<RwLock<Option<Policy>>> = OnceLock::new();

fn active_slot() -> &'static RwLock<Option<Policy>> {
    ACTIVE.get_or_init(|| RwLock::new(None))
}

pub fn configure(enabled: bool, spec: PolicySpec) -> Result<()> {
    let policy = enabled.then(|| Policy::new(spec)).transpose()?;
    if let Ok(mut active) = active_slot().write() {
        *active = policy;
    }
    Ok(())
}

pub fn activate_builtin() {
    if let Ok(mut active) = active_slot().write()
        && active.is_none()
    {
        *active = Some(Policy::builtin());
    }
}

pub fn active_policy() -> Option<Policy> {
    active_slot().read().ok().and_then(|policy| policy.clone())
}

pub fn is_enabled() -> bool {
    active_slot().read().is_ok_and(|policy| policy.is_some())
}

pub fn active_spec_or_builtin() -> PolicySpec {
    active_policy()
        .map(|policy| policy.spec().clone())
        .unwrap_or_default()
}

pub fn redact_output_if_active(value: Value) -> Value {
    active_policy()
        .map(|policy| policy.redact_output(value.clone()))
        .unwrap_or(value)
}

pub fn redact_text_if_active(text: &str) -> Cow<'_, str> {
    let Some(policy) = active_policy() else {
        return Cow::Borrowed(text);
    };
    let redacted = policy.redact_text(text);
    if redacted == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(redacted)
    }
}

pub fn redact_png_if_active(
    bytes: &[u8],
    screen: &crate::proto::ScreenResponse,
) -> Result<(Vec<u8>, PixelRedactionReport)> {
    let policy = active_policy().ok_or_else(|| {
        crate::diagnostic::DiagnosticError::new(
            "screenshot_redaction_not_enabled",
            "input",
            "pixel redaction requires the global --redact flag or redaction.enabled=true",
        )
        .next_actions(["rerun with --redact and the command's explicit pixel-redaction flag"])
    })?;
    redact_png(&policy, bytes, screen)
}

fn redact_png(
    policy: &Policy,
    bytes: &[u8],
    screen: &crate::proto::ScreenResponse,
) -> Result<(Vec<u8>, PixelRedactionReport)> {
    let mut image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|error| anyhow::anyhow!("decode screenshot PNG for redaction: {error}"))?
        .to_rgba8();
    let scale_x = f64::from(image.width()) / f64::from(screen.viewport.w.max(1));
    let scale_y = f64::from(image.height()) / f64::from(screen.viewport.h.max(1));
    let mut regions = 0usize;
    for element in &screen.elements {
        let sensitive = element.password
            || element
                .text
                .as_deref()
                .is_some_and(|text| policy.text_is_sensitive(text))
            || element
                .desc
                .as_deref()
                .is_some_and(|text| policy.text_is_sensitive(text));
        let Some([left, top, right, bottom]) = element.bounds else {
            continue;
        };
        if !sensitive || right <= left || bottom <= top {
            continue;
        }
        let padding = 4.0;
        let left = ((f64::from(left) - padding) * scale_x).floor().max(0.0) as u32;
        let top = ((f64::from(top) - padding) * scale_y).floor().max(0.0) as u32;
        let right = ((f64::from(right) + padding) * scale_x)
            .ceil()
            .min(f64::from(image.width())) as u32;
        let bottom = ((f64::from(bottom) + padding) * scale_y)
            .ceil()
            .min(f64::from(image.height())) as u32;
        for y in top..bottom {
            for x in left..right {
                *image.get_pixel_mut(x, y) = image::Rgba([0, 0, 0, 255]);
            }
        }
        regions += 1;
    }
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|error| anyhow::anyhow!("encode redacted screenshot PNG: {error}"))?;
    Ok((
        output.into_inner(),
        PixelRedactionReport {
            method: "accessibility_bounds",
            regions_redacted: regions,
            // Accessibility cannot prove that every rendered glyph is exposed.
            potentially_sensitive: true,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn property_config() -> ProptestConfig {
        ProptestConfig {
            cases: 160,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x5344_5245_4441_4354),
            ..ProptestConfig::default()
        }
    }

    fn realistic_text() -> BoxedStrategy<String> {
        prop_oneof![
            1 => prop::collection::vec(any::<char>(), 0..256)
                .prop_map(|chars| chars.into_iter().collect()),
            3 => "[A-Za-z0-9]{8,32}".prop_map(|secret| format!("token={secret}&safe=visible")),
            3 => "[A-Za-z0-9._~-]{8,32}"
                .prop_map(|secret| format!("Authorization: Bearer {secret}")),
            2 => "[A-Za-z0-9]{8,32}".prop_map(|secret| {
                json!({
                    "message": format!("token={secret}"),
                    "nested": {"authorization": secret},
                })
                .to_string()
            }),
            2 => Just(
                "Authorization: <redacted:token>\n\
                 token=<redacted:secret>&encoded=%3Credacted%3Atoken%3E"
                    .to_string(),
            ),
        ]
        .boxed()
    }

    proptest! {
        #![proptest_config(property_config())]

        #[test]
        fn text_redaction_is_idempotent(input in realistic_text()) {
            let policy = Policy::builtin();
            let once = policy.redact_text(&input);
            prop_assert_eq!(policy.redact_text(&once), once);
        }

        #[test]
        fn structured_output_redaction_is_idempotent(
            secret in "[A-Za-z0-9]{8,32}",
            safe_tail in "[A-Za-z0-9 ._/-]{0,48}",
            order in 0u32..100_000,
            message_kind in 0u8..6,
        ) {
            let policy = Policy::new(PolicySpec {
                json_keys: vec!["customerCode".into()],
                patterns: vec![r"ORDER-[0-9]+".into(), "redacted".into()],
            })
            .unwrap();
            let message = match message_kind {
                0 => format!("token={secret}&safe={safe_tail}"),
                1 => format!("Authorization: Bearer {secret}"),
                2 => format!(
                    "already redacted outside <redacted:token> ORDER-{order} {safe_tail}"
                ),
                3 => format!(
                    "token=<redacted:secret>&encoded=%3Credacted%3Atoken%3E ORDER-{order}"
                ),
                4 => json!({"authorization": secret, "safe": safe_tail}).to_string(),
                _ => format!("ORDER-{order} {safe_tail}"),
            };
            let input = json!({
                "access_token": secret.clone(),
                "message": message,
                "echo": secret,
                "customerCode": format!("customer-{order}"),
                "nested": {"email": "person@example.invalid"},
            });
            let once = policy.redact_output(input);
            let twice = policy.redact_output(once.clone());
            prop_assert_eq!(twice, once);
        }

        #[test]
        fn generated_query_redaction_matches_the_exact_independent_target(
            secret_a in "[A-Za-z0-9]{8,32}",
            secret_b in "[A-Za-z0-9]{8,32}",
            safe_values in prop::collection::vec("[a-z0-9_-]{0,16}", 1..8),
            parameter_kinds in prop::collection::vec(0u8..8, 1..24),
        ) {
            let policy = Policy::builtin();
            let mut input_parameters = Vec::with_capacity(parameter_kinds.len());
            let mut expected_parameters = Vec::with_capacity(parameter_kinds.len());
            for (index, kind) in parameter_kinds.into_iter().enumerate() {
                let safe = &safe_values[index % safe_values.len()];
                let (input, expected) = match kind {
                    0 => (
                        format!("access%5Ftoken={secret_a}"),
                        "access%5Ftoken=%3Credacted%3Atoken%3E".to_string(),
                    ),
                    1 => (
                        format!("token={secret_b}"),
                        "token=%3Credacted%3Atoken%3E".to_string(),
                    ),
                    2 => (
                        "email=person%40example.invalid".to_string(),
                        "email=%3Credacted%3Aemail%3E".to_string(),
                    ),
                    3 => (
                        format!("password={secret_a}"),
                        "password=%3Credacted%3Asecret%3E".to_string(),
                    ),
                    // Separate arms deliberately produce duplicate `safe`
                    // names. Their order and values must remain byte-for-byte
                    // stable rather than being normalized as a map.
                    4 | 5 => (format!("safe={safe}"), format!("safe={safe}")),
                    6 => (format!("page={safe}"), format!("page={safe}")),
                    _ => ("token=".to_string(), "token=".to_string()),
                };
                input_parameters.push(input);
                expected_parameters.push(expected);
            }
            let input = format!("/v1/items?{}#section", input_parameters.join("&"));
            let expected = format!("/v1/items?{}#section", expected_parameters.join("&"));
            let (once, changed) = policy.redact_request_target(&input);
            let (twice, changed_again) = policy.redact_request_target(&once);
            prop_assert_eq!(&once, &expected);
            prop_assert_eq!(changed, input != expected);
            prop_assert_eq!(twice, once);
            prop_assert!(!changed_again);
        }
    }

    #[test]
    fn inline_and_configured_placeholders_preserve_counts_on_repeated_output_redaction() {
        let policy = Policy::new(PolicySpec {
            json_keys: vec![],
            patterns: vec!["redacted".into(), r"ORDER-[0-9]+".into()],
        })
        .unwrap();
        let first = policy.redact_output(json!({
            "message": "token=first\nAuthorization: Bearer second\nexisting token=<redacted:secret> <redacted:pattern> %3Credacted%3Atoken%3E ORDER-42",
        }));
        let first_count = first["redaction"]["redacted_values"].clone();
        let second = policy.redact_output(first.clone());

        assert_eq!(second, first);
        assert_eq!(second["redaction"]["redacted_values"], first_count);
    }

    #[test]
    fn sensitive_literals_never_rewrite_the_placeholder_that_replaced_them() {
        let policy = Policy::builtin();
        let first = policy.redact_output(json!({
            "access_token": "redacted",
            "message": "echo redacted",
        }));

        assert_eq!(first["access_token"], "<redacted:token>");
        assert_eq!(first["message"], "echo <redacted:token>");
        assert_eq!(first["redaction"]["redacted_values"], 2);
        assert_eq!(policy.redact_output(first.clone()), first);
    }

    #[test]
    fn minimized_known_literal_idempotence_regression_is_pinned_by_value() {
        // Former proptest seed
        // 101f699bd05fc081b05c17874a6ee0ae59a9ae5574183b30777eff0670c25c48
        // shrank to this exact secret and empty message. Pin the minimized value
        // directly so future strategy changes cannot silently change the case.
        let policy = Policy::builtin();
        let first = policy.redact_output(json!({
            "access_token": "AA0aAaaA",
            "message": "",
            "echo": "AA0aAaaA",
        }));

        assert_eq!(first["access_token"], "<redacted:token>");
        assert_eq!(first["message"], "");
        assert_eq!(first["echo"], "<redacted:token>");
        assert_eq!(first["redaction"]["redacted_values"], 2);
        assert_eq!(policy.redact_output(first.clone()), first);
    }

    #[test]
    fn flattened_plugin_redaction_metadata_cannot_bypass_policy() {
        let policy = Policy::builtin();
        let first = policy.redact_output(json!({
            "type": "logpoint",
            "redaction": {
                "screenshot_pixels_requested": true,
                "video_pixels": false,
                "metadata": "marker_labels_only",
                "note": "Authorization: Bearer plugin-secret",
                "access_token": "metadata-secret",
                "echo": "metadata-secret",
            },
        }));

        let metadata = &first["redaction"];
        assert_eq!(metadata["screenshot_pixels_requested"], true);
        assert_eq!(metadata["video_pixels"], false);
        assert_eq!(metadata["metadata"], "marker_labels_only");
        assert_eq!(metadata["note"], "Authorization: <redacted:token>");
        assert_eq!(metadata["access_token"], "<redacted:token>");
        assert_eq!(metadata["echo"], "<redacted:token>");
        assert_eq!(metadata["redacted_values"], 3);
        assert_eq!(policy.redact_output(first.clone()), first);
    }

    #[test]
    fn custom_redaction_patterns_must_consume_text() {
        for pattern in [r"\b", r"[a&&b]|"] {
            let error = Policy::new(PolicySpec {
                json_keys: vec![],
                patterns: vec![pattern.into()],
            })
            .unwrap_err();
            let diagnostic = error
                .downcast_ref::<crate::diagnostic::DiagnosticError>()
                .unwrap();
            assert_eq!(diagnostic.code, "invalid_redaction_pattern");
        }
    }

    #[test]
    fn json_fragments_only_treat_exact_placeholders_as_opaque() {
        let policy = Policy::builtin();
        for input in [
            r#"prefix "password":"<redacted:token>suffix-secret" tail"#,
            r#"prefix \"password\":\"<redacted:token>suffix-secret\" tail"#,
        ] {
            let output = policy.redact_text(input);
            assert!(
                !output.contains("suffix-secret"),
                "fragment leaked: {output}"
            );
            assert!(output.contains("<redacted:secret>"));
        }

        for input in [
            r#"prefix "password":"%3Credacted%3Atoken%3E" tail"#,
            r#"prefix \"password\":\"%3Credacted%3Atoken%3E\" tail"#,
        ] {
            assert_eq!(policy.redact_text(input), input);
        }
    }

    #[test]
    fn generated_redaction_metadata_is_outside_custom_policy_matching() {
        let policy = Policy::new(PolicySpec {
            json_keys: vec!["policy".into()],
            patterns: vec!["builtin".into()],
        })
        .unwrap();
        let first = policy.redact_output(json!({
            "policy": "outside-policy-value",
            "message": "builtin outside metadata",
            "redaction": {"screenshot_pixels_requested": true},
        }));

        assert_eq!(first["policy"], "<redacted:configured>");
        assert_eq!(first["message"], "<redacted:pattern> outside metadata");
        assert_eq!(first["redaction"]["policy"], "builtin+config");
        assert_eq!(first["redaction"]["screenshot_pixels_requested"], true);
        assert_eq!(first["redaction"]["redacted_values"], 2);
        assert_eq!(policy.redact_output(first.clone()), first);
    }

    #[test]
    fn nested_json_and_graphql_body_keep_shape_and_typed_placeholders() {
        let policy = Policy::builtin();
        let input = json!({
            "operationName": "Login",
            "variables": {
                "email": "person@example.com",
                "password": "correct horse",
                "profile": {"accessToken": "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.signature"}
            },
            "req_body": "{\"variables\":{\"refreshToken\":\"secret-token\",\"safe\":7}}"
        });
        let output = policy.redact_output(input);
        assert_eq!(output["variables"]["email"], "<redacted:email>");
        assert_eq!(output["variables"]["password"], "<redacted:secret>");
        assert_eq!(
            output["variables"]["profile"]["accessToken"],
            "<redacted:jwt>"
        );
        assert!(
            output["req_body"]
                .as_str()
                .unwrap()
                .contains("<redacted:token>")
        );
        assert_eq!(output["req_body_redacted"], true);
        assert_eq!(output["body_redacted"], true);
        assert_eq!(output["redaction"]["version"], POLICY_VERSION);
    }

    #[test]
    fn escaped_log_text_redacts_email_ip_bearer_and_nested_json() {
        let policy = Policy::builtin();
        let log = r#"request from 192.168.1.4 person@example.com Authorization: Bearer abc.def.ghi
{\"password\":\"do-not-print\",\"ok\":true}"#;
        let output = policy.redact_text(log);
        assert!(!output.contains("192.168.1.4"));
        assert!(!output.contains("person@example.com"));
        assert!(!output.contains("Bearer abc.def.ghi"));
        assert!(!output.contains("do-not-print"));
        assert!(output.contains("<redacted:ip>"));
    }

    #[test]
    fn request_target_redaction_decodes_names_values_and_preserves_safe_pairs() {
        let policy = Policy::builtin();
        let input = "/users/person%40example.com?access%5Ftoken=secret-one&safe=visible&email=person%40example.com&safe=second&token=secret-two&token=";
        let (output, changed) = policy.redact_request_target(input);

        assert!(changed);
        for secret in [
            "person@example.com",
            "person%40example.com",
            "secret-one",
            "secret-two",
        ] {
            assert!(
                !output.contains(secret),
                "request target leaked {secret}: {output}"
            );
        }
        assert!(output.contains("safe=visible"));
        assert!(output.contains("safe=visible&email="));
        assert!(output.contains("safe=second"));
        assert!(output.ends_with("&token="));
        assert!(output.contains("access%5Ftoken=%3Credacted%3Atoken%3E"));
        assert!(output.contains("email=%3Credacted%3Aemail%3E"));

        let (second, changed_again) = policy.redact_request_target(&output);
        assert_eq!(second, output);
        assert!(!changed_again, "redaction must be idempotent");
    }

    #[test]
    fn flow_metadata_is_redacted_before_persistence_without_changing_lengths() {
        let policy = Policy::new(PolicySpec {
            json_keys: vec!["customerCode".into()],
            patterns: vec!["ORDER-[0-9]+".into()],
        })
        .unwrap();
        let mut flow = crate::net::flow::FlowRecord {
            host: "10.2.3.4".into(),
            path: "/ORDER-42?customerCode=customer-secret&safe=visible".into(),
            req_len: 91,
            resp_len: 123,
            error: Some(
                "upstream https://10.2.3.4/v1?customer%43ode=url-secret&access%5Ftoken=encoded-secret rejected person@example.com for ORDER-42"
                    .into(),
            ),
            ..Default::default()
        };

        policy.redact_flow_record(&mut flow);

        let persisted = serde_json::to_string(&flow).unwrap();
        for secret in [
            "10.2.3.4",
            "customer-secret",
            "url-secret",
            "encoded-secret",
            "person@example.com",
            "ORDER-42",
        ] {
            assert!(!persisted.contains(secret), "flow metadata leaked {secret}");
        }
        assert!(flow.path.contains("safe=visible"));
        assert!(flow.host_redacted);
        assert!(flow.path_redacted);
        assert!(flow.error_redacted);
        assert_eq!(flow.req_len, 91);
        assert_eq!(flow.resp_len, 123);
        assert_eq!(flow.redaction_policy_version, Some(POLICY_VERSION));
    }

    #[test]
    fn complete_json_with_bearer_text_remains_valid_json() {
        let policy = Policy::builtin();
        let input = json!({
            "log_expression": "\"Authorization: Bearer e2e.token.value \" + payload",
            "message": "Authorization: Bearer e2e.token.value payload",
            "line": 119,
        })
        .to_string();

        let output = policy.redact_text(&input);
        let parsed: Value = serde_json::from_str(&output).expect("redacted JSON must stay valid");

        assert_eq!(parsed["line"], 119);
        assert!(!output.contains("e2e.token.value"));
        assert!(output.contains("<redacted:token>"));
    }

    #[test]
    fn custom_keys_and_patterns_are_deterministic() {
        let policy = Policy::new(PolicySpec {
            json_keys: vec!["customerCode".into(), "customerCode".into()],
            patterns: vec![r"ORDER-[0-9]+".into(), r"ORDER-[0-9]+".into()],
        })
        .unwrap();
        let input = json!({"customerCode":"abc", "message":"ORDER-123 ORDER-456"});
        let first = policy.redact_output(input.clone());
        let second = policy.redact_output(input);
        assert_eq!(first, second);
        assert_eq!(first["customerCode"], "<redacted:configured>");
        assert_eq!(first["message"], "<redacted:pattern> <redacted:pattern>");
        assert_eq!(first["redaction"]["custom_json_keys"], 1);
        assert_eq!(first["redaction"]["custom_patterns"], 1);
    }

    #[test]
    fn sensitive_identifiers_are_removed_from_recovery_commands_too() {
        let policy = Policy::builtin();
        let output = policy.redact_output(json!({
            "serial": "emulator-5554",
            "next_actions": ["shadowdroid -d emulator-5554 connect"]
        }));
        assert_eq!(output["serial"], "<redacted:id>");
        assert_eq!(
            output["next_actions"][0],
            "shadowdroid -d <redacted:id> connect"
        );
    }

    #[test]
    fn existing_privacy_metadata_is_extended_not_discarded() {
        let output = Policy::builtin().redact_output(json!({
            "redaction": {"screenshot_pixels_requested": true},
            "email": "person@example.com"
        }));
        assert_eq!(output["redaction"]["screenshot_pixels_requested"], true);
        assert_eq!(output["redaction"]["enabled"], true);
        assert_eq!(output["redaction"]["redacted_values"], 1);
    }

    #[test]
    fn invalid_custom_pattern_is_typed_without_echoing_the_pattern() {
        let error = Policy::new(PolicySpec {
            json_keys: vec![],
            patterns: vec!["SECRET([".into()],
        })
        .unwrap_err();
        let diagnostic = error
            .downcast_ref::<crate::diagnostic::DiagnosticError>()
            .unwrap();
        assert_eq!(diagnostic.code, "invalid_redaction_pattern");
        assert!(!diagnostic.to_string().contains("SECRET"));
    }

    #[test]
    fn ipv6_detection_does_not_redact_timestamps() {
        let policy = Policy::builtin();
        let output = policy.redact_text("at 12:34:56 from 2001:db8::1 or ::1");
        assert!(output.contains("12:34:56"));
        assert!(!output.contains("2001:db8::1"));
        assert!(!output.contains("::1"));
    }

    #[test]
    fn screenshot_redaction_blacks_out_sensitive_accessibility_bounds() {
        let policy = Policy::builtin();
        let source = image::RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let screen = crate::proto::ScreenResponse {
            screen_hash: "hash".into(),
            screen_hash_version: 3,
            content_hash: None,
            interaction_hash: None,
            interaction_hash_version: 1,
            snapshot_state: crate::proto::SnapshotState::Consistent,
            captured_at_ms: None,
            viewport: crate::proto::Viewport { w: 20, h: 20 },
            current_app: crate::proto::AppRef {
                package: None,
                activity: None,
                pid: None,
                sampled_at_ms: None,
            },
            ui_tree: None,
            warning: None,
            element_count: 1,
            ime: crate::proto::ImeState::default(),
            elements: vec![crate::proto::Element {
                id: 0,
                handle: None,
                text: Some("person@example.com".into()),
                desc: None,
                klass: None,
                rid: None,
                bounds: Some([5, 5, 15, 15]),
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
            }],
        };
        let (redacted, report) = redact_png(&policy, bytes.get_ref(), &screen).unwrap();
        let image = image::load_from_memory(&redacted).unwrap().to_rgba8();
        assert_eq!(image.get_pixel(10, 10), &image::Rgba([0, 0, 0, 255]));
        assert_eq!(image.get_pixel(0, 0), &image::Rgba([255, 255, 255, 255]));
        assert_eq!(report.regions_redacted, 1);
        assert!(report.potentially_sensitive);
    }
}
