//! Typed, deterministic HTTP replay bundles.
//!
//! A bundle is committed by `manifest.json`: response bodies are written under
//! content-addressed names first, then the complete manifest is atomically
//! replaced. Loading follows the inverse rule — parse and validate the entire
//! manifest and every referenced body before constructing an immutable
//! [`ReplaySet`]. Callers can therefore swap one set into the daemon with a
//! single lock assignment without exposing a partially loaded configuration.

use crate::net::flow::{self, FlowRecord};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

pub const REPLAY_ARTIFACT_TYPE: &str = "shadowdroid_replay_bundle";
pub const REPLAY_FORMAT_VERSION: u32 = 1;
pub const REPLAY_PAYLOAD_TYPE: &str = "shadowdroid_replay_set";

const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LEGACY_JSONL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_RESPONSE_BODY_BYTES: u64 = flow::BODY_CAP as u64;
const MAX_TOTAL_RESPONSE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_REPLAY_ENTRIES: usize = 10_000;

/// One captured flow plus the effective request port observed on the wire.
///
/// `FlowRecord` did not historically retain non-default ports. Keeping the
/// port explicit here prevents the bundle builder from silently guessing. The
/// integration layer should pass the newly captured port, and only its
/// deliberate legacy compatibility path should use [`default_port_for_scheme`].
#[derive(Debug, Clone, Copy)]
pub struct ReplaySource<'a> {
    pub flow: &'a FlowRecord,
    pub effective_port: u16,
}

impl<'a> ReplaySource<'a> {
    pub fn new(flow: &'a FlowRecord, effective_port: u16) -> Self {
        Self {
            flow,
            effective_port,
        }
    }

    /// Exact current-schema adapter. New proxy and AAR captures must retain the
    /// observed port instead of letting the replay layer guess.
    pub fn from_flow(flow: &'a FlowRecord) -> Result<Self> {
        let effective_port = flow
            .port
            .context("flow has no effective port; use the explicit legacy adapter if intended")?;
        ensure!(effective_port != 0, "flow effective port must be non-zero");
        Ok(Self::new(flow, effective_port))
    }

