//! Traffic faults installed in the running ShadowDroid proxy.

use super::args::InjectCmd;
use super::error;
use super::kinds::{Ctx, Injected};
use super::restore::RestoreStep;
use crate::ids::Serial;
use crate::net::fault::{NetFaultEffect, NetFaultSpec};
use anyhow::{Result, bail};
use serde_json::{Value, json};

fn spec_for(id: &str, cmd: &InjectCmd) -> NetFaultSpec {
    let spec = |scope: &super::args::ProxyScope, effect| NetFaultSpec {
        id: id.to_string(),
        host: scope.host.clone().filter(|host| !host.is_empty()),
        path: scope.path.clone().filter(|path| !path.is_empty()),
        percent: scope.percent,
        seed: scope.seed,
        effect,
    };
    match cmd {
        InjectCmd::HttpErrors { scope, status, .. } => {
            spec(scope, NetFaultEffect::ErrorStatus { status: *status })
        }
        InjectCmd::HttpLatency {
            scope,
            delay_ms,
            jitter_ms,
            ..
        } => spec(
            scope,
            NetFaultEffect::Latency {
                delay_ms: *delay_ms,
                jitter_ms: *jitter_ms,
            },
        ),
        InjectCmd::Bandwidth {
            scope,
            bytes_per_sec,
            ..
        } => spec(
            scope,
            NetFaultEffect::Bandwidth {
                bytes_per_sec: *bytes_per_sec,
            },
        ),
        InjectCmd::ConnectionReset {
            scope, after_bytes, ..
        } => spec(
            scope,
            NetFaultEffect::ConnectionReset {
                after_bytes: *after_bytes,
            },
        ),
        InjectCmd::TruncatedResponse {
            scope, keep_bytes, ..
        } => spec(
            scope,
            NetFaultEffect::Truncate {
                keep_bytes: *keep_bytes,
            },
        ),
        InjectCmd::TlsFailure {
            host,
            percent,
            seed,
            ..
        } => NetFaultSpec {
            id: id.to_string(),
            host: Some(host.clone()),
            path: None,
            percent: *percent,
            seed: *seed,
            effect: NetFaultEffect::TlsFailure,
        },
        _ => unreachable!("not a proxy fault"),
    }
}

pub async fn inject(ctx: &mut Ctx<'_>, cmd: &InjectCmd) -> Result<Injected> {
    let serial = Serial::new(ctx.serial);
    if !crate::net::control::is_running(&serial).await {
        return Err(error::requires_proxy(ctx.serial, cmd.kind()));
    }
    let spec = spec_for(ctx.id, cmd);
    spec.validate().map_err(error::invalid_param)?;
    let params = serde_json::to_value(&spec)?;
    let params = json!({
        "host": params["host"],
        "path": params["path"],
        "percent": spec.percent,
        "seed": spec.seed,
        "effect": params["effect"],
    });
    ctx.recorder
        .record(
            &params,
            vec![RestoreStep::ProxyFault {
                id: ctx.id.to_string(),
            }],
        )
        .await?;
    let reply =
        crate::net::control::request(&serial, json!({"op": "fault_add", "spec": spec})).await?;
    if reply["ok"] != true {
        let message = reply["error"].as_str().unwrap_or("").to_string();
        if message.contains("unknown") || message.is_empty() {
            return Err(error::proxy_unsupported(ctx.serial));
        }
        bail!("the proxy refused the fault: {message}");
    }
    let mut result = Injected {
        params,
        observed: json!({"proxy_fault": reply["fault"]}),
        warnings: Vec::new(),
        next_actions: vec![format!("shadowdroid -d {} net log", ctx.serial)],
    };
    result.warnings.push(
        "only traffic the app sends through the ShadowDroid proxy is affected; `net log` lists hit requests with fault_ids"
            .into(),
    );
    Ok(result)
}

/// Remove a proxy fault. A stopped proxy took its faults with it.
pub async fn remove(serial: &str, id: &str) -> Result<()> {
    let serial = Serial::new(serial);
    if !crate::net::control::is_running(&serial).await {
        return Ok(());
    }
    let reply =
        crate::net::control::request(&serial, json!({"op": "fault_remove", "id": id})).await?;
    if reply["ok"] != true {
        bail!(
            "the proxy did not remove fault {id}: {}",
            reply["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(())
}

/// Live hit counts for this device's proxy faults, keyed by fault id.
pub async fn stats(serial: &str) -> Option<Value> {
    let serial = Serial::new(serial);
    if !crate::net::control::is_running(&serial).await {
        return None;
    }
    let reply = crate::net::control::request(&serial, json!({"op": "fault_list"}))
        .await
        .ok()?;
    let mut out = serde_json::Map::new();
    for fault in reply["faults"].as_array()? {
        if let Some(id) = fault["spec"]["id"].as_str() {
            out.insert(
                id.to_string(),
                json!({"matching_requests": fault["matching_requests"], "hits": fault["hits"]}),
            );
        }
    }
    Some(Value::Object(out))
}
