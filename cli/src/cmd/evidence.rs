//! Correlated, projected evidence. Captures read existing state, never start a
//! device/server/proxy, and persist only selected fields, never raw private files.
use anyhow::{Context, Result, bail};
use base64::Engine;
use clap::{Args, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ids::Serial;

#[derive(Args)]
pub struct EvidenceArgs {
    #[command(subcommand)]
    pub command: EvidenceCmd,
}

#[derive(Subcommand)]
pub enum EvidenceCmd {
    /// Link a UI snapshot, network checkpoint, video marker and selected fields.
    Checkpoint {
        label: String,
        /// New or existing ShadowDroid evidence bundle directory.
        #[arg(long)]
        out: PathBuf,
        /// JSON storage/flow projections; only selected fields are retained.
        #[arg(long)]
        spec: Option<PathBuf>,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..=86400))]
        since_seconds: u32,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1000))]
        flow_limit: u32,
    },
    /// Read all saved checkpoints, deduplicate events, and sort by event time.
    Timeline { bundle: PathBuf },
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    #[serde(default)]
    storage: Vec<StorageProbe>,
    #[serde(default)]
    flows: Vec<FlowProbe>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageProbe {
    name: String,
    package: String,
    path: String,
    /// Omit for a JSON file; set to a SharedPreferences string containing JSON.
    preference_key: Option<String>,
    fields: BTreeMap<String, Projection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FlowProbe {
    name: String,
    host: String,
    #[serde(default)]
    path: String,
    operation_name: Option<String>,
    body: Body,
    fields: BTreeMap<String, Projection>,
    /// Optional pointer to an array of telemetry events inside the body.
    events_pointer: Option<String>,
    /// Relative to each event; timestamps are explicitly milliseconds or seconds.
    event_time_pointer: Option<String>,
    #[serde(default)]
    time_unit: TimeUnit,
    correlation_pointer: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Body {
    Request,
    Response,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TimeUnit {
    #[default]
    Milliseconds,
    Seconds,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Projection {
    pointer: String,
    #[serde(default)]
    mode: ProjectionMode,
    claim: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProjectionMode {
    #[default]
    Value,
    Sha256,
    JwtClaim,
}

fn validate_spec(spec: &Spec) -> Result<()> {
    if spec.storage.len() + spec.flows.len() > 100 {
        bail!("at most 100 probes are supported");
    }
    let mut names = HashSet::new();
    for (name, fields) in spec
        .storage
        .iter()
        .map(|p| (&p.name, &p.fields))
        .chain(spec.flows.iter().map(|p| (&p.name, &p.fields)))
    {
        if name.is_empty() || !names.insert(name) {
            bail!("probe names must be nonempty and unique");
        }
        if fields.is_empty() || fields.len() > 100 {
            bail!("each probe requires 1 to 100 fields");
        }
        for projection in fields.values() {
            if (!projection.pointer.is_empty() && !projection.pointer.starts_with('/'))
                || projection
                    .pointer
                    .split('~')
                    .skip(1)
                    .any(|s| !s.starts_with(['0', '1']))
            {
                bail!("projection requires an RFC 6901 JSON pointer");
            }
            if matches!(projection.mode, ProjectionMode::JwtClaim)
                && projection.claim.as_deref().is_none_or(str::is_empty)
            {
                bail!("jwt_claim projection requires a claim name");
            }
        }
    }
    if spec.flows.iter().any(|p| p.host.is_empty()) {
        bail!("flow probes require a host filter");
    }
    Ok(())
}

fn fingerprint(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn project(
    document: &Value,
    fields: &BTreeMap<String, Projection>,
    policy: &crate::redaction::Policy,
) -> Value {
    // Value projections need the source keys (including sensitive ancestors)
    // and known literals before fields become generic name/value rows. Keep
    // fingerprint and explicit JWT-claim projections on the original input.
    let redacted_document = fields
        .values()
        .any(|p| matches!(p.mode, ProjectionMode::Value))
        .then(|| policy.redact_source_value(document));
    let mut result = Vec::new();
    for (name, projection) in fields {
        let Some(value) = document.pointer(&projection.pointer) else {
            result.push(json!({"name":name,"present":false}));
            continue;
        };
        let redacted = value.as_str().is_some_and(|s| s.contains("<redacted:"));
        let mut selected = if redacted {
            json!({"present":true,"unavailable":"source_redacted"})
        } else {
            match projection.mode {
                ProjectionMode::Value => match redacted_document
                    .as_ref()
                    .and_then(|source| source.pointer(&projection.pointer))
                {
                    Some(value) => json!({"present":true,"value":value}),
                    // A sensitive ancestor may have become a placeholder,
                    // hiding this path. The original field was still present.
                    None => json!({"present":true,"unavailable":"source_redacted"}),
                },
                ProjectionMode::Sha256 => {
                    let bytes = value
                        .as_str()
                        .map(|s| s.as_bytes().to_vec())
                        .unwrap_or_else(|| serde_json::to_vec(value).unwrap());
                    json!({"present":true,"fingerprint":fingerprint(&bytes)})
                }
                ProjectionMode::JwtClaim => {
                    let claim = (|| {
                        let token = value.as_str()?;
                        let mut parts = token.split('.');
                        parts.next()?;
                        let payload = parts.next()?;
                        parts.next()?;
                        if parts.next().is_some() {
                            return None;
                        }
                        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(payload.trim_end_matches('='))
                            .ok()?;
                        let claims: Value = serde_json::from_slice(&bytes).ok()?;
                        claims.get(projection.claim.as_deref()?).cloned()
                    })();
                    match claim {
                        Some(value) => {
                            json!({"present":true,"value":value,"signature_verified":false})
                        }
                        None => json!({"present":true,"unavailable":"jwt_claim_unavailable"}),
                    }
                }
            }
        };
        selected["name"] = name.clone().into();
        result.push(selected);
    }
    Value::Array(result)
}

fn preference_json(bytes: &[u8], key: &str) -> Result<Value> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(std::str::from_utf8(bytes)?);
    let mut selected = None;
    let mut depth = 0usize;
    let mut collecting = false;
    let mut text = String::new();
    loop {
        match reader.read_event()? {
            Event::DocType(_) => bail!("SharedPreferences DTDs are unsupported"),
            Event::Start(e) => {
                depth += 1;
                if depth == 1 && e.name().as_ref() != b"map" {
                    bail!("expected SharedPreferences map");
                }
                if collecting {
                    bail!("nested XML inside preference string");
                }
                if depth == 2 && e.name().as_ref() == b"string" {
                    for attribute in e.attributes() {
                        let attribute = attribute?;
                        if attribute.key.as_ref() == b"name"
                            && attribute
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                                .as_ref()
                                == key
                        {
                            if selected.is_some() {
                                bail!("duplicate preference key");
                            }
                            collecting = true;
                            text.clear();
                        }
                    }
                }
            }
            Event::Text(e) if collecting => text.push_str(&e.decode()?),
            // quick-xml emits references separately from text. Resolve the
            // predefined/numeric entities and reject unknown names as before.
            Event::GeneralRef(e) if collecting => {
                text.push_str(&quick_xml::escape::unescape(&format!("&{};", e.decode()?))?);
            }
            Event::CData(e) if collecting => text.push_str(std::str::from_utf8(e.as_ref())?),
            Event::End(_) => {
                if collecting && depth == 2 {
                    selected = Some(serde_json::from_str(&text).context("preference is not JSON")?);
                    collecting = false;
                }
                depth = depth.checked_sub(1).context("unbalanced preference XML")?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 {
        bail!("incomplete preference XML");
    }
    selected.context("preference key not found")
}

fn write_private(path: &Path, value: &Value) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("missing parent")?)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .context("publish evidence without overwriting a checkpoint")?;
    Ok(())
}

fn bundle_lock(out: &Path) -> Result<std::fs::File> {
    if !out.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(out)?;
        write_private(
            &out.join("evidence.json"),
            &json!({"schema_version":1,"type":"shadowdroid_evidence"}),
        )?;
    }
    let marker: Value = serde_json::from_slice(&std::fs::read(out.join("evidence.json"))?)?;
    if marker["type"] != "shadowdroid_evidence" || marker["schema_version"] != 1 {
        bail!("not an evidence bundle");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(out.join(".capture.lock"))?;
    lock.try_lock()
        .map_err(|_| anyhow::anyhow!("another checkpoint is writing this bundle"))?;
    Ok(lock)
}

fn probe_result(result: Result<Value>, name: &str, errors: &mut Vec<Value>) -> Value {
    match result {
        Ok(value) => value,
        Err(error) => {
            let code = crate::cli::error_code_of(&error);
            errors.push(json!({"probe":name,"code":code}));
            json!({"available":false,"code":code})
        }
    }
}

pub async fn checkpoint(serial: &Serial, args: &EvidenceCmd) -> Result<()> {
    let EvidenceCmd::Checkpoint {
        label,
        out,
        spec,
        since_seconds,
        flow_limit,
    } = args
    else {
        unreachable!()
    };
    if label.trim().is_empty() || label.chars().count() > 1000 {
        bail!("checkpoint label must have 1 to 1000 characters");
    }
    let spec: Spec = spec
        .as_ref()
        .map(|path| -> Result<Spec> { Ok(serde_json::from_slice(&std::fs::read(path)?)?) })
        .transpose()?
        .unwrap_or_default();
    validate_spec(&spec)?;
    let _lock = bundle_lock(out)?;
    let id = crate::net::new_startup_id();
    let started = crate::events::now_ts();
    let policy =
        crate::redaction::active_policy().unwrap_or_else(crate::redaction::Policy::builtin);
    let mut errors = Vec::new();
    let screen = probe_result(
        async {
            let client = crate::device::installer::probe_existing(serial, false)
                .await?
                .context("existing UI server unavailable")?;
            Ok(serde_json::to_value(client.screen().await?)?)
        }
        .await,
        "screen",
        &mut errors,
    );
    let mut network = probe_result(
        crate::net::commands::checkpoint_value(serial).await,
        "network",
        &mut errors,
    );
    if let Some(capture) = network["capture_session_id"].as_str() {
        network["capture_ref"] = fingerprint(capture.as_bytes()).into();
    }
    let video = probe_result(
        crate::video::checkpoint_marker(serial, &format!("{id}: {label}")).await,
        "video",
        &mut errors,
    );
    let mut records = Vec::new();
    for probe in &spec.storage {
        let observed = crate::events::now_ts();
        let fields = probe_result(
            async {
                let bytes =
                    super::app_state::private_read_evidence(serial, &probe.package, &probe.path)
                        .await?;
                let document = match &probe.preference_key {
                    Some(key) => preference_json(&bytes, key)?,
                    None => serde_json::from_slice(&bytes)?,
                };
                Ok(project(&document, &probe.fields, &policy))
            }
            .await,
            &probe.name,
            &mut errors,
        );
        records.push(json!({"kind":"storage","record_id":format!("{id}:{}",probe.name),
            "checkpoint_id":id,"probe":probe.name,"observed_at_ms":observed*1000.0,"fields":fields}));
    }
    let flows = crate::net::store::read_filtered(
        serial,
        &crate::net::Matcher::default(),
        *flow_limit as usize,
    )
    .unwrap_or_else(|error| {
        errors.push(json!({"probe":"flows","code":crate::cli::error_code_of(&error)}));
        Vec::new()
    });
    for flow in flows
        .into_iter()
        .filter(|f| f.ts >= started - f64::from(*since_seconds))
    {
        for probe in &spec.flows {
            if !flow.host.contains(&probe.host) || !flow.path.contains(&probe.path) {
                continue;
            }
            if let Some(operation) = &probe.operation_name {
                let matcher = crate::net::rule::RuleMatcher::GraphqlOperation {
                    equals: operation.clone(),
                };
                if !matcher.matches(&crate::net::MatchContext {
                    host: &flow.host,
                    path: &flow.path,
                    method: &flow.method,
                    status: flow.status,
                    content_type: flow.req_type.as_deref(),
                    body: flow.req_body.as_deref().unwrap_or("").as_bytes(),
                    direction: None,
                    opcode: None,
                }) {
                    continue;
                }
            }
            let (body, redacted) = match probe.body {
                Body::Request => (flow.req_body.as_deref(), flow.req_body_redacted),
                Body::Response => (flow.resp_body.as_deref(), flow.resp_body_redacted),
            };
            let document = body.and_then(|body| serde_json::from_str::<Value>(body).ok());
            let Some(document) = document else {
                errors.push(
                    json!({"probe":probe.name,"flow_id":flow.id,"code":"flow_json_unavailable"}),
                );
                continue;
            };
            let documents = if let Some(pointer) = &probe.events_pointer {
                match document.pointer(pointer).and_then(Value::as_array) {
                    Some(events) => events.iter().collect::<Vec<_>>(),
                    None => {
                        errors.push(json!({"probe":probe.name,"flow_id":flow.id,"code":"events_array_missing"}));
                        continue;
                    }
                }
            } else {
                vec![&document]
            };
            for (index, event) in documents.into_iter().enumerate() {
                let capture_ref = fingerprint(flow.capture_session_id.as_bytes());
                let event_time = probe
                    .event_time_pointer
                    .as_ref()
                    .and_then(|p| event.pointer(p))
                    .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
                    .map(|v| match probe.time_unit {
                        TimeUnit::Milliseconds => v,
                        TimeUnit::Seconds => v * 1000.0,
                    });
                let correlation = probe
                    .correlation_pointer
                    .as_ref()
                    .and_then(|p| event.pointer(p))
                    .and_then(Value::as_str)
                    .filter(|v| !v.contains("<redacted:"))
                    .map(|v| fingerprint(v.as_bytes()));
                records.push(json!({"kind":if probe.events_pointer.is_some() {"sdk_event"} else {"network"},
                    "record_id":format!("{capture_ref}:{}:{}:{index}",flow.id,probe.name),
                    "checkpoint_id":id,"probe":probe.name,"flow_id":flow.id,"capture_ref":capture_ref,
                    "observed_at_ms":flow.ts*1000.0,"event_time_ms":event_time,"correlation_ref":correlation,
                    "source_body_redacted":redacted,"modified":flow.modified,"rule_ids":flow.rule_ids,
                    "fields":project(event,&probe.fields,&policy)}));
            }
        }
    }
    let saved = policy.redact_output(json!({"schema_version":1,"type":"evidence_checkpoint",
        "checkpoint_id":id,"label":label,"started_at_ms":started*1000.0,
        "finished_at_ms":crate::events::now_ts()*1000.0,"screen":screen,
        "network_checkpoint":network,"video_marker":video,"records":records,"errors":errors,
        "partial":!errors.is_empty(),"capture_is_atomic":false}));
    let path = out.join(format!("checkpoint-{id}.json"));
    write_private(&path, &saved)?;
    if !errors.is_empty() {
        return Err(crate::diagnostic::DiagnosticError::new(
            "evidence_partial",
            "evidence",
            "checkpoint saved with unavailable evidence",
        )
        .detail(json!({"path":path,"errors":errors}))
        .next_actions([format!(
            "shadowdroid evidence timeline {}",
            crate::events::shell_token(&out.to_string_lossy())
        )])
        .into());
    }
    crate::events::emit_action(
        "evidence_checkpoint",
        &json!({"path":path,"bundle":out,"checkpoint_id":id,"records":records.len()}),
    );
    Ok(())
}

pub fn timeline(bundle: &Path) -> Result<()> {
    let mut checkpoints = Vec::new();
    for entry in std::fs::read_dir(bundle)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("checkpoint-") && n.ends_with(".json"))
        {
            checkpoints.push(serde_json::from_slice::<Value>(&std::fs::read(path)?)?);
        }
    }
    checkpoints.sort_by(|a, b| {
        a["checkpoint_id"]
            .as_str()
            .cmp(&b["checkpoint_id"].as_str())
    });
    let records = merge_records(&checkpoints);
    let value = crate::redaction::active_policy().unwrap_or_else(crate::redaction::Policy::builtin)
        .redact_output(json!({"bundle":bundle,"checkpoints":checkpoints.len(),"records":records,
            "ordering":"event_time_ms when available, otherwise observed_at_ms; clocks are not automatically aligned"}));
    crate::events::emit_action("evidence_timeline", &value);
    Ok(())
}

fn merge_records(checkpoints: &[Value]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut records: Vec<Value> = checkpoints
        .iter()
        .filter_map(|c| c["records"].as_array())
        .flatten()
        .filter(|r| seen.insert(r["record_id"].as_str().unwrap_or_default().to_owned()))
        .cloned()
        .collect();
    for checkpoint in checkpoints {
        if checkpoint["type"] != "evidence_checkpoint" {
            continue;
        }
        let Some(id) = checkpoint["checkpoint_id"].as_str() else {
            continue;
        };
        if seen.insert(id.to_owned()) {
            records.push(json!({"kind":"checkpoint","record_id":id,"checkpoint_id":id,
                "label":checkpoint["label"],"observed_at_ms":checkpoint["started_at_ms"],
                "finished_at_ms":checkpoint["finished_at_ms"],"partial":checkpoint["partial"],
                "checkpoint_file":format!("checkpoint-{id}.json"),"screen_pointer":"/screen",
                "screen_hash":checkpoint["screen"]["screen_hash"],
                "network_checkpoint":checkpoint["network_checkpoint"],"video_marker":checkpoint["video_marker"],
                "errors":checkpoint["errors"]}));
        }
    }
    let time = |v: &Value| {
        v["event_time_ms"]
            .as_f64()
            .or_else(|| v["observed_at_ms"].as_f64())
            .unwrap_or(0.0)
    };
    records.sort_by(|a, b| {
        time(a)
            .total_cmp(&time(b))
            .then_with(|| a["record_id"].as_str().cmp(&b["record_id"].as_str()))
    });
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    fn named_fields(value: Value) -> Value {
        Value::Object(
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|row| (row["name"].as_str().unwrap().to_owned(), row.clone()))
                .collect(),
        )
    }
    #[test]
    fn projections_preserve_missing_vs_null_and_fingerprint_credentials() {
        let fields = serde_json::from_value(json!({"missing":{"pointer":"/type"},"null":{"pointer":"/null"},
            "token":{"pointer":"/access","mode":"sha256"},"claim":{"pointer":"/access","mode":"jwt_claim","claim":"subt"}})).unwrap();
        let token = format!(
            "e30.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"subt":"registered"}"#)
        );
        let projected_raw = project(
            &json!({"null":null,"access":token}),
            &fields,
            &crate::redaction::Policy::builtin(),
        );
        let projected =
            named_fields(crate::redaction::Policy::builtin().redact_output(projected_raw));
        assert_eq!(projected["missing"]["present"], false);
        assert_eq!(projected["null"]["present"], true);
        assert!(projected["null"]["value"].is_null());
        assert_eq!(projected["claim"]["value"], "registered");
        assert_eq!(projected["claim"]["signature_verified"], false);
        assert!(!projected.to_string().contains(&token));
        assert_eq!(
            named_fields(project(
                &json!({"access":"<redacted:token>"}),
                &fields,
                &crate::redaction::Policy::builtin()
            ))["token"]["unavailable"],
            "source_redacted"
        );
    }
    #[test]
    fn value_projections_redact_source_keys_ancestors_and_aliases_before_saving() {
        let policy = crate::redaction::Policy::new(crate::redaction::PolicySpec {
            json_keys: vec!["customerId".into(), "private/key".into()],
            patterns: vec!["ORDER-[0-9]+".into()],
        })
        .unwrap();
        let document = json!({
            "password":"opaque-password", "accessToken":"opaque-token",
            "customerId":"opaque-customer", "alias":"opaque-password",
            "private/key":{"nested":"opaque-nested"},
            "profile":{"password":"nested-password","display":"public"},
            "order":"ORDER-1234", "null":null,
            "redaction":{"classification":"public", "secret":"metadata-secret"}
        });
        let fields = serde_json::from_value(json!({
            "p":{"pointer":"/password"}, "t":{"pointer":"/accessToken"},
            "customer":{"pointer":"/customerId"}, "alias":{"pointer":"/alias"},
            "nested":{"pointer":"/private~1key/nested"},
            "profile":{"pointer":"/profile"}, "order":{"pointer":"/order"},
            "null":{"pointer":"/null"}, "missing":{"pointer":"/missing"},
            "metadata":{"pointer":"/redaction/classification"},
            "metadata-value":{"pointer":"/redaction/secret"}
        }))
        .unwrap();
        let saved = policy
            .redact_output(json!({"records":[{"fields":project(&document,&fields,&policy)}]}));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.json");
        write_private(&path, &saved).unwrap();
        let bytes = std::fs::read_to_string(path).unwrap();
        for secret in [
            "opaque-password",
            "opaque-token",
            "opaque-customer",
            "opaque-nested",
            "nested-password",
            "metadata-secret",
            "ORDER-1234",
        ] {
            assert!(
                !bytes.contains(&format!("\"{secret}\"")),
                "saved secret: {secret}"
            );
        }
        let rows = named_fields(saved["records"][0]["fields"].clone());
        assert_eq!(rows["p"]["value"], "<redacted:secret>");
        assert_eq!(rows["t"]["value"], "<redacted:token>");
        assert_eq!(rows["customer"]["value"], "<redacted:configured>");
        assert_eq!(rows["nested"]["present"], true);
        assert_eq!(rows["nested"]["unavailable"], "source_redacted");
        assert_eq!(rows["profile"]["value"]["display"], "public");
        assert_eq!(rows["metadata"]["value"], "public");
        assert_eq!(rows["null"]["present"], true);
        assert!(rows["null"]["value"].is_null());
        assert_eq!(rows["missing"]["present"], false);
    }

    #[test]
    fn value_redaction_does_not_change_fingerprints_or_explicit_jwt_claims() {
        let policy = crate::redaction::Policy::builtin();
        let token = format!(
            "e30.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"subt":"registered"}"#)
        );
        let fields = serde_json::from_value(json!({
            "value":{"pointer":"/accessToken"},
            "digest":{"pointer":"/accessToken","mode":"sha256"},
            "claim":{"pointer":"/accessToken","mode":"jwt_claim","claim":"subt"},
            "root":{"pointer":""}
        }))
        .unwrap();
        let rows = named_fields(project(&json!({"accessToken":token}), &fields, &policy));
        assert_eq!(rows["value"]["value"], "<redacted:token>");
        assert_eq!(rows["root"]["value"]["accessToken"], "<redacted:token>");
        assert_eq!(rows["digest"]["fingerprint"], fingerprint(token.as_bytes()));
        assert_eq!(rows["claim"]["value"], "registered");
        assert_eq!(rows["claim"]["signature_verified"], false);
    }

    #[test]
    fn preference_xml_decodes_entities_and_rejects_ambiguous_keys() {
        let xml =
            br#"<map><string name="token">{&quot;type&quot;:&quot;Account&quot;}</string></map>"#;
        assert_eq!(preference_json(xml, "token").unwrap()["type"], "Account");
        assert!(preference_json(xml, "missing").is_err());
        assert!(
            preference_json(
                br#"<map><string name="x">{}</string><string name="x">{}</string></map>"#,
                "x"
            )
            .is_err()
        );
        assert!(preference_json(br#"<!DOCTYPE map><map/>"#, "x").is_err());
    }

    #[test]
    fn preference_xml_preserves_references_and_checks_duplicate_attributes() {
        let xml = br#"<map><string name="a&amp;b">{"symbols":"&amp;&lt;&gt;&apos;","quote":"\&quot;","unicode":"&#65;&#x1F680;","mixed":"before<![CDATA[ & literal]]>after"}</string></map>"#;
        assert_eq!(
            preference_json(xml, "a&b").unwrap(),
            json!({"symbols":"&<>'","quote":"\"","unicode":"A🚀","mixed":"before & literalafter"})
        );
        assert!(
            preference_json(
                br#"<map><string name="x">{"value":"&unknown;"}</string></map>"#,
                "x"
            )
            .is_err()
        );

        // Cross the parser's small-attribute threshold and ensure a duplicate
        // near the end is still rejected by its bounded-complexity check.
        let attributes = (0..128).map(|i| format!(" a{i}=\"v\"")).collect::<String>();
        let xml =
            format!("<map><string name=\"x\"{attributes} a127=\"duplicate\">{{}}</string></map>");
        assert!(preference_json(xml.as_bytes(), "x").is_err());
    }
    #[test]
    fn timeline_keeps_checkpoint_links_when_no_field_probes_were_requested() {
        let checkpoint = json!({"type":"evidence_checkpoint","checkpoint_id":"cp-test",
            "started_at_ms":1000,"screen":{"screen_hash":"abc"},"records":[],"partial":true});
        let records = merge_records(&[checkpoint]);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["kind"], "checkpoint");
        assert_eq!(records[0]["screen_hash"], "abc");
        assert_eq!(records[0]["checkpoint_file"], "checkpoint-cp-test.json");
    }

    #[test]
    fn telemetry_uses_event_time_and_deduplicates_repeated_checkpoint_observations() {
        let first = json!({"records":[{"record_id":"late-upload","observed_at_ms":3000,"event_time_ms":1000},
            {"record_id":"storage","observed_at_ms":2000}]});
        let second = json!({"records":[{"record_id":"late-upload","observed_at_ms":3000,"event_time_ms":1000}]});
        let records = merge_records(&[first, second]);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["record_id"], "late-upload");
    }
}