    /// Deliberate compatibility adapter for pre-port `FlowRecord` values.
    pub fn from_flow_or_default_port(flow: &'a FlowRecord) -> Result<Self> {
        if flow.port.is_some() {
            return Self::from_flow(flow);
        }
        Ok(Self::new(flow, default_port_for_scheme(&flow.scheme)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplayManifest {
    pub artifact_type: String,
    pub format_version: u32,
    /// Compatibility alias retained for existing neutral-fixture consumers.
    pub version: u32,
    pub generated_by: String,
    pub count: usize,
    pub fixtures: Vec<ReplayManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplayManifestEntry {
    // Flat compatibility fields used by the original fixtures export.
    pub method: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Complete request target (`path` plus an optional raw query).
    pub path: String,
    pub operation_name: Option<String>,
    pub status: u16,
    pub request_content_type: Option<String>,
    pub response_content_type: Option<String>,
    pub response_file: Option<String>,
    pub response_truncated: bool,

    /// Canonical exact-match key used by ShadowDroid replay.
    #[serde(rename = "match")]
    pub request_match: ReplayKey,
    pub response_headers: Vec<(String, String)>,
    /// Representation length advertised by a captured HEAD 2xx/3xx response.
    /// Framing headers themselves are never persisted.
    pub representation_content_length: Option<u64>,
    pub response_bytes: u64,
    pub response_sha256: String,
}

/// Canonical exact request identity. Ordering is also the deterministic bundle
/// order and the in-memory map order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct ReplayKey {
    pub method: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub query: Option<String>,
    pub graphql_operation_name: Option<String>,
    pub request_body_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplayResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Representation length for HEAD responses, whose wire body is empty.
    pub representation_content_length: Option<u64>,
    /// Captured responses are replayable only when their exact UTF-8 body was
    /// retained. Binary/streamed/incomplete responses are rejected at export.
    pub body: String,
    pub body_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplayRuntimeEntry {
    pub key: ReplayKey,
    pub response: ReplayResponse,
}

/// Versioned wire value sent in one daemon control operation. The daemon must
/// reconstruct it with [`ReplaySet::from_payload`] before replacing live state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplayLoadPayload {
    pub artifact_type: String,
    pub format_version: u32,
    /// SHA-256 of the canonical, sorted runtime entries below. The daemon
    /// recomputes it before publishing the set.
    pub active_set_sha256: String,
    pub entries: Vec<ReplayRuntimeEntry>,
}

/// Immutable, fully validated replay configuration.
#[derive(Debug, Clone)]
pub struct ReplaySet {
    entries: BTreeMap<ReplayKey, ReplayResponse>,
    source_bundle_sha256: Option<String>,
    active_set_sha256: String,
    source_count: usize,
}

/// Typed lookup result. `Ambiguous` is unreachable for a validated
/// [`ReplaySet`], but remains explicit at the proxy boundary so a future
/// matcher extension can never regress to first-match-wins behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayLookup {
    Hit(ReplayResponse),
    Miss,
    Ambiguous { matches: usize },
}

impl ReplaySet {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn source_count(&self) -> usize {
        self.source_count
    }

    /// Artifact manifest identity, available on host-loaded sets. A daemon set
    /// reconstructed from the wire intentionally does not trust this metadata.
    pub fn source_bundle_sha256(&self) -> Option<&str> {
        self.source_bundle_sha256.as_deref()
    }

    /// Identity of this exact active entry subset. Count and fingerprint are
    /// read from the same immutable `ReplaySet` snapshot.
    pub fn active_set_sha256(&self) -> &str {
        &self.active_set_sha256
    }

    pub fn lookup(&self, request: &ReplayRequest<'_>) -> Result<ReplayLookup> {
        let key = request_key(request)?;
        let mut matches = self.entries.range(key.clone()..=key);
        Ok(match (matches.next(), matches.next()) {
            (None, _) => ReplayLookup::Miss,
            (Some((_, response)), None) => ReplayLookup::Hit(response.clone()),
            (Some(_), Some(_)) => ReplayLookup::Ambiguous {
                matches: 2 + matches.count(),
            },
        })
    }

    pub fn to_payload(&self) -> ReplayLoadPayload {
        ReplayLoadPayload {
            artifact_type: REPLAY_PAYLOAD_TYPE.into(),
            format_version: REPLAY_FORMAT_VERSION,
            active_set_sha256: self.active_set_sha256.clone(),
            entries: self
                .entries
                .iter()
                .map(|(key, response)| ReplayRuntimeEntry {
                    key: key.clone(),
                    response: response.clone(),
                })
                .collect(),
        }
    }

    /// Strict daemon-side reconstruction. No caller should publish the payload
    /// to live state until this function succeeds.
    pub fn from_payload(payload: ReplayLoadPayload) -> Result<Self> {
        ensure!(
            payload.artifact_type == REPLAY_PAYLOAD_TYPE,
            "unsupported replay payload type {:?}; expected {:?}",
            payload.artifact_type,
            REPLAY_PAYLOAD_TYPE
        );
        ensure!(
            payload.format_version == REPLAY_FORMAT_VERSION,
            "unsupported replay payload version {}; expected {}",
            payload.format_version,
            REPLAY_FORMAT_VERSION
        );
        validate_sha256(&payload.active_set_sha256, "active_set_sha256")?;
        ensure!(!payload.entries.is_empty(), "replay payload is empty");
        ensure!(
            payload.entries.len() <= MAX_REPLAY_ENTRIES,
            "replay payload has {} entries; maximum is {}",
            payload.entries.len(),
            MAX_REPLAY_ENTRIES
        );

        let mut total_bytes = 0_u64;
        let mut entries = BTreeMap::new();
        for (index, entry) in payload.entries.into_iter().enumerate() {
            validate_key(&entry.key)
                .with_context(|| format!("validate replay payload entry {index} key"))?;
            validate_runtime_response(&entry.key, &entry.response)
                .with_context(|| format!("validate replay payload entry {index} response"))?;
            total_bytes = total_bytes
                .checked_add(entry.response.body.len() as u64)
                .context("replay payload response byte count overflow")?;
            ensure!(
                total_bytes <= MAX_TOTAL_RESPONSE_BYTES,
                "replay payload bodies exceed {} bytes",
                MAX_TOTAL_RESPONSE_BYTES
            );
            if entries.insert(entry.key.clone(), entry.response).is_some() {
                bail!("ambiguous duplicate replay key at payload entry {index}");
            }
        }

        let computed_sha256 = active_entries_sha256(&entries)?;
        ensure!(
            computed_sha256 == payload.active_set_sha256,
            "replay payload active_set_sha256 mismatch: expected {}, got {}",
            payload.active_set_sha256,
            computed_sha256
        );

        Ok(Self {
            source_count: entries.len(),
            entries,
            source_bundle_sha256: None,
            active_set_sha256: computed_sha256,
        })
    }

    /// Strict JSON control-plane boundary: unknown/missing fields fail before
    /// [`from_payload`](Self::from_payload) validates semantics and constructs
    /// the immutable set.
    pub fn from_wire_value(value: serde_json::Value) -> Result<Self> {
        let payload: ReplayLoadPayload =
            serde_json::from_value(value).context("decode replay load payload")?;
        Self::from_payload(payload)
    }
}

/// Live request fields needed for exact replay lookup.
#[derive(Debug, Clone, Copy)]
pub struct ReplayRequest<'a> {
    pub method: &'a str,
    pub scheme: &'a str,
    pub host: &'a str,
    pub effective_port: u16,
    pub path_and_query: &'a str,
    pub body: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct BuiltBundle {
    pub manifest: ReplayManifest,
    /// Relative POSIX path to exact UTF-8 response body.
    pub response_files: BTreeMap<String, String>,
}

impl BuiltBundle {
    pub fn manifest_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec_pretty(&self.manifest)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() as u64 <= MAX_MANIFEST_BYTES,
            "replay manifest is {} bytes; maximum is {}",
            bytes.len(),
            MAX_MANIFEST_BYTES
        );
        Ok(bytes)
    }

    pub fn into_replay_set(self, host_filter: Option<&str>) -> Result<ReplaySet> {
        let manifest_bytes = self.manifest_bytes()?;
        let BuiltBundle {
            manifest,
            response_files,
        } = self;
        replay_set_from_manifest(manifest, &manifest_bytes, host_filter, |entry| match &entry
            .response_file
        {
            Some(path) => response_files
                .get(path)
                .cloned()
                .with_context(|| format!("missing built response {path}")),
            None => Ok(String::new()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteSummary {
    pub out: PathBuf,
    pub manifest: PathBuf,
    pub count: usize,
    pub response_files: usize,
    pub source_bundle_sha256: String,
    pub active_set_sha256: String,
}

pub fn default_port_for_scheme(scheme: &str) -> Result<u16> {
    match scheme.trim().to_ascii_lowercase().as_str() {
        "http" => Ok(80),
        "https" => Ok(443),
        other => bail!("unsupported replay scheme {other:?}; expected http or https"),
    }
}

/// Build a deterministic bundle in memory. Input order, flow ids, timestamps,
/// and capture sequence numbers do not influence the result.
pub fn build_bundle(sources: &[ReplaySource<'_>]) -> Result<BuiltBundle> {
    ensure!(!sources.is_empty(), "cannot build an empty replay bundle");
    ensure!(
        sources.len() <= MAX_REPLAY_ENTRIES,
        "replay bundle has {} flows; maximum is {}",
        sources.len(),
        MAX_REPLAY_ENTRIES
    );

    let mut entries = BTreeMap::<ReplayKey, (ReplayManifestEntry, String)>::new();
    let mut total_bytes = 0_u64;

    for source in sources {
        let flow = source.flow;
        if let Some(captured_port) = flow.port {
            ensure!(
                captured_port == source.effective_port,
                "flow {:?} captured port {} but replay source supplied {}",
                flow.id,
                captured_port,
                source.effective_port
            );
        }
        validate_source_flow(flow)
            .with_context(|| format!("flow {:?} is not replayable", flow.id))?;

        let key = key_from_flow(flow, source.effective_port)
            .with_context(|| format!("build replay key for flow {:?}", flow.id))?;
        let status = flow.status.context("validated flow lost response status")?;
        let body = exact_response_body(flow)?;
        let body_bytes = body.len() as u64;
        let representation_content_length = captured_representation_content_length(flow, status)?;
        let mut response_headers = sanitize_response_headers(&flow.resp_headers)?;
        if !response_headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            && let Some(content_type) = &flow.resp_type
        {
            response_headers.push(("content-type".into(), content_type.clone()));
        }
        validate_response_headers(&response_headers)?;

        let response_sha256 = sha256_bytes(body.as_bytes());
        let response_file = (!body.is_empty()).then(|| response_path(&response_sha256));
        let target = join_target(&key.path, key.query.as_deref());
        let manifest_entry = ReplayManifestEntry {
            method: key.method.clone(),
            scheme: key.scheme.clone(),
            host: key.host.clone(),
            port: key.port,
            path: target,
            operation_name: key.graphql_operation_name.clone(),
            status,
            request_content_type: flow.req_type.clone(),
            response_content_type: flow.resp_type.clone(),
            response_file,
            response_truncated: false,
            request_match: key.clone(),
            response_headers: response_headers.clone(),
            representation_content_length,
            response_bytes: body_bytes,
            response_sha256: response_sha256.clone(),
        };
        let response = ReplayResponse {
            status,
            headers: response_headers,
            representation_content_length,
            body: body.clone(),
            body_sha256: response_sha256,
        };
        validate_runtime_response(&key, &response)?;

        if let Some((existing_entry, existing_body)) = entries.get_mut(&key) {
            let same_response = existing_entry.status == manifest_entry.status
                && existing_entry.response_headers == manifest_entry.response_headers
                && existing_entry.representation_content_length
                    == manifest_entry.representation_content_length
                && existing_entry.response_sha256 == manifest_entry.response_sha256
                && existing_body == &body;
            if !same_response {
                bail!(
                    "conflicting responses for replay key from flow {:?}: {} {}://{}:{}{}",
                    flow.id,
                    key.method,
                    key.scheme,
                    key.host,
                    key.port,
                    join_target(&key.path, key.query.as_deref())
                );
            }
            // Compatibility-only metadata is not part of matching or replay.
            // Pick a stable representative when identical captures disagree.
            existing_entry.request_content_type = stable_optional_min(
                existing_entry.request_content_type.take(),
                manifest_entry.request_content_type,
            );
            existing_entry.response_content_type = stable_optional_min(
                existing_entry.response_content_type.take(),
                manifest_entry.response_content_type,
            );
            continue;
        }

        total_bytes = total_bytes
            .checked_add(body_bytes)
            .context("replay bundle response byte count overflow")?;
        ensure!(
            total_bytes <= MAX_TOTAL_RESPONSE_BYTES,
            "replay bundle bodies exceed {} bytes",
            MAX_TOTAL_RESPONSE_BYTES
        );
        entries.insert(key, (manifest_entry, body));
    }

    let mut fixtures = Vec::with_capacity(entries.len());
    let mut response_files = BTreeMap::new();
    for (_, (entry, body)) in entries {
        if let Some(path) = &entry.response_file {
            match response_files.insert(path.clone(), body.clone()) {
                Some(existing) if existing != body => {
                    bail!("SHA-256 collision while content-addressing response {path}")
                }
                _ => {}
            }
        }
        fixtures.push(entry);
    }

    let bundle = BuiltBundle {
        manifest: ReplayManifest {
            artifact_type: REPLAY_ARTIFACT_TYPE.into(),
            format_version: REPLAY_FORMAT_VERSION,
            version: REPLAY_FORMAT_VERSION,
            generated_by: format!(
                "shadowdroid net export fixtures ({})",
                env!("CARGO_PKG_VERSION")
            ),
            count: fixtures.len(),
            fixtures,
        },
        response_files,
    };
    // A generated artifact must be consumable by the loader that advertises
    // the same format. Reject it here rather than writing a manifest that the
    // documented export-to-replay next step will refuse.
    bundle.manifest_bytes()?;
    Ok(bundle)
}

/// Write response bodies first and atomically publish `manifest.json` last.
pub fn write_bundle(sources: &[ReplaySource<'_>], out: &Path) -> Result<WriteSummary> {
    let bundle = build_bundle(sources)?;
    ensure_directory(out, "replay output")?;
    let responses_dir = out.join("responses");
    ensure_directory(&responses_dir, "replay responses")?;

    for (relative, body) in &bundle.response_files {
        validate_response_relative_path(relative, &sha256_bytes(body.as_bytes()))?;
        let destination = out.join(relative);
        if destination.exists() {
            let existing = read_regular_file(&destination, MAX_RESPONSE_BODY_BYTES, "response")?;
            ensure!(
                existing == body.as_bytes(),
                "content-addressed response {} has unexpected contents",
                destination.display()
            );
        } else {
            write_atomic(&destination, body.as_bytes())?;
        }
    }

    let manifest_bytes = bundle.manifest_bytes()?;
    let source_bundle_sha256 = sha256_bytes(&manifest_bytes);
    let active_set_sha256 = bundle.clone().into_replay_set(None)?.active_set_sha256;
    let manifest_path = out.join("manifest.json");
    write_atomic(&manifest_path, &manifest_bytes)?;

    Ok(WriteSummary {
        out: out.to_path_buf(),
        manifest: manifest_path,
        count: bundle.manifest.count,
        response_files: bundle.response_files.len(),
        source_bundle_sha256,
        active_set_sha256,
    })
}

/// Load a bundle directory (or its exact `manifest.json`), validating the
/// complete unfiltered bundle before applying an optional exact host filter.
pub fn load_bundle(from: &Path, host_filter: Option<&str>) -> Result<ReplaySet> {
    let (root, manifest_path) = resolve_bundle_paths(from)?;
    let manifest_bytes = read_regular_file(&manifest_path, MAX_MANIFEST_BYTES, "manifest")?;
    let manifest: ReplayManifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("parse replay manifest {}", manifest_path.display()))?;

    let responses_dir = root.join("responses");
    if responses_dir.exists() {
        require_existing_directory(&responses_dir, "replay responses")?;
    }
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("canonicalize replay root {}", root.display()))?;

    replay_set_from_manifest(manifest, &manifest_bytes, host_filter, |entry| match &entry
        .response_file
    {
        None => Ok(String::new()),
        Some(relative) => {
            validate_response_relative_path(relative, &entry.response_sha256)?;
            require_existing_directory(&responses_dir, "replay responses")?;
            let path = root.join(relative);
            let canonical = path
                .canonicalize()
                .with_context(|| format!("canonicalize response {}", path.display()))?;
            ensure!(
                canonical.starts_with(&canonical_root),
                "response {} escapes replay root {}",
                path.display(),
                root.display()
            );
            let bytes = read_regular_file(&path, MAX_RESPONSE_BODY_BYTES, "response")?;
            String::from_utf8(bytes)
                .with_context(|| format!("response {} is not UTF-8", path.display()))
        }
    })
}

/// Explicit compatibility loader for the undocumented historical JSONL replay
/// input. Every non-empty line must be a complete `FlowRecord`; malformed or
/// non-flow records fail the entire load. A retained port is honored; genuinely
/// old records use a deliberate scheme-default fallback.
pub fn load_legacy_jsonl_with_default_ports(
    from: &Path,
    host_filter: Option<&str>,
) -> Result<ReplaySet> {
    let bytes = read_regular_file(from, MAX_LEGACY_JSONL_BYTES, "legacy replay JSONL")?;
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("legacy replay file {} is not UTF-8", from.display()))?;
    let mut flows = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).with_context(|| {
            format!(
                "parse legacy replay JSONL {} line {}",
                from.display(),
                line_index + 1
            )
        })?;
        ensure!(
            value.get("type").is_none(),
            "legacy replay JSONL {} line {} is a typed non-flow record",
            from.display(),
            line_index + 1
        );
        let flow: FlowRecord = serde_json::from_value(value).with_context(|| {
            format!(
                "decode legacy replay flow {} line {}",
                from.display(),
                line_index + 1
            )
        })?;
        flows.push(flow);
        ensure!(
            flows.len() <= MAX_REPLAY_ENTRIES,
            "legacy replay JSONL exceeds {} flows",
            MAX_REPLAY_ENTRIES
        );
    }
    ensure!(!flows.is_empty(), "legacy replay JSONL contains no flows");

    let sources = flows
        .iter()
        .map(ReplaySource::from_flow_or_default_port)
        .collect::<Result<Vec<_>>>()?;
    build_bundle(&sources)?.into_replay_set(host_filter)
}

/// Alias naming the behavior rather than its port fallback. Kept separate from
/// [`load_bundle`] so callers cannot accidentally reinterpret a directory
/// manifest as the legacy line format.
pub fn load_legacy_jsonl_strict(from: &Path, host_filter: Option<&str>) -> Result<ReplaySet> {
    load_legacy_jsonl_with_default_ports(from, host_filter)
}

pub fn request_key(request: &ReplayRequest<'_>) -> Result<ReplayKey> {
    let method = normalize_method(request.method)?;
    let scheme = normalize_scheme(request.scheme)?;
    let host = normalize_host(request.host)?;
    ensure!(
        request.effective_port != 0,
        "replay request port must be non-zero"
    );
    let (path, query) = split_target(request.path_and_query)?;
    let graphql_operation_name = graphql_operation_name(query.as_deref(), request.body)?;
    let key = ReplayKey {
        method,
        scheme,
        host,
        port: request.effective_port,
        path,
        query,
        graphql_operation_name,
        request_body_sha256: canonical_request_body_sha256(request.body)?,
    };
    validate_key(&key)?;
    Ok(key)
}

fn key_from_flow(flow: &FlowRecord, effective_port: u16) -> Result<ReplayKey> {
    let request_body = exact_request_body(flow)?;
    request_key(&ReplayRequest {
        method: &flow.method,
        scheme: &flow.scheme,
        host: &flow.host,
        effective_port,
        path_and_query: &flow.path,
        body: request_body.as_bytes(),
    })
}

fn validate_source_flow(flow: &FlowRecord) -> Result<()> {
    ensure!(flow.error.is_none(), "flow has an upstream/capture error");
    ensure!(flow.status.is_some(), "flow has no final response status");
    ensure!(!flow.req_streamed, "request body was streamed");
    ensure!(!flow.req_truncated, "request body was truncated");
    ensure!(
        !flow.request_body_modified,
        "request body was modified after replay lookup; capture a fresh unmodified request"
    );
    ensure!(!flow.streamed, "response body was streamed or binary");
    ensure!(!flow.resp_truncated, "response body was truncated");
    ensure!(
        flow.redaction_policy.is_none()
            && flow.redaction_policy_version.is_none()
            && !flow.host_redacted
            && !flow.path_redacted
            && !flow.req_body_redacted
            && !flow.resp_body_redacted
            && !flow.error_redacted,
        "flow was redacted; exact request replay requires an unredacted capture"
    );
    normalize_method(&flow.method)?;
    normalize_scheme(&flow.scheme)?;
    normalize_host(&flow.host)?;
    split_target(&flow.path)?;
    exact_request_body(flow)?;
    exact_response_body(flow)?;
    validate_status(flow.status.context("flow has no final response status")?)?;
    sanitize_response_headers(&flow.resp_headers)?;
    Ok(())
}

fn exact_request_body(flow: &FlowRecord) -> Result<&str> {
    match &flow.req_body {
        Some(body) => {
            ensure!(
                body.len() as u64 == flow.req_len,
                "request body length mismatch: captured {} bytes, metadata says {}",
                body.len(),
                flow.req_len
            );
            Ok(body)
        }
        None => {
            ensure!(
                flow.req_len == 0,
                "request has {} bytes but no exact textual body",
                flow.req_len
            );
            Ok("")
        }
    }
}

fn exact_response_body(flow: &FlowRecord) -> Result<String> {
    let status = flow.status.context("flow has no final response status")?;
    let body_forbidden = flow.method.eq_ignore_ascii_case("HEAD") || status_forbids_body(status);
    match &flow.resp_body {
        Some(body) => {
            ensure!(
                !body_forbidden || body.is_empty(),
                "response carries a body forbidden by method/status"
            );
            ensure!(
                body.len() as u64 == flow.resp_len,
                "response body length mismatch: captured {} bytes, metadata says {}",
                body.len(),
                flow.resp_len
            );
            ensure!(
                body.len() as u64 <= MAX_RESPONSE_BODY_BYTES,
                "response body exceeds {} bytes",
                MAX_RESPONSE_BODY_BYTES
            );
            Ok(body.clone())
        }
        None => {
            ensure!(
                flow.resp_len == 0 || body_forbidden,
                "response has {} bytes but no exact textual body",
                flow.resp_len
            );
            Ok(String::new())
        }
    }
}

fn captured_representation_content_length(flow: &FlowRecord, status: u16) -> Result<Option<u64>> {
    if !flow.method.eq_ignore_ascii_case("HEAD") || !(200..400).contains(&status) {
        return Ok(None);
    }

    let mut captured = None;
    for (_, raw_value) in flow
        .resp_headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    {
        // RFC 9110 permits a repeated/comma-joined Content-Length only when
        // every advertised value is identical. Preserve that one semantic
        // value, never the framing field lines themselves.
        for raw_token in raw_value.split(',') {
            let token = raw_token.trim();
            ensure!(
                !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit()),
                "HEAD response has invalid Content-Length {raw_value:?}"
            );
            let value = token
                .parse::<u64>()
                .with_context(|| format!("HEAD response Content-Length is too large: {token:?}"))?;
            if let Some(existing) = captured {
                ensure!(
                    existing == value,
                    "HEAD response has conflicting Content-Length values {existing} and {value}"
                );
            } else {
                captured = Some(value);
            }
        }
    }
    Ok(captured)
}

fn replay_set_from_manifest<F>(
    manifest: ReplayManifest,
    manifest_bytes: &[u8],
    host_filter: Option<&str>,
    mut read_body: F,
) -> Result<ReplaySet>
where
    F: FnMut(&ReplayManifestEntry) -> Result<String>,
{
    validate_manifest_header(&manifest)?;
    let wanted_host = host_filter.map(normalize_host).transpose()?;
    let source_count = manifest.fixtures.len();
    let mut all_entries = BTreeMap::new();
    let mut total_bytes = 0_u64;

    for (index, entry) in manifest.fixtures.iter().enumerate() {
        validate_manifest_entry(entry)
            .with_context(|| format!("validate replay manifest entry {index}"))?;
        let body = read_body(entry)
            .with_context(|| format!("read replay manifest entry {index} response"))?;
        ensure!(
            body.len() as u64 == entry.response_bytes,
            "response byte count mismatch at entry {index}: expected {}, got {}",
            entry.response_bytes,
            body.len()
        );
        let actual_sha256 = sha256_bytes(body.as_bytes());
        ensure!(
            actual_sha256 == entry.response_sha256,
            "response SHA-256 mismatch at entry {index}: expected {}, got {}",
            entry.response_sha256,
            actual_sha256
        );
        total_bytes = total_bytes
            .checked_add(entry.response_bytes)
            .context("replay bundle response byte count overflow")?;
        ensure!(
            total_bytes <= MAX_TOTAL_RESPONSE_BYTES,
            "replay bundle bodies exceed {} bytes",
            MAX_TOTAL_RESPONSE_BYTES
        );

        let response = ReplayResponse {
            status: entry.status,
            headers: entry.response_headers.clone(),
            representation_content_length: entry.representation_content_length,
            body,
            body_sha256: entry.response_sha256.clone(),
        };
        validate_runtime_response(&entry.request_match, &response)?;
        if all_entries
            .insert(entry.request_match.clone(), response)
            .is_some()
        {
            bail!("ambiguous duplicate replay key at manifest entry {index}");
        }
    }

    let entries = match wanted_host {
        Some(host) => all_entries
            .into_iter()
            .filter(|(key, _)| key.host == host)
            .collect::<BTreeMap<_, _>>(),
        None => all_entries,
    };
    ensure!(
        !entries.is_empty(),
        "replay bundle has no entries matching the requested host"
    );

    let active_set_sha256 = active_entries_sha256(&entries)?;
    Ok(ReplaySet {
        entries,
        source_bundle_sha256: Some(sha256_bytes(manifest_bytes)),
        active_set_sha256,
        source_count,
    })
}

fn active_entries_sha256(entries: &BTreeMap<ReplayKey, ReplayResponse>) -> Result<String> {
    let canonical = entries
        .iter()
        .map(|(key, response)| ReplayRuntimeEntry {
            key: key.clone(),
            response: response.clone(),
        })
        .collect::<Vec<_>>();
    Ok(sha256_bytes(&serde_json::to_vec(&canonical)?))
}

fn validate_manifest_header(manifest: &ReplayManifest) -> Result<()> {
    ensure!(
        manifest.artifact_type == REPLAY_ARTIFACT_TYPE,
        "unsupported replay artifact type {:?}; expected {:?}",
        manifest.artifact_type,
        REPLAY_ARTIFACT_TYPE
    );
    ensure!(
        manifest.format_version == REPLAY_FORMAT_VERSION,
        "unsupported replay format version {}; expected {}",
        manifest.format_version,
        REPLAY_FORMAT_VERSION
    );
    ensure!(
        manifest.version == REPLAY_FORMAT_VERSION,
        "legacy version alias {} disagrees with format version {}",
        manifest.version,
        REPLAY_FORMAT_VERSION
    );
    ensure!(
        !manifest.generated_by.trim().is_empty(),
        "generated_by is empty"
    );
    ensure!(
        manifest.count == manifest.fixtures.len(),
        "manifest count {} does not match {} fixture entries",
        manifest.count,
        manifest.fixtures.len()
    );
    ensure!(manifest.count != 0, "replay bundle is empty");
    ensure!(
        manifest.count <= MAX_REPLAY_ENTRIES,
        "replay bundle has {} entries; maximum is {}",
        manifest.count,
        MAX_REPLAY_ENTRIES
    );
    Ok(())
}

fn validate_manifest_entry(entry: &ReplayManifestEntry) -> Result<()> {
    validate_key(&entry.request_match)?;
    ensure!(
        entry.method == entry.request_match.method,
        "flat method disagrees with match key"
    );
    ensure!(
        entry.scheme == entry.request_match.scheme,
        "flat scheme disagrees with match key"
    );
    ensure!(
        entry.host == entry.request_match.host,
        "flat host disagrees with match key"
    );
    ensure!(
        entry.port == entry.request_match.port,
        "flat port disagrees with match key"
    );
    ensure!(
        entry.path
            == join_target(
                &entry.request_match.path,
                entry.request_match.query.as_deref()
            ),
        "flat request target disagrees with match key"
    );
    ensure!(
        entry.operation_name == entry.request_match.graphql_operation_name,
        "flat GraphQL operation disagrees with match key"
    );
    ensure!(!entry.response_truncated, "response is marked truncated");
    validate_status(entry.status)?;
    validate_response_headers(&entry.response_headers)?;
    validate_representation_content_length(
        &entry.request_match,
        entry.status,
        entry.representation_content_length,
    )?;
    validate_sha256(&entry.response_sha256, "response_sha256")?;
    ensure!(
        entry.response_bytes <= MAX_RESPONSE_BODY_BYTES,
        "response claims {} bytes; maximum is {}",
        entry.response_bytes,
        MAX_RESPONSE_BODY_BYTES
    );
    match (&entry.response_file, entry.response_bytes) {
        (None, 0) => {
            ensure!(
                entry.response_sha256 == sha256_bytes(b""),
                "empty response has the wrong SHA-256"
            );
            Ok(())
        }
        (Some(path), bytes) if bytes > 0 => {
            validate_response_relative_path(path, &entry.response_sha256)
        }
        (None, bytes) => bail!("{bytes}-byte response has no response_file"),
        (Some(_), 0) => bail!("empty response must not reference a response_file"),
        (Some(_), _) => unreachable!(),
    }
}

fn validate_key(key: &ReplayKey) -> Result<()> {
    ensure!(
        key.method == normalize_method(&key.method)?,
        "method is not canonical"
    );
    ensure!(
        key.scheme == normalize_scheme(&key.scheme)?,
        "scheme is not canonical"
    );
    ensure!(
        key.host == normalize_host(&key.host)?,
        "host is not canonical"
    );
    ensure!(key.port != 0, "port must be non-zero");
    let target = join_target(&key.path, key.query.as_deref());
    let (path, query) = split_target(&target)?;
    ensure!(
        path == key.path && query == key.query,
        "path/query are not canonical"
    );
    if let Some(operation) = &key.graphql_operation_name {
        ensure!(!operation.trim().is_empty(), "GraphQL operation is blank");
        ensure!(
            operation == operation.trim() && !operation.chars().any(char::is_control),
            "GraphQL operation is not canonical"
        );
    }
    validate_sha256(&key.request_body_sha256, "request_body_sha256")
}

fn validate_runtime_response(key: &ReplayKey, response: &ReplayResponse) -> Result<()> {
    validate_status(response.status)?;
    validate_response_headers(&response.headers)?;
    validate_representation_content_length(
        key,
        response.status,
        response.representation_content_length,
    )?;
    validate_sha256(&response.body_sha256, "body_sha256")?;
    ensure!(
        response.body.len() as u64 <= MAX_RESPONSE_BODY_BYTES,
        "response body exceeds {} bytes",
        MAX_RESPONSE_BODY_BYTES
    );
    ensure!(
        sha256_bytes(response.body.as_bytes()) == response.body_sha256,
        "response body SHA-256 does not match"
    );
    ensure!(
        !(key.method == "HEAD" || status_forbids_body(response.status)) || response.body.is_empty(),
        "response body is forbidden by method/status"
    );
    Ok(())
}

fn validate_representation_content_length(
    key: &ReplayKey,
    status: u16,
    representation_content_length: Option<u64>,
) -> Result<()> {
    if representation_content_length.is_some() {
        ensure!(
            key.method == "HEAD"
                && (200..400).contains(&status)
                && (!status_forbids_body(status) || status == 304),
            "representation_content_length is only valid for HEAD 2xx/3xx responses"
        );
    }
    Ok(())
}

fn validate_status(status: u16) -> Result<()> {
    ensure!(
        (200..=599).contains(&status),
        "invalid final HTTP status {status}; expected 200..=599"
    );
    Ok(())
}

fn validate_response_headers(headers: &[(String, String)]) -> Result<()> {
    let nominated = connection_nominated_headers(headers)?;
    for (name, value) in headers {
        name.parse::<http::header::HeaderName>()
            .with_context(|| format!("invalid HTTP response header name {name:?}"))?;
        value
            .parse::<http::header::HeaderValue>()
            .with_context(|| format!("invalid HTTP response header value for {name:?}"))?;
        let lower = name.to_ascii_lowercase();
        ensure!(
            !is_managed_response_header(&lower) && !nominated.contains(&lower),
            "replay response must not persist framing or hop-by-hop header {name:?}"
        );
    }
    Ok(())
}

fn sanitize_response_headers(headers: &[(String, String)]) -> Result<Vec<(String, String)>> {
    // Validate syntax before interpreting Connection nominations; malformed
    // captured headers must fail export, not be silently discarded.
    for (name, value) in headers {
        name.parse::<http::header::HeaderName>()
            .with_context(|| format!("invalid HTTP response header name {name:?}"))?;
        value
            .parse::<http::header::HeaderValue>()
            .with_context(|| format!("invalid HTTP response header value for {name:?}"))?;
    }
    let nominated = connection_nominated_headers(headers)?;
    let sanitized = headers
        .iter()
        .filter(|(name, _)| {
            let lower = name.to_ascii_lowercase();
            !is_managed_response_header(&lower) && !nominated.contains(&lower)
        })
        .cloned()
        .collect::<Vec<_>>();
    validate_response_headers(&sanitized)?;
    Ok(sanitized)
}

fn connection_nominated_headers(headers: &[(String, String)]) -> Result<BTreeSet<String>> {
    let mut nominated = BTreeSet::new();
    for (_, value) in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
    {
        for token in value.split(',') {
            let token = token.trim();
            ensure!(
                !token.is_empty(),
                "Connection header contains a blank token"
            );
            token
                .parse::<http::header::HeaderName>()
                .with_context(|| format!("invalid Connection-nominated header {token:?}"))?;
            nominated.insert(token.to_ascii_lowercase());
        }
    }
    Ok(nominated)
}

fn is_managed_response_header(name: &str) -> bool {
    matches!(
        name,
        "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "upgrade"
            | "trailer"
            | "te"
    )
}

fn normalize_method(method: &str) -> Result<String> {
    let trimmed = method.trim();
    ensure!(!trimmed.is_empty(), "HTTP method is empty");
    trimmed
        .parse::<http::Method>()
        .with_context(|| format!("invalid HTTP method {method:?}"))?;
    Ok(trimmed.to_ascii_uppercase())
}

fn normalize_scheme(scheme: &str) -> Result<String> {
    let normalized = scheme.trim().to_ascii_lowercase();
    ensure!(
        matches!(normalized.as_str(), "http" | "https"),
        "unsupported replay scheme {scheme:?}; expected http or https"
    );
    Ok(normalized)
}

fn normalize_host(host: &str) -> Result<String> {
    let mut normalized = host.trim().to_ascii_lowercase();
    if normalized.starts_with('[') && normalized.ends_with(']') {
        normalized = normalized[1..normalized.len() - 1].to_string();
    }
    if normalized.ends_with('.') {
        normalized.pop();
    }
    ensure!(!normalized.is_empty(), "host is empty");
    ensure!(
        !normalized.chars().any(char::is_whitespace)
            && !normalized.contains('/')
            && !normalized.contains('?')
            && !normalized.contains('#'),
        "invalid replay host {host:?}"
    );
    let authority = if normalized.contains(':') {
        format!("[{normalized}]:1")
    } else {
        format!("{normalized}:1")
    };
    authority
        .parse::<http::uri::Authority>()
        .with_context(|| format!("invalid replay host {host:?}"))?;
    Ok(normalized)
}

fn split_target(target: &str) -> Result<(String, Option<String>)> {
    ensure!(
        target.starts_with('/'),
        "request target must start with '/'"
    );
    ensure!(
        !target.contains('#') && !target.chars().any(char::is_control),
        "request target contains a fragment or control character"
    );
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(canonical_query(query)?)),
        None => (target, None),
    };
    ensure!(!path.is_empty(), "request path is empty");
    Ok((path.to_string(), query))
}

fn canonical_query(query: &str) -> Result<String> {
    if query.is_empty() {
        return Ok(String::new());
    }
    let mut pairs = Vec::new();
    for pair in query.split('&') {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        pairs.push((
            decode_query_component(raw_key)?,
            decode_query_component(raw_value)?,
        ));
    }
    pairs.sort();
    Ok(pairs
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                urlencoding::encode(&key),
                urlencoding::encode(&value)
            )
        })
        .collect::<Vec<_>>()
        .join("&"))
}

fn join_target(path: &str, query: Option<&str>) -> String {
    match query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    }
}

fn graphql_operation_name(query: Option<&str>, body: &[u8]) -> Result<Option<String>> {
    if let Some(query) = query {
        for pair in query.split('&') {
            let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = decode_query_component(raw_key)?;
            if key == "operationName" {
                let value = decode_query_component(raw_value)?;
                return normalized_operation_name(&value);
            }
        }
    }

    if body.is_empty() {
        return Ok(None);
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Ok(None);
    };
    match find_json_operation_name(&value) {
        Some(name) => normalized_operation_name(name),
        None => Ok(None),
    }
}

fn decode_query_component(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            ensure!(
                index + 2 < bytes.len()
                    && bytes[index + 1].is_ascii_hexdigit()
                    && bytes[index + 2].is_ascii_hexdigit(),
                "invalid percent-encoding in query component {value:?}"
            );
            index += 3;
        } else {
            index += 1;
        }
    }
    let form_value = value.replace('+', " ");
    urlencoding::decode(&form_value)
        .map(|decoded| decoded.into_owned())
        .with_context(|| format!("invalid percent-encoding in query component {value:?}"))
}

fn find_json_operation_name(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::Object(object) => object
            .get("operationName")
            .and_then(serde_json::Value::as_str),
        serde_json::Value::Array(values) => values.iter().find_map(find_json_operation_name),
        _ => None,
    }
}

fn normalized_operation_name(value: &str) -> Result<Option<String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    ensure!(
        !trimmed.chars().any(char::is_control),
        "GraphQL operation contains a control character"
    );
    Ok(Some(trimmed.to_string()))
}

fn canonical_request_body_sha256(body: &[u8]) -> Result<String> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Ok(sha256_bytes(body));
    };
    let canonical = canonical_json(value);
    Ok(sha256_bytes(&serde_json::to_vec(&canonical)?))
}

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let sorted = object
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            let mut object = serde_json::Map::new();
            for (key, value) in sorted {
                object.insert(key, value);
            }
            serde_json::Value::Object(object)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        scalar => scalar,
    }
}

fn stable_optional_min(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn status_forbids_body(status: u16) -> bool {
    (100..200).contains(&status) || status == 204 || status == 304
}

fn response_path(sha256: &str) -> String {
    format!("responses/{sha256}")
}

fn validate_response_relative_path(relative: &str, sha256: &str) -> Result<()> {
    validate_sha256(sha256, "response_sha256")?;
    ensure!(
        relative == response_path(sha256),
        "response path {relative:?} is not the content-addressed path for {sha256}"
    );
    let path = Path::new(relative);
    ensure!(!path.is_absolute(), "response path must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "response path contains traversal or non-normal components"
    );
    Ok(())
}

fn validate_sha256(value: &str, field: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{field} must be 64 lowercase hexadecimal characters"
    );
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn resolve_bundle_paths(from: &Path) -> Result<(PathBuf, PathBuf)> {
    let metadata = fs::symlink_metadata(from)
        .with_context(|| format!("inspect replay input {}", from.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "replay input {} must not be a symlink",
        from.display()
    );
    if metadata.is_dir() {
        Ok((from.to_path_buf(), from.join("manifest.json")))
    } else {
        ensure!(
            metadata.is_file(),
            "replay input must be a directory or manifest.json"
        );
        ensure!(
            from.file_name().and_then(|name| name.to_str()) == Some("manifest.json"),
            "replay manifest file must be named manifest.json"
        );
        let root = from
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Ok((root, from.to_path_buf()))
    }
}

fn ensure_directory(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.file_type().is_symlink() && metadata.is_dir(),
                "{label} {} must be a real directory, not a symlink or file",
                path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .with_context(|| format!("create {label} directory {}", path.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {label} {}", path.display()));
        }
    }
    Ok(())
}

fn require_existing_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_dir(),
        "{label} {} must be a real directory, not a symlink or file",
        path.display()
    );
    Ok(())
}

fn read_regular_file(path: &Path, max_bytes: u64, label: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "{label} {} must be a regular file, not a symlink",
        path.display()
    );
    ensure!(
        metadata.len() <= max_bytes,
        "{label} {} is {} bytes; maximum is {}",
        path.display(),
        metadata.len(),
        max_bytes
    );
    let bytes = fs::read(path).with_context(|| format!("read {label} {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= max_bytes,
        "{label} {} grew beyond {} bytes while reading",
        path.display(),
        max_bytes
    );
    Ok(bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    require_existing_directory(parent, "artifact parent")?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            !metadata.file_type().is_symlink() && metadata.is_file(),
            "artifact {} must be a regular file, not a symlink",
            path.display()
        );
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary artifact beside {}", path.display()))?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("write temporary artifact for {}", path.display()))?;
    temporary
        .flush()
        .with_context(|| format!("flush temporary artifact for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("sync temporary artifact for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace artifact {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_flow(id: &str, path: &str, request: &str, response: &str) -> FlowRecord {
        FlowRecord {
            id: id.into(),
            ts: 1.0,
            method: "POST".into(),
            scheme: "https".into(),
            host: "Api.Example.COM.".into(),
            path: path.into(),
            status: Some(207),
            req_headers: vec![("content-type".into(), "application/json".into())],
            resp_headers: vec![
                ("content-type".into(), "application/json".into()),
                ("x-fixture".into(), "one".into()),
            ],
            req_type: Some("application/json".into()),
            resp_type: Some("application/json".into()),
            req_len: request.len() as u64,
            resp_len: response.len() as u64,
            req_body: Some(request.into()),
            resp_body: Some(response.into()),
            ..Default::default()
        }
    }

    fn source<'a>(flow: &'a FlowRecord) -> ReplaySource<'a> {
        ReplaySource::new(flow, 443)
    }

    fn write_one(dir: &Path) -> (FlowRecord, WriteSummary) {
        let flow = sample_flow(
            "f1",
            "/graphql?case=e2e",
            r#"{"operationName":"ReplayE2E","variables":{"revision":1}}"#,
            r#"{"fixture":"REPLAY-E2E"}"#,
        );
        let summary = write_bundle(&[source(&flow)], dir).unwrap();
        (flow, summary)
    }

    #[test]
    fn round_trip_bundle_matches_every_exact_dimension() {
        let dir = tempfile::tempdir().unwrap();
        let (flow, summary) = write_one(dir.path());
        let set = load_bundle(dir.path(), None).unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(set.source_count(), 1);
        assert_eq!(
            set.source_bundle_sha256(),
            Some(summary.source_bundle_sha256.as_str())
        );
        assert_eq!(set.active_set_sha256(), summary.active_set_sha256);

        let request = ReplayRequest {
            method: "post",
            scheme: "HTTPS",
            host: "api.example.com",
            effective_port: 443,
            path_and_query: &flow.path,
            body: flow.req_body.as_deref().unwrap().as_bytes(),
        };
        let ReplayLookup::Hit(response) = set.lookup(&request).unwrap() else {
            panic!("exact request did not hit replay");
        };
        assert_eq!(response.status, 207);
        assert_eq!(response.body, r#"{"fixture":"REPLAY-E2E"}"#);
        assert_eq!(response.headers[1], ("x-fixture".into(), "one".into()));

        let mismatch = |request: ReplayRequest<'_>| {
            assert_eq!(set.lookup(&request).unwrap(), ReplayLookup::Miss)
        };
        mismatch(ReplayRequest {
            method: "GET",
            ..request
        });
        mismatch(ReplayRequest {
            scheme: "http",
            ..request
        });
        mismatch(ReplayRequest {
            host: "other.example.com",
            ..request
        });
        mismatch(ReplayRequest {
            effective_port: 444,
            ..request
        });
        mismatch(ReplayRequest {
            path_and_query: "/other?case=e2e",
            ..request
        });
        mismatch(ReplayRequest {
            path_and_query: "/graphql?case=other",
            ..request
        });
        mismatch(ReplayRequest {
            body: br#"{"operationName":"Other","variables":{"revision":1}}"#,
            ..request
        });
        mismatch(ReplayRequest {
            body: br#"{"operationName":"ReplayE2E","variables":{"revision":2}}"#,
            ..request
        });
    }

    #[test]
    fn build_is_input_order_independent_and_content_addressed() {
        let one = sample_flow("f99", "/one", "{}", r#"{"same":true}"#);
        let two = sample_flow("f1", "/two", "[]", r#"{"same":true}"#);
        let left = build_bundle(&[source(&one), source(&two)]).unwrap();
        let right = build_bundle(&[source(&two), source(&one)]).unwrap();
        assert_eq!(
            left.manifest_bytes().unwrap(),
            right.manifest_bytes().unwrap()
        );
        assert_eq!(left.response_files, right.response_files);
        assert_eq!(left.response_files.len(), 1, "equal bodies share one file");
        let relative = left.response_files.keys().next().unwrap();
        assert_eq!(relative, &response_path(&sha256_bytes(br#"{"same":true}"#)));
    }

    #[test]
    fn generated_and_mutated_bundles_cannot_exceed_the_loader_manifest_limit() {
        let mut oversized_source = sample_flow("large", "/x", "{}", "ok");
        oversized_source.path = format!("/{}", "x".repeat(MAX_MANIFEST_BYTES as usize));
        let error = format!(
            "{:#}",
            build_bundle(&[source(&oversized_source)]).unwrap_err()
        );
        assert!(error.contains("replay manifest is"), "{error}");
        assert!(error.contains("maximum is"), "{error}");

        let normal = sample_flow("normal", "/x", "{}", "ok");
        let mut mutated = build_bundle(&[source(&normal)]).unwrap();
        mutated.manifest.generated_by = "x".repeat(MAX_MANIFEST_BYTES as usize);
        assert!(mutated.manifest_bytes().is_err());
        assert!(mutated.into_replay_set(None).is_err());
    }

    #[test]
    fn identical_duplicate_captures_coalesce_but_conflicting_responses_fail() {
        let one = sample_flow("f1", "/same", "{}", "one");
        let mut two = one.clone();
        two.id = "f2".into();
        let coalesced = build_bundle(&[source(&one), source(&two)]).unwrap();
        assert_eq!(coalesced.manifest.count, 1);
        let reversed = build_bundle(&[source(&two), source(&one)]).unwrap();
        assert_eq!(
            coalesced.manifest_bytes().unwrap(),
            reversed.manifest_bytes().unwrap()
        );

        two.resp_body = Some("two".into());
        two.resp_len = 3;
        let error = build_bundle(&[source(&one), source(&two)])
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflicting responses"), "{error}");
    }

    #[test]
    fn json_body_and_query_pairs_are_canonical_but_values_remain_exact() {
        let left = request_key(&ReplayRequest {
            method: "POST",
            scheme: "https",
            host: "api.example.com",
            effective_port: 443,
            path_and_query: "/graphql?b=2&a=1&a=0&space=hello+world",
            body: br#" { "z": 2, "nested": { "b": true, "a": 1 } } "#,
        })
        .unwrap();
        let right = request_key(&ReplayRequest {
            path_and_query: "/graphql?space=hello%20world&a=0&b=2&a=1",
            body: br#"{"nested":{"a":1,"b":true},"z":2}"#,
            ..ReplayRequest {
                method: "POST",
                scheme: "https",
                host: "api.example.com",
                effective_port: 443,
                path_and_query: "",
                body: &[],
            }
        })
        .unwrap();
        assert_eq!(left, right);
        assert_eq!(
            left.query.as_deref(),
            Some("a=0&a=1&b=2&space=hello%20world")
        );

        let changed = request_key(&ReplayRequest {
            body: br#"{"nested":{"a":2,"b":true},"z":2}"#,
            ..ReplayRequest {
                method: "POST",
                scheme: "https",
                host: "api.example.com",
                effective_port: 443,
                path_and_query: "/graphql?space=hello%20world&a=0&b=2&a=1",
                body: &[],
            }
        })
        .unwrap();
        assert_ne!(left.request_body_sha256, changed.request_body_sha256);
        assert!(
            request_key(&ReplayRequest {
                path_and_query: "/graphql?token=%ZZ",
                ..ReplayRequest {
                    method: "GET",
                    scheme: "https",
                    host: "api.example.com",
                    effective_port: 443,
                    path_and_query: "",
                    body: &[],
                }
            })
            .is_err()
        );
    }

    #[test]
    fn incomplete_error_and_redacted_flows_are_rejected() {
        let base = sample_flow("f1", "/x", "{}", "ok");
        let mut cases = Vec::new();

        let mut errored = base.clone();
        errored.error = Some("boom".into());
        cases.push(errored);
        let mut req_streamed = base.clone();
        req_streamed.req_streamed = true;
        cases.push(req_streamed);
        let mut req_truncated = base.clone();
        req_truncated.req_truncated = true;
        cases.push(req_truncated);
        let mut request_body_modified = base.clone();
        request_body_modified.request_body_modified = true;
        cases.push(request_body_modified);
        let mut streamed = base.clone();
        streamed.streamed = true;
        cases.push(streamed);
        let mut truncated = base.clone();
        truncated.resp_truncated = true;
        cases.push(truncated);
        let mut missing = base.clone();
        missing.resp_body = None;
        cases.push(missing);
        let mut redacted = base.clone();
        redacted.path_redacted = true;
        cases.push(redacted);
        let mut policy = base.clone();
        policy.redaction_policy = Some("builtin".into());
        cases.push(policy);

        for flow in cases {
            assert!(
                build_bundle(&[source(&flow)]).is_err(),
                "accepted {}",
                flow.id
            );
        }
    }

    #[test]
    fn invalid_status_method_and_headers_are_rejected() {
        let base = sample_flow("f1", "/x", "{}", "ok");
        let mut status = base.clone();
        status.status = Some(199);
        assert!(build_bundle(&[source(&status)]).is_err());

        let mut method = base.clone();
        method.method = "bad method".into();
        assert!(build_bundle(&[source(&method)]).is_err());

        let mut header = base;
        header.resp_headers = vec![("bad header".into(), "value".into())];
        assert!(build_bundle(&[source(&header)]).is_err());
    }

    #[test]
    fn export_strips_managed_and_connection_nominated_headers_and_loader_rejects_them() {
        let mut flow = sample_flow("f1", "/x", "{}", "ok");
        flow.resp_headers.extend([
            ("content-length".into(), "2".into()),
            ("transfer-encoding".into(), "chunked".into()),
            ("connection".into(), "x-remove, keep-alive".into()),
            ("x-remove".into(), "gone".into()),
            ("x-keep".into(), "present".into()),
        ]);
        let bundle = build_bundle(&[source(&flow)]).unwrap();
        let headers = &bundle.manifest.fixtures[0].response_headers;
        assert!(headers.iter().any(|(name, _)| name == "x-keep"));
        for removed in [
            "content-length",
            "transfer-encoding",
            "connection",
            "x-remove",
        ] {
            assert!(
                !headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(removed)),
                "retained {removed}"
            );
        }

        let mut payload = bundle.into_replay_set(None).unwrap().to_payload();
        payload.entries[0]
            .response
            .headers
            .push(("content-length".into(), "2".into()));
        assert!(ReplaySet::from_payload(payload).is_err());
    }

    #[test]
    fn head_representation_content_length_survives_bundle_load_and_wire_validation() {
        let mut flow = sample_flow("head", "/head", "", "");
        flow.method = "HEAD".into();
        flow.status = Some(200);
        flow.req_body = None;
        flow.resp_body = None;
        flow.req_len = 0;
        flow.resp_len = 0;
        flow.resp_headers = vec![
            ("content-type".into(), "application/json".into()),
            ("content-length".into(), "1234, 1234".into()),
            ("Content-Length".into(), "1234".into()),
        ];

        let dir = tempfile::tempdir().unwrap();
        let summary = write_bundle(&[source(&flow)], dir.path()).unwrap();
        let manifest: ReplayManifest =
            serde_json::from_slice(&fs::read(&summary.manifest).unwrap()).unwrap();
        let entry = &manifest.fixtures[0];
        assert_eq!(entry.representation_content_length, Some(1234));
        assert!(
            !entry
                .response_headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        );

        let set = load_bundle(dir.path(), None).unwrap();
        let request = ReplayRequest {
            method: "HEAD",
            scheme: "https",
            host: "api.example.com",
            effective_port: 443,
            path_and_query: "/head",
            body: &[],
        };
        let ReplayLookup::Hit(response) = set.lookup(&request).unwrap() else {
            panic!("HEAD fixture did not survive bundle load");
        };
        assert_eq!(response.representation_content_length, Some(1234));
        assert!(response.body.is_empty());

        let payload = set.to_payload();
        assert_eq!(
            payload.entries[0].response.representation_content_length,
            Some(1234)
        );
        let restored = ReplaySet::from_payload(payload.clone()).unwrap();
        let ReplayLookup::Hit(response) = restored.lookup(&request).unwrap() else {
            panic!("HEAD fixture did not survive wire validation");
        };
        assert_eq!(response.representation_content_length, Some(1234));

        let mut fingerprint_tamper = payload.clone();
        fingerprint_tamper.entries[0]
            .response
            .representation_content_length = Some(1235);
        assert!(ReplaySet::from_payload(fingerprint_tamper).is_err());

        let mut invalid_scope = payload;
        invalid_scope.entries[0].key.method = "GET".into();
        let error = format!("{:#}", ReplaySet::from_payload(invalid_scope).unwrap_err());
        assert!(error.contains("representation_content_length"), "{error}");
    }

    #[test]
    fn head_content_length_rejects_invalid_or_conflicting_capture_values() {
        let mut flow = sample_flow("head", "/head", "", "");
        flow.method = "HEAD".into();
        flow.status = Some(200);
        flow.req_body = None;
        flow.resp_body = None;
        flow.req_len = 0;
        flow.resp_len = 0;

        flow.resp_headers = vec![("content-length".into(), "twelve".into())];
        assert!(build_bundle(&[source(&flow)]).is_err());

        flow.resp_headers = vec![
            ("content-length".into(), "12".into()),
            ("Content-Length".into(), "13".into()),
        ];
        assert!(build_bundle(&[source(&flow)]).is_err());
    }

    #[test]
    fn manifest_type_version_and_count_are_strict() {
        for mutate in [
            |value: &mut serde_json::Value| value["artifact_type"] = json!("other"),
            |value: &mut serde_json::Value| value["format_version"] = json!(2),
            |value: &mut serde_json::Value| value["count"] = json!(2),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write_one(dir.path());
            let path = dir.path().join("manifest.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            mutate(&mut value);
            fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
            assert!(load_bundle(dir.path(), None).is_err());
        }
    }

    #[test]
    fn missing_size_and_hash_mismatches_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (_, summary) = write_one(dir.path());
        let manifest: ReplayManifest =
            serde_json::from_slice(&fs::read(&summary.manifest).unwrap()).unwrap();
        let body = dir
            .path()
            .join(manifest.fixtures[0].response_file.as_ref().unwrap());

        fs::write(&body, "different").unwrap();
        assert!(load_bundle(dir.path(), None).is_err());
        fs::remove_file(&body).unwrap();
        assert!(load_bundle(dir.path(), None).is_err());
    }

    #[test]
    fn traversal_and_non_content_addressed_paths_are_rejected_before_read() {
        let dir = tempfile::tempdir().unwrap();
        write_one(dir.path());
        let path = dir.path().join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["fixtures"][0]["response_file"] = json!("responses/../outside");
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let error = load_bundle(dir.path(), None).unwrap_err().to_string();
        assert!(error.contains("validate replay manifest entry"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_bundle_inputs_and_response_files_are_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let (_, summary) = write_one(dir.path());
        let link_root = tempfile::tempdir().unwrap();
        let root_link = link_root.path().join("bundle");
        symlink(dir.path(), &root_link).unwrap();
        assert!(load_bundle(&root_link, None).is_err());

        let manifest: ReplayManifest =
            serde_json::from_slice(&fs::read(&summary.manifest).unwrap()).unwrap();
        let response = dir
            .path()
            .join(manifest.fixtures[0].response_file.as_ref().unwrap());
        let real = dir.path().join("real-response");
        fs::rename(&response, &real).unwrap();
        symlink(&real, &response).unwrap();
        assert!(load_bundle(dir.path(), None).is_err());
    }

    #[test]
    fn exact_host_filter_validates_first_and_never_selects_substrings() {
        let dir = tempfile::tempdir().unwrap();
        write_one(dir.path());
        assert_eq!(
            load_bundle(dir.path(), Some("API.EXAMPLE.COM."))
                .unwrap()
                .len(),
            1
        );
        assert!(load_bundle(dir.path(), Some("example.com")).is_err());
    }

    #[test]
    fn host_filtered_sets_share_source_identity_but_have_distinct_active_fingerprints() {
        let dir = tempfile::tempdir().unwrap();
        let mut one = sample_flow("f1", "/x", "{}", "one");
        one.host = "a.example.com".into();
        let mut two = sample_flow("f2", "/x", "{}", "two");
        two.host = "b.example.com".into();
        write_bundle(&[source(&one), source(&two)], dir.path()).unwrap();

        let a = load_bundle(dir.path(), Some("a.example.com")).unwrap();
        let b = load_bundle(dir.path(), Some("b.example.com")).unwrap();
        assert_eq!(a.source_count(), 2);
        assert_eq!(b.source_count(), 2);
        assert_eq!(a.source_bundle_sha256(), b.source_bundle_sha256());
        assert_ne!(a.active_set_sha256(), b.active_set_sha256());
    }

    #[test]
    fn daemon_payload_round_trip_revalidates_and_rejects_duplicates() {
        let flow = sample_flow("f1", "/x", "{}", "ok");
        let set = build_bundle(&[source(&flow)])
            .unwrap()
            .into_replay_set(None)
            .unwrap();
        let payload = set.to_payload();
        let restored = ReplaySet::from_payload(payload.clone()).unwrap();
        assert_eq!(restored.len(), 1);
        assert!(matches!(
            restored
                .lookup(&ReplayRequest {
                    method: "POST",
                    scheme: "https",
                    host: "api.example.com",
                    effective_port: 443,
                    path_and_query: "/x",
                    body: b"{}",
                })
                .unwrap(),
            ReplayLookup::Hit(_)
        ));

        let mut duplicated = payload.clone();
        duplicated.entries.push(duplicated.entries[0].clone());
        assert!(ReplaySet::from_payload(duplicated).is_err());

        let mut wrong_hash = payload.clone();
        wrong_hash.entries[0].response.body.push('!');
        assert!(ReplaySet::from_payload(wrong_hash).is_err());

        // Even an internally consistent entry mutation must not retain the old
        // active-set fingerprint.
        let mut consistent_tamper = payload.clone();
        consistent_tamper.entries[0].response.status = 208;
        assert!(ReplaySet::from_payload(consistent_tamper).is_err());

        let mut wrong_status = payload.clone();
        wrong_status.entries[0].response.status = 199;
        assert!(ReplaySet::from_payload(wrong_status).is_err());

        let mut wrong_header = payload.clone();
        wrong_header.entries[0].response.headers = vec![("bad header".into(), "x".into())];
        assert!(ReplaySet::from_payload(wrong_header).is_err());

        let mut wrong_type = payload;
        wrong_type.artifact_type = "old_replay_set".into();
        assert!(ReplaySet::from_payload(wrong_type).is_err());

        let mut wire = serde_json::to_value(set.to_payload()).unwrap();
        wire["unexpected"] = json!(true);
        assert!(ReplaySet::from_wire_value(wire).is_err());
    }

    #[test]
    fn graphql_operation_comes_from_query_or_json_batch() {
        let query = request_key(&ReplayRequest {
            method: "POST",
            scheme: "https",
            host: "api.example.com",
            effective_port: 443,
            path_and_query: "/graphql?operationName=From%20Query",
            body: br#"{"operationName":"FromBody"}"#,
        })
        .unwrap();
        assert_eq!(query.graphql_operation_name.as_deref(), Some("From Query"));

        let batch = request_key(&ReplayRequest {
            path_and_query: "/graphql",
            body: br#"[{"operationName":"FromBatch"}]"#,
            ..ReplayRequest {
                method: "POST",
                scheme: "https",
                host: "api.example.com",
                effective_port: 443,
                path_and_query: "",
                body: &[],
            }
        })
        .unwrap();
        assert_eq!(batch.graphql_operation_name.as_deref(), Some("FromBatch"));
    }

    #[test]
    fn strict_legacy_jsonl_loader_uses_default_ports_and_rejects_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flows.jsonl");
        let flow = sample_flow("f1", "/x", "{}", "ok");
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&flow).unwrap()),
        )
        .unwrap();
        let set = load_legacy_jsonl_with_default_ports(&path, None).unwrap();
        assert_eq!(set.to_payload().entries[0].key.port, 443);

        fs::write(&path, "{not-json}\n").unwrap();
        assert!(load_legacy_jsonl_with_default_ports(&path, None).is_err());
        fs::write(
            &path,
            serde_json::to_string(&json!({"type":"tls_error"})).unwrap(),
        )
        .unwrap();
        assert!(load_legacy_jsonl_with_default_ports(&path, None).is_err());
    }

    #[test]
    fn manifest_unknown_fields_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_one(dir.path());
        let path = dir.path().join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["unexpected"] = json!(true);
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        assert!(load_bundle(dir.path(), None).is_err());
    }
}
