//! Typed, executable effect contracts for the public CLI surface.
//!
//! `agent_metadata::side_effects` remains useful prose. This module is the
//! machine contract: every public leaf must have one exact registry entry, and
//! namespace commands expose the union of their descendants. Effects are
//! conservative upper bounds — an effect may depend on flags/configuration,
//! but an omitted effect is a promise that the command will not perform it.
//! `unbounded_external_command` is the one explicit wildcard: it covers the
//! transitive effects of an external subprocess, not ShadowDroid's own code.

use clap::Command;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CommandEffect {
    HostRead,
    HostWrite,
    DeviceRead,
    DeviceMutate,
    PackageInstall,
    ProcessStart,
    ProcessStop,
    PortMappingMutate,
    NetworkDownload,
    NetworkListen,
    UnboundedExternalCommand,
}

impl CommandEffect {
    const ALL: [Self; 11] = [
        Self::HostRead,
        Self::HostWrite,
        Self::DeviceRead,
        Self::DeviceMutate,
        Self::PackageInstall,
        Self::ProcessStart,
        Self::ProcessStop,
        Self::PortMappingMutate,
        Self::NetworkDownload,
        Self::NetworkListen,
        Self::UnboundedExternalCommand,
    ];

    fn description(self) -> &'static str {
        match self {
            Self::HostRead => "reads host files or persistent host state",
            Self::HostWrite => "creates, changes, or removes host files or persistent host state",
            Self::DeviceRead => {
                "observes an Android device, emulator, or an already-running in-app endpoint"
            }
            Self::DeviceMutate => {
                "changes Android device, emulator, app, debugger, or proxied-traffic state"
            }
            Self::PackageInstall => "installs, replaces, or uninstalls an Android package",
            Self::ProcessStart => {
                "starts a managed long-lived process or boots an emulator; short-lived helper probes are excluded"
            }
            Self::ProcessStop => {
                "stops a managed long-lived process, app, server, recorder, or emulator-owned process"
            }
            Self::PortMappingMutate => {
                "creates, repairs, or removes an adb forward or reverse mapping"
            }
            Self::NetworkDownload => "downloads an external release, update, or dependency asset",
            Self::NetworkListen => {
                "opens a persistent host listener such as the capture proxy or recorder control plane"
            }
            Self::UnboundedExternalCommand => {
                "runs an external subprocess whose transitive effects ShadowDroid cannot bound"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum EffectfulDependency {
    ConfigLoad,
    DeviceInventory,
    TargetResolveMayStart,
    TargetResolveExisting,
    TargetResolveOnline,
    ServerEnsureReady,
    ExistingServerProbe,
    ArtifactWriter,
    ReleaseAssetDownload,
    PackageInstaller,
    ManagedProcessStart,
    ManagedProcessStop,
    PortMappingMutation,
    NetworkListener,
    ExternalCommand,
}

impl EffectfulDependency {
    fn required_effects(self) -> &'static [CommandEffect] {
        use CommandEffect as E;
        match self {
            Self::ConfigLoad => &[E::HostRead],
            Self::DeviceInventory => &[E::DeviceRead],
            // Named target resolution reads/writes ownership and lock state
            // even when it refuses to start a stopped target.
            Self::TargetResolveExisting | Self::TargetResolveOnline => {
                &[E::HostWrite, E::DeviceRead]
            }
            // The probe reads the persisted port slot before observing the
            // already-running endpoint. Its implementation is source-guarded
            // below so it cannot silently grow lifecycle behavior.
            Self::ExistingServerProbe => &[E::HostRead, E::DeviceRead],
            Self::TargetResolveMayStart => &[
                E::HostWrite,
                E::DeviceRead,
                E::DeviceMutate,
                E::ProcessStart,
            ],
            // Missing/mismatched release APKs may be downloaded and cached;
            // bring-up may replace packages, stop a stale server, start
            // instrumentation, and establish a forward.
            Self::ServerEnsureReady => &[
                E::HostRead,
                E::HostWrite,
                E::DeviceRead,
                E::DeviceMutate,
                E::PackageInstall,
                E::ProcessStart,
                E::ProcessStop,
                E::PortMappingMutate,
                E::NetworkDownload,
            ],
            Self::ArtifactWriter => &[E::HostWrite],
            Self::ReleaseAssetDownload => &[E::HostWrite, E::NetworkDownload],
            Self::PackageInstaller => &[
                E::DeviceRead,
                E::DeviceMutate,
                E::PackageInstall,
                E::ProcessStart,
                E::ProcessStop,
            ],
            Self::ManagedProcessStart => &[E::ProcessStart],
            Self::ManagedProcessStop => &[E::ProcessStop],
            Self::PortMappingMutation => &[E::HostWrite, E::DeviceMutate, E::PortMappingMutate],
            Self::NetworkListener => &[E::ProcessStart, E::NetworkListen],
            Self::ExternalCommand => &[E::UnboundedExternalCommand],
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct LeafEffectContract {
    effects: &'static [CommandEffect],
    dependencies: &'static [EffectfulDependency],
}

const fn leaf(
    effects: &'static [CommandEffect],
    dependencies: &'static [EffectfulDependency],
) -> LeafEffectContract {
    LeafEffectContract {
        effects,
        dependencies,
    }
}

use CommandEffect as E;
use EffectfulDependency as D;

const HOST_READ: &[E] = &[E::HostRead];
const HOST_WRITE: &[E] = &[E::HostRead, E::HostWrite];
const HOST_DOWNLOAD: &[E] = &[E::HostRead, E::HostWrite, E::NetworkDownload];
const UPDATE_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::NetworkDownload,
    E::UnboundedExternalCommand,
];
const DEVICE_INVENTORY: &[E] = &[E::HostRead, E::DeviceRead];
const TARGET_READ: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStart,
];
const TARGET_MUTATE: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStart,
];
const TARGET_WRITE: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStart,
];
const TARGET_PORT_READ: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStart,
    E::PortMappingMutate,
];
const TARGET_PORT_WRITE: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStart,
    E::PortMappingMutate,
];
const EXISTING_READ: &[E] = &[E::HostRead, E::HostWrite, E::DeviceRead];
const COLLECT_EFFECTS: &[E] = &[E::HostRead, E::HostWrite, E::DeviceRead];
const SERVER_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::PackageInstall,
    E::ProcessStart,
    E::ProcessStop,
    E::PortMappingMutate,
    E::NetworkDownload,
];
const INSTALL_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::PackageInstall,
    E::ProcessStart,
    E::ProcessStop,
];
const DISCONNECT_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStop,
    E::PortMappingMutate,
];
const TEST_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::PackageInstall,
    E::ProcessStart,
    E::ProcessStop,
    E::PortMappingMutate,
    E::NetworkDownload,
    E::UnboundedExternalCommand,
];
const VIDEO_START_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStart,
    E::ProcessStop,
    E::NetworkListen,
];
const VIDEO_STOP_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStop,
];
const NET_START_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStart,
    E::ProcessStop,
    E::PortMappingMutate,
    E::NetworkListen,
];
const NET_STOP_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStop,
    E::PortMappingMutate,
];
const DEBUGGER_READ: &[E] = &[E::HostRead, E::DeviceRead];
const DEBUGGER_MUTATE: &[E] = &[E::HostRead, E::DeviceRead, E::DeviceMutate];
const DEBUGGER_PERSISTENT_MUTATE: &[E] =
    &[E::HostRead, E::HostWrite, E::DeviceRead, E::DeviceMutate];
const APP_STATE_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::DeviceRead,
    E::DeviceMutate,
    E::ProcessStart,
    E::ProcessStop,
];
const AAR_INSTALL_EFFECTS: &[E] = &[
    E::HostRead,
    E::HostWrite,
    E::NetworkDownload,
    E::UnboundedExternalCommand,
];
const NET_EXISTING_MUTATE: &[E] = &[E::HostRead, E::HostWrite, E::DeviceRead, E::DeviceMutate];

const CONFIG: &[D] = &[D::ConfigLoad];
const INVENTORY: &[D] = &[D::ConfigLoad, D::DeviceInventory];
const TARGET: &[D] = &[D::ConfigLoad, D::TargetResolveMayStart];
const TARGET_PORT: &[D] = &[
    D::ConfigLoad,
    D::TargetResolveMayStart,
    D::PortMappingMutation,
];
const EXISTING: &[D] = &[D::ConfigLoad, D::TargetResolveExisting];
const SERVER: &[D] = &[
    D::ConfigLoad,
    D::TargetResolveMayStart,
    D::ServerEnsureReady,
];

/// Exact registry for executable public leaves. Deliberately no prefix/default
/// arm: adding a Clap command without choosing a contract fails the exhaustive
/// catalog test.
fn leaf_contract(path: &str) -> Option<LeafEffectContract> {
    Some(match path {
        // Introspection/recovery commands dispatched before normal config load.
        "commands" => leaf(HOST_READ, &[]),
        "usage status" | "config paths" | "config schema" | "config explain"
        | "config validate" => leaf(HOST_READ, &[]),
        // Reporting serializes with writers by opening the usage lock file.
        "usage report" => leaf(HOST_WRITE, &[D::ArtifactWriter]),
        "usage enable" | "usage disable" | "usage clear" | "config init" | "skill" => {
            leaf(HOST_WRITE, &[D::ArtifactWriter])
        }
        "update" => leaf(
            UPDATE_EFFECTS,
            &[D::ReleaseAssetDownload, D::ExternalCommand],
        ),

        // Top-level lifecycle and diagnostics.
        "devices" => leaf(DEVICE_INVENTORY, INVENTORY),
        "connect" | "doctor" => leaf(SERVER_EFFECTS, SERVER),
        "disconnect" => leaf(
            DISCONNECT_EFFECTS,
            &[
                D::ConfigLoad,
                D::TargetResolveExisting,
                D::ManagedProcessStop,
                D::PortMappingMutation,
            ],
        ),
        "test" => leaf(
            TEST_EFFECTS,
            &[
                D::ConfigLoad,
                D::TargetResolveMayStart,
                D::ServerEnsureReady,
                D::ManagedProcessStop,
                D::ExternalCommand,
            ],
        ),
        "init" | "studio install" => leaf(
            HOST_DOWNLOAD,
            &[D::ConfigLoad, D::ArtifactWriter, D::ReleaseAssetDownload],
        ),
        "studio status" => leaf(HOST_READ, CONFIG),
        "collect" => leaf(
            COLLECT_EFFECTS,
            &[
                D::ConfigLoad,
                D::TargetResolveOnline,
                D::ExistingServerProbe,
                D::ArtifactWriter,
            ],
        ),
        "log" => leaf(TARGET_READ, TARGET),
        "why" => leaf(
            TARGET_READ,
            &[
                D::ConfigLoad,
                D::TargetResolveMayStart,
                D::ExistingServerProbe,
            ],
        ),

        // Android Studio debugger bridge commands do not bring up ShadowDroid's
        // device server. They still observe or change the debugged process.
        "debug status"
        | "debug sessions"
        | "debug clients"
        | "debug logpoint list"
        | "debug logpoint events"
        | "debug logpoint follow"
        | "debug breakpoints"
        | "debug stack"
        | "debug threads"
        | "debug variables"
        | "debug inspect"
        | "debug coroutines snapshot"
        | "debug coroutines threads"
        | "debug coroutines continuation"
        | "debug coroutines flow"
        | "debug watch list" => leaf(DEBUGGER_READ, CONFIG),
        "debug attach" => leaf(
            &[E::HostRead, E::DeviceRead, E::DeviceMutate, E::ProcessStart],
            &[D::ConfigLoad, D::ManagedProcessStart],
        ),
        "debug stop" => leaf(
            &[E::HostRead, E::DeviceRead, E::DeviceMutate, E::ProcessStop],
            &[D::ConfigLoad, D::ManagedProcessStop],
        ),
        "debug break line"
        | "debug break exception"
        | "debug break method"
        | "debug break field"
        | "debug break update"
        | "debug break remove"
        | "debug logpoint add"
        | "debug logpoint remove"
        | "debug logpoint clear" => leaf(DEBUGGER_PERSISTENT_MUTATE, CONFIG),
        "debug pause"
        | "debug resume"
        | "debug step-in"
        | "debug step-over"
        | "debug step-out"
        | "debug eval"
        | "debug continue-until"
        | "debug watch add"
        | "debug watch remove"
        | "debug watch clear" => leaf(DEBUGGER_MUTATE, CONFIG),

        // Debug workflows below share the on-device server bring-up.
        "debug auto"
        | "debug snapshot"
        | "debug record"
        | "debug replay"
        | "debug step-until-screen-change"
        | "debug step-until-log"
        | "debug run-until-crash"
        | "debug native status"
        | "debug tombstones list"
        | "debug tombstones pull" => leaf(SERVER_EFFECTS, SERVER),

        // Host-side screen recording has its own persistent process lifecycle.
        "video record" | "video start" => leaf(
            VIDEO_START_EFFECTS,
            &[
                D::ConfigLoad,
                D::TargetResolveMayStart,
                D::ArtifactWriter,
                D::ManagedProcessStart,
                D::ManagedProcessStop,
                D::NetworkListener,
            ],
        ),
        "video status" => leaf(EXISTING_READ, EXISTING),
        "video mark" => leaf(
            &[E::HostRead, E::HostWrite, E::DeviceRead],
            &[D::ConfigLoad, D::TargetResolveExisting, D::ArtifactWriter],
        ),
        "video stop" => leaf(
            VIDEO_STOP_EFFECTS,
            &[
                D::ConfigLoad,
                D::TargetResolveExisting,
                D::ArtifactWriter,
                D::ManagedProcessStop,
            ],
        ),
        "watch" => leaf(SERVER_EFFECTS, SERVER),

        // App lifecycle. The read-looking leaves still inherit server bring-up.
        "app start" | "app stop" | "app clear" | "app info" | "app wait" | "app current" => {
            leaf(SERVER_EFFECTS, SERVER)
        }
        "app install" | "app reinstall" => leaf(
            INSTALL_EFFECTS,
            &[D::ConfigLoad, D::TargetResolveMayStart, D::PackageInstaller],
        ),
        "app state snapshot" | "app state restore" | "app state recover" => leaf(
            APP_STATE_EFFECTS,
            &[
                D::ConfigLoad,
                D::TargetResolveMayStart,
                D::ArtifactWriter,
                D::ManagedProcessStop,
            ],
        ),
        "app state cleanup" => leaf(HOST_WRITE, &[D::ConfigLoad, D::ArtifactWriter]),

        // Pure-adb state commands may boot an explicitly configured target.
        "perm grant" | "perm revoke" | "perm reset" | "appops set" | "profile apply"
        | "profile reset" => leaf(TARGET_MUTATE, TARGET),
        "perm list" | "appops get" => leaf(TARGET_READ, TARGET),
        "profile snapshot" => leaf(TARGET_WRITE, TARGET),

        // These namespaces currently share server bring-up, even for reads.
        "device info"
        | "device wake"
        | "device sleep"
        | "device unlock"
        | "device orientation"
        | "device clipboard"
        | "device notifications"
        | "device quick-settings"
        | "device open-url"
        | "files ls"
        | "files push"
        | "files pull" => leaf(SERVER_EFFECTS, SERVER),
        "device shell" => leaf(
            &[
                E::HostRead,
                E::HostWrite,
                E::DeviceRead,
                E::DeviceMutate,
                E::PackageInstall,
                E::ProcessStart,
                E::ProcessStop,
                E::PortMappingMutate,
                E::NetworkDownload,
                E::UnboundedExternalCommand,
            ],
            &[
                D::ConfigLoad,
                D::TargetResolveMayStart,
                D::ServerEnsureReady,
                D::ExternalCommand,
            ],
        ),

        // Host proxy / network capture control.
        "net check" | "net trust" => leaf(TARGET_WRITE, TARGET),
        "net ca info" => leaf(EXISTING_READ, EXISTING),
        "net ca import" | "net ca reset" => leaf(
            &[E::HostRead, E::HostWrite, E::DeviceRead],
            &[D::ConfigLoad, D::TargetResolveExisting, D::ArtifactWriter],
        ),
        "net start" => leaf(
            NET_START_EFFECTS,
            &[
                D::ConfigLoad,
                D::TargetResolveMayStart,
                D::ArtifactWriter,
                D::ManagedProcessStart,
                D::ManagedProcessStop,
                D::PortMappingMutation,
                D::NetworkListener,
            ],
        ),
        "net stop" => leaf(
            NET_STOP_EFFECTS,
            &[
                D::ConfigLoad,
                D::TargetResolveExisting,
                D::ArtifactWriter,
                D::ManagedProcessStop,
                D::PortMappingMutation,
            ],
        ),
        "net status" => leaf(EXISTING_READ, EXISTING),
        // `net log --action clear` changes the active capture's query boundary.
        "net log" => leaf(
            &[E::HostRead, E::HostWrite, E::DeviceRead, E::DeviceMutate],
            &[D::ConfigLoad, D::TargetResolveExisting, D::ArtifactWriter],
        ),
        "net checkpoint" | "net show" | "net export" => leaf(
            &[E::HostRead, E::HostWrite, E::DeviceRead],
            &[D::ConfigLoad, D::TargetResolveExisting, D::ArtifactWriter],
        ),
        "net ws" | "net rule list" => leaf(EXISTING_READ, EXISTING),
        "net rule lint" | "net rule explain" => leaf(HOST_READ, CONFIG),
        "net inject" | "net intercept" | "net resume" | "net drop" | "net respond"
        | "net rule add" | "net rule rm" | "net rule clear" => leaf(NET_EXISTING_MUTATE, EXISTING),
        "net override" | "net rules" | "net replay" => leaf(NET_EXISTING_MUTATE, EXISTING),

        // Every UI/layout leaf currently uses the shared server bring-up.
        "ui dump"
        | "ui audit"
        | "ui gen"
        | "ui screenshot"
        | "ui find"
        | "ui tap"
        | "ui set-progress"
        | "ui double-tap"
        | "ui long-tap"
        | "ui swipe"
        | "ui drag"
        | "ui swipe-ext"
        | "ui pinch"
        | "ui scroll-to"
        | "ui focus"
        | "ui text"
        | "ui pin"
        | "ui key"
        | "ui hide-keyboard"
        | "ui back"
        | "ui home"
        | "ui wait"
        | "ui toast"
        | "layout snapshot"
        | "layout diff"
        | "layout recompositions"
        | "layout source" => leaf(SERVER_EFFECTS, SERVER),

        // In-app AAR management is host-only; runtime agent verbs use adb but
        // do not install/start the ShadowDroid UiAutomation server.
        "aar status" => leaf(HOST_READ, CONFIG),
        "aar install" => leaf(
            AAR_INSTALL_EFFECTS,
            &[
                D::ConfigLoad,
                D::ArtifactWriter,
                D::ReleaseAssetDownload,
                D::ExternalCommand,
            ],
        ),
        "aar remove" => leaf(HOST_WRITE, &[D::ConfigLoad, D::ArtifactWriter]),
        "aar capture" | "aar coroutines" => leaf(TARGET_PORT_WRITE, TARGET_PORT),
        "aar agent" => leaf(TARGET_PORT_READ, TARGET_PORT),
        "aar intercept" | "aar resume" | "aar drop" => leaf(TARGET_PORT_READ, TARGET_PORT),

        _ => return None,
    })
}

fn visible_children(command: &Command) -> impl Iterator<Item = &Command> {
    command
        .get_subcommands()
        .filter(|child| child.get_name() != "help" && !child.is_hide_set())
}

#[derive(Default)]
struct ResolvedContract {
    effects: BTreeSet<CommandEffect>,
    dependencies: BTreeSet<EffectfulDependency>,
}

fn resolve(command: &Command, path: &[String]) -> Option<ResolvedContract> {
    let children = visible_children(command).collect::<Vec<_>>();
    if children.is_empty() {
        let contract = leaf_contract(&path.join(" "))?;
        let effects = contract.effects.iter().copied().collect::<BTreeSet<_>>();
        // Refuse to publish a contract whose implementation dependency needs
        // an effect the command did not declare. The exhaustive unit test gives
        // a detailed failure; this runtime check keeps even a test-skipping
        // build from advertising a mechanically false promise.
        if contract.dependencies.iter().any(|dependency| {
            dependency
                .required_effects()
                .iter()
                .any(|effect| !effects.contains(effect))
        }) {
            return None;
        }
        return Some(ResolvedContract {
            effects,
            dependencies: contract.dependencies.iter().copied().collect(),
        });
    }

    let mut resolved = ResolvedContract::default();
    for child in children {
        let mut child_path = path.to_vec();
        child_path.push(child.get_name().to_string());
        let child_contract = resolve(child, &child_path)?;
        resolved.effects.extend(child_contract.effects);
        resolved.dependencies.extend(child_contract.dependencies);
    }
    Some(resolved)
}

pub(super) fn command_effect_contract(
    command: &Command,
    path: &[String],
) -> Option<serde_json::Value> {
    let namespace = visible_children(command).next().is_some();
    let resolved = resolve(command, path)?;
    let has_unbounded_subprocess = resolved
        .effects
        .contains(&CommandEffect::UnboundedExternalCommand);
    Some(serde_json::json!({
        "schema_version": 1,
        "mode": "conservative_upper_bound",
        "scope": if namespace { "namespace_descendant_union" } else { "invocation" },
        "effects": resolved.effects,
        "effect_coverage": {
            "kind": if has_unbounded_subprocess {
                "enumerated_plus_unbounded_external_subprocess"
            } else {
                "enumerated"
            },
            "wildcard_effects": if has_unbounded_subprocess {
                vec![CommandEffect::UnboundedExternalCommand]
            } else {
                Vec::new()
            },
        },
        "conditional_effects": usage_log_conditional_effects(),
        "effectful_dependencies": resolved.dependencies,
    }))
}

fn usage_log_conditional_effects() -> serde_json::Value {
    serde_json::json!([{
        "effect": "host_write",
        "when": "usage logging is explicitly enabled",
        "reason": "the cross-cutting usage recorder appends one local metadata line after the invocation"
    }])
}

pub(super) fn effect_model_json() -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "mode": "conservative_upper_bound",
        "semantics": "listed effects may occur depending on flags and configuration; an effect omitted from both effects and conditional_effects is forbidden for ShadowDroid's implementation, except for transitive subprocess effects when unbounded_external_command is listed",
        "namespace_scope": "namespace commands expose the union of every public executable descendant",
        "effects": CommandEffect::ALL.into_iter().map(|effect| serde_json::json!({
            "name": serde_json::to_value(effect).expect("serialize command effect"),
            "description": effect.description(),
            "wildcard": effect == CommandEffect::UnboundedExternalCommand,
        })).collect::<Vec<_>>(),
        "wildcard_semantics": {
            "effect": CommandEffect::UnboundedExternalCommand,
            "scope": "transitive_external_subprocess_effects",
            "does_not_cover": "side effects performed directly by ShadowDroid",
        },
        "global_conditional_effects": usage_log_conditional_effects(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn visit_public_leaves(command: &Command, path: &mut Vec<String>, leaves: &mut Vec<String>) {
        let children = visible_children(command).collect::<Vec<_>>();
        if children.is_empty() {
            if !path.is_empty() {
                leaves.push(path.join(" "));
            }
            return;
        }
        for child in children {
            path.push(child.get_name().to_string());
            visit_public_leaves(child, path, leaves);
            path.pop();
        }
    }

    fn source_region<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        let start = source.find(start).expect("guarded region start");
        let tail = &source[start..];
        let end = tail.find(end).expect("guarded region end");
        &tail[..end]
    }

    fn compact_source(source: &str) -> String {
        source
            .chars()
            .filter(|char| !char.is_whitespace())
            .collect()
    }

    #[test]
    fn every_public_leaf_has_an_exact_effect_contract() {
        let root = crate::cli::Cli::command();
        let mut leaves = Vec::new();
        visit_public_leaves(&root, &mut Vec::new(), &mut leaves);
        let missing = leaves
            .iter()
            .filter(|path| leaf_contract(path).is_none())
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "public leaves without effects: {missing:?}"
        );
    }

    #[test]
    fn every_public_leaf_declares_the_global_usage_config_read() {
        let root = crate::cli::Cli::command();
        let mut leaves = Vec::new();
        visit_public_leaves(&root, &mut Vec::new(), &mut leaves);
        let missing = leaves
            .iter()
            .filter(|path| {
                !leaf_contract(path)
                    .expect("effect contract")
                    .effects
                    .contains(&E::HostRead)
            })
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "commands missing the usage recorder's unconditional host config read: {missing:?}"
        );
    }

    #[test]
    fn declared_effects_cover_every_effectful_dependency() {
        let root = crate::cli::Cli::command();
        let mut leaves = Vec::new();
        visit_public_leaves(&root, &mut Vec::new(), &mut leaves);
        let mut violations = Vec::new();
        for path in leaves {
            let contract = leaf_contract(&path).unwrap();
            let declared = contract.effects.iter().copied().collect::<BTreeSet<_>>();
            for dependency in contract.dependencies {
                let missing = dependency
                    .required_effects()
                    .iter()
                    .filter(|effect| !declared.contains(effect))
                    .copied()
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    violations.push(format!("{path}: {dependency:?} needs {missing:?}"));
                }
            }
        }
        assert!(
            violations.is_empty(),
            "effectful dependencies exceed command declarations:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn validator_would_reject_an_undeclared_effectful_dependency() {
        let declared = [E::DeviceRead].into_iter().collect::<BTreeSet<_>>();
        let missing = D::ServerEnsureReady
            .required_effects()
            .iter()
            .filter(|effect| !declared.contains(effect))
            .collect::<Vec<_>>();
        assert!(missing.contains(&&E::PackageInstall));
        assert!(missing.contains(&&E::ProcessStart));
        assert!(missing.contains(&&E::PortMappingMutate));
    }

    #[test]
    fn reviewed_runtime_paths_keep_their_non_obvious_effects() {
        fn has_effect(path: &str, effect: E) -> bool {
            leaf_contract(path).unwrap().effects.contains(&effect)
        }
        fn has_dependency(path: &str, dependency: D) -> bool {
            leaf_contract(path)
                .unwrap()
                .dependencies
                .contains(&dependency)
        }

        // Even recovery/introspection dispatches pass through the global usage
        // recorder, which always reads the opt-in user configuration.
        assert!(has_effect("commands", E::HostRead));
        assert!(has_effect("usage report", E::HostWrite));
        assert!(has_dependency("usage report", D::ArtifactWriter));

        for path in [
            "net log",
            "net checkpoint",
            "net show",
            "net export",
            "net ws",
            "net inject",
            "net intercept",
            "net resume",
            "net drop",
            "net respond",
            "net rule add",
            "net rule list",
            "net rule rm",
            "net rule clear",
            "net override",
            "net rules",
            "net replay",
        ] {
            assert!(has_effect(path, E::DeviceRead), "{path}");
            assert!(has_effect(path, E::HostWrite), "{path}");
            assert!(has_dependency(path, D::TargetResolveExisting), "{path}");
        }
        for path in ["net ca import", "net ca info", "net ca reset"] {
            assert!(has_effect(path, E::DeviceRead), "{path}");
            assert!(has_dependency(path, D::TargetResolveExisting), "{path}");
        }
        for path in ["net check", "net trust", "disconnect"] {
            assert!(has_effect(path, E::HostWrite), "{path}");
        }
        for path in ["video record", "video start", "net start"] {
            assert!(has_effect(path, E::ProcessStop), "{path}");
            assert!(has_dependency(path, D::ManagedProcessStop), "{path}");
        }
        for path in ["video record", "video start"] {
            assert!(has_effect(path, E::NetworkListen), "{path}");
            assert!(has_dependency(path, D::NetworkListener), "{path}");
        }
        assert!(has_effect("net log", E::DeviceMutate));
        for path in [
            "debug break line",
            "debug break exception",
            "debug break method",
            "debug break field",
            "debug break update",
            "debug break remove",
            "debug logpoint add",
            "debug logpoint remove",
            "debug logpoint clear",
        ] {
            assert!(has_effect(path, E::HostWrite), "{path}");
        }
        assert!(has_effect("debug attach", E::ProcessStart));
        assert!(has_dependency("debug attach", D::ManagedProcessStart));
        assert!(has_effect("debug stop", E::ProcessStop));
        assert!(has_dependency("debug stop", D::ManagedProcessStop));
        for path in [
            "app state snapshot",
            "app state restore",
            "app state recover",
        ] {
            assert!(has_effect(path, E::ProcessStop), "{path}");
            assert!(has_dependency(path, D::ManagedProcessStop), "{path}");
        }
        assert!(has_effect("aar install", E::UnboundedExternalCommand));
        assert!(has_dependency("aar install", D::ExternalCommand));
    }

    #[test]
    fn stateful_effect_contracts_stay_source_linked() {
        let usage_source = include_str!("../usage.rs");
        let usage_report = source_region(
            usage_source,
            "fn report(days: u32)",
            "fn build_recommendations(",
        );
        assert!(usage_report.contains("acquire_usage_lock(&path)?"));
        assert!(
            leaf_contract("usage report")
                .unwrap()
                .effects
                .contains(&E::HostWrite)
        );

        let video_daemon = include_str!("../../video/daemon.rs");
        assert!(video_daemon.contains("TcpListener::bind((\"127.0.0.1\", 0u16))"));
        assert!(video_daemon.contains("listener.accept()"));
        for path in ["video record", "video start"] {
            let contract = leaf_contract(path).unwrap();
            assert!(contract.effects.contains(&E::NetworkListen), "{path}");
            assert!(
                contract.dependencies.contains(&D::NetworkListener),
                "{path}"
            );
        }

        let cli_source = include_str!("../../cli.rs");
        let net_log_dispatch = source_region(
            cli_source,
            "        NetCmd::Log {",
            "        NetCmd::Checkpoint",
        );
        assert!(net_log_dispatch.contains("nc::log_clear(serial).await"));
        let control_source = include_str!("../../net/control.rs");
        assert!(control_source.contains("        \"log_clear\" => {"));
        assert!(
            leaf_contract("net log")
                .unwrap()
                .effects
                .contains(&E::DeviceMutate)
        );

        let breakpoint_source = include_str!(
            "../../../../shadowdroid-plugin/src/main/kotlin/io/github/andriyo/shadowdroid/studio/BreakpointBridge.kt"
        );
        assert!(breakpoint_source.contains("persisted breakpoint"));
        assert!(breakpoint_source.contains(".addLineBreakpoint("));
        assert!(breakpoint_source.contains(".removeBreakpoint("));
        for path in [
            "debug break line",
            "debug break exception",
            "debug break method",
            "debug break field",
            "debug break update",
            "debug break remove",
            "debug logpoint add",
            "debug logpoint remove",
            "debug logpoint clear",
        ] {
            assert!(
                leaf_contract(path).unwrap().effects.contains(&E::HostWrite),
                "{path}"
            );
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ResolverPolicy {
        None,
        MayStart,
        Existing,
        Online,
    }

    fn declared_resolver_policy(path: &str) -> ResolverPolicy {
        let contract = leaf_contract(path).expect("effect contract");
        let policies = contract
            .dependencies
            .iter()
            .filter_map(|dependency| match dependency {
                D::TargetResolveMayStart => Some(ResolverPolicy::MayStart),
                D::TargetResolveExisting => Some(ResolverPolicy::Existing),
                D::TargetResolveOnline => Some(ResolverPolicy::Online),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            policies.len() <= 1,
            "{path} declares conflicting target resolver dependencies: {policies:?}"
        );
        policies.first().copied().unwrap_or(ResolverPolicy::None)
    }

    /// Independent dispatch review table. Most commands fall through phase 2
    /// and may start a configured target; every exception must be named here.
    /// This intentionally does not derive from `leaf_contract`, so changing a
    /// dispatch classifier without its effect dependency fails mechanically.
    fn expected_resolver_policy(path: &str) -> ResolverPolicy {
        match path {
            "collect" => ResolverPolicy::Online,
            "disconnect" | "video status" | "video mark" | "video stop" | "net ca import"
            | "net ca info" | "net ca reset" | "net stop" | "net status" | "net log"
            | "net checkpoint" | "net show" | "net export" | "net ws" | "net inject"
            | "net intercept" | "net resume" | "net drop" | "net respond" | "net rule add"
            | "net rule list" | "net rule rm" | "net rule clear" | "net override" | "net rules"
            | "net replay" => ResolverPolicy::Existing,
            "devices"
            | "update"
            | "init"
            | "commands"
            | "usage status"
            | "usage enable"
            | "usage disable"
            | "usage report"
            | "usage clear"
            | "config init"
            | "config paths"
            | "config schema"
            | "config explain"
            | "config validate"
            | "skill"
            | "studio status"
            | "studio install"
            | "debug status"
            | "debug sessions"
            | "debug clients"
            | "debug attach"
            | "debug break line"
            | "debug break exception"
            | "debug break method"
            | "debug break field"
            | "debug break update"
            | "debug break remove"
            | "debug logpoint add"
            | "debug logpoint list"
            | "debug logpoint events"
            | "debug logpoint follow"
            | "debug logpoint remove"
            | "debug logpoint clear"
            | "debug breakpoints"
            | "debug pause"
            | "debug resume"
            | "debug step-in"
            | "debug step-over"
            | "debug step-out"
            | "debug stop"
            | "debug stack"
            | "debug threads"
            | "debug variables"
            | "debug eval"
            | "debug inspect"
            | "debug coroutines snapshot"
            | "debug coroutines threads"
            | "debug coroutines continuation"
            | "debug coroutines flow"
            | "debug continue-until"
            | "debug watch add"
            | "debug watch list"
            | "debug watch remove"
            | "debug watch clear"
            | "app state cleanup"
            | "net rule lint"
            | "net rule explain"
            | "aar install"
            | "aar status"
            | "aar remove" => ResolverPolicy::None,
            _ => ResolverPolicy::MayStart,
        }
    }

    #[test]
    fn target_resolver_dispatch_is_source_linked_to_every_leaf_contract() {
        let root = crate::cli::Cli::command();
        let mut leaves = Vec::new();
        visit_public_leaves(&root, &mut Vec::new(), &mut leaves);
        let mismatches = leaves
            .iter()
            .filter_map(|path| {
                let expected = expected_resolver_policy(path);
                let declared = declared_resolver_policy(path);
                (expected != declared)
                    .then(|| format!("{path}: dispatch={expected:?}, contract={declared:?}"))
            })
            .collect::<Vec<_>>();
        assert!(
            mismatches.is_empty(),
            "target resolver routing and effect dependencies disagree:\n{}",
            mismatches.join("\n")
        );

        let cli_source = include_str!("../../cli.rs");
        for (call, expected_count) in [
            ("selection.resolve(&config)", 15),
            ("selection.resolve_existing(&config)", 3),
            ("selection.resolve_online(&config)", 1),
        ] {
            assert_eq!(
                cli_source.matches(call).count(),
                expected_count,
                "target resolver call inventory changed at {call}; review every affected public leaf contract"
            );
        }

        // The two shared conditional routers are the highest-leverage seams:
        // adding `Ws` to NetCmd::allows_target_start, for example, would make
        // `net ws` boot an emulator and immediately fail this exact guard.
        let net_impl = compact_source(source_region(
            cli_source,
            "impl NetCmd {",
            "#[derive(Subcommand)]\npub enum NetCaCmd",
        ));
        assert!(net_impl.contains(
            "fnallows_target_start(&self)->bool{matches!(self,Self::Check{..}|Self::Trust{..}|Self::Start{..})}"
        ));
        assert!(!net_impl.contains("Self::Ws"));

        let net_dispatch = compact_source(source_region(
            cli_source,
            "        Cmd::Net(c) => {",
            "        _ => {}",
        ));
        assert!(net_dispatch.contains(
            "letserial=ifc.allows_target_start(){selection.resolve(&config).await?}else{selection.resolve_existing(&config).await?};"
        ));
        assert!(net_dispatch.contains("ifmatches!(c,NetCmd::Ca(_)){"));
        assert!(net_dispatch.contains(
            "letserial=resolve_serial(direct).await.unwrap_or_else(|_|Serial::new(\"\"));"
        ));

        let video_dispatch = compact_source(source_region(
            cli_source,
            "        Cmd::Video(args) => {",
            "        Cmd::Perm(c) => {",
        ));
        assert!(video_dispatch.contains(
            "letserial=ifargs.allows_target_start(){selection.resolve(&config).await?}else{selection.resolve_existing(&config).await?};"
        ));
        let video_source = compact_source(include_str!("../../video/mod.rs"));
        assert!(video_source.contains(
            "pubfnallows_target_start(&self)->bool{matches!(self.command,VideoCmd::Start(_)|VideoCmd::Record(_))}"
        ));

        let aar_source = compact_source(include_str!("../aar.rs"));
        assert!(aar_source.contains(
            "pub(crate)fnrequires_device(&self)->bool{!matches!(self,Self::Install(_)|Self::Status(_)|Self::Remove(_))}"
        ));
        let debug_source = compact_source(include_str!("../debug.rs"));
        assert!(
            debug_source
                .contains("pubfnis_host_only(&self)->bool{matches!(self.cmd,DebugCmd::Studio(_))}")
        );
        let app_state_source = compact_source(include_str!("../app_state.rs"));
        assert!(
            app_state_source.contains(
                "pubfnneeds_device(&self)->bool{!matches!(self.cmd,StateCmd::Cleanup{..})}"
            )
        );

        let selection_impl = compact_source(source_region(
            cli_source,
            "impl DeviceSelection {",
            "/// Entry point: run the command",
        ));
        assert!(
            selection_impl
                .contains("asyncfnresolve(&self,config:&ShadowDroidConfig)->Result<Serial>")
        );
        assert!(
            selection_impl.contains("crate::device::target::resolve(config,target,self.takeover)")
        );
        assert!(
            selection_impl.contains(
                "asyncfnresolve_existing(&self,config:&ShadowDroidConfig)->Result<Serial>"
            )
        );
        assert!(
            selection_impl
                .contains("crate::device::target::resolve_existing(config,target,self.takeover)")
        );
        assert!(
            selection_impl
                .contains("asyncfnresolve_online(&self,config:&ShadowDroidConfig)->Result<Serial>")
        );

        assert_eq!(declared_resolver_policy("net ws"), ResolverPolicy::Existing);
        assert_eq!(declared_resolver_policy("collect"), ResolverPolicy::Online);
    }

    #[test]
    fn collect_is_structurally_limited_to_passive_lifecycle_dependencies() {
        let contract = leaf_contract("collect").unwrap();
        let effects = contract.effects.iter().copied().collect::<BTreeSet<_>>();
        for forbidden in [
            E::DeviceMutate,
            E::PackageInstall,
            E::ProcessStart,
            E::ProcessStop,
            E::PortMappingMutate,
            E::NetworkDownload,
            E::NetworkListen,
            E::UnboundedExternalCommand,
        ] {
            assert!(
                !effects.contains(&forbidden),
                "collect declares {forbidden:?}"
            );
        }
        assert_eq!(
            contract.dependencies,
            &[
                D::ConfigLoad,
                D::TargetResolveOnline,
                D::ExistingServerProbe,
                D::ArtifactWriter,
            ]
        );

        // Guard the two places where collect can acquire lifecycle behavior.
        // This complements the typed table: a future direct dependency bypass
        // cannot silently make the implementation stronger than its contract.
        let collect_source = include_str!("../collect.rs");
        for forbidden_call in [
            "installer::ensure_ready(",
            "installer::ensure_ready_for_command(",
            "adb::forward(",
            "adb::reverse(",
            "Command::new(",
        ] {
            assert!(
                !collect_source.contains(forbidden_call),
                "collect implementation gained forbidden dependency {forbidden_call}"
            );
        }
        assert!(collect_source.contains("installer::probe_existing("));

        // Guard the complete low-level read chain as well as collect's direct
        // caller. Otherwise `probe_existing` could acquire lifecycle behavior
        // internally while collect.rs continued to look passive.
        fn region<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
            let start = source.find(start).expect("guarded region start");
            let tail = &source[start..];
            let end = tail.find(end).expect("guarded region end");
            &tail[..end]
        }
        let installer_source = include_str!("../../device/installer.rs");
        let probe_existing = region(
            installer_source,
            "pub async fn probe_existing(",
            "/// Make sure the device has our server running",
        );
        assert!(probe_existing.contains("portmap::peek("));
        assert!(probe_existing.contains("probe(&client"));
        let probe = region(
            installer_source,
            "async fn probe(client:",
            "async fn wait_for_server(",
        );
        assert!(probe.contains("client.state().await"));

        let portmap_source = include_str!("../../device/portmap.rs");
        let peek = region(
            portmap_source,
            "pub fn peek(",
            "/// Allocate and publish an ADB forward",
        );
        assert!(peek.contains("std::fs::read_to_string("));

        for (name, source) in [
            ("installer::probe_existing", probe_existing),
            ("installer::probe", probe),
            ("portmap::peek", peek),
        ] {
            for forbidden_call in [
                "ensure_forward(",
                "publish_forward(",
                "acquire_lifecycle_lock(",
                "resolve_apk(",
                "install_if_needed(",
                "start_instrumentation(",
                "cleanup_stale_server(",
                "long_cooldown(",
                "adb::forward(",
                "adb::install(",
                "adb::uninstall(",
                "std::fs::write(",
                "std::fs::create_dir",
                "std::fs::remove_",
                "Command::new(",
                ".spawn(",
            ] {
                assert!(
                    !source.contains(forbidden_call),
                    "passive chain member {name} gained forbidden lifecycle dependency {forbidden_call}"
                );
            }
        }

        let dispatch_source = include_str!("../../cli.rs");
        let start = dispatch_source
            .find("        Cmd::Collect {\n            app,\n            out,")
            .expect("collect dispatch arm");
        let dispatch = &dispatch_source[start..];
        let end = dispatch
            .find("        // `log` and `why`")
            .expect("collect arm end");
        let dispatch = &dispatch[..end];
        assert!(dispatch.contains("selection.resolve_online(&config)"));
        for forbidden_call in [
            "selection.resolve(&config)",
            "ensure_ready(",
            "ensure_ready_for_command(",
            "adb::forward(",
            "adb::reverse(",
        ] {
            assert!(
                !dispatch.contains(forbidden_call),
                "collect dispatch gained forbidden dependency {forbidden_call}"
            );
        }
    }

    #[test]
    fn high_risk_lifecycle_call_sites_are_an_exact_reviewed_inventory() {
        // This is intentionally an exact source seam, not a heuristic search
        // for words such as "start". These APIs are the narrow lifecycle
        // choke-points behind package replacement, server bring-up, and ADB
        // port-map mutation. A new caller or an extra call in an existing file
        // forces a contract review. Low-level implementations are excluded:
        // their public callers are the dependencies inventoried here.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest.join("src");
        let excluded = [
            "device/adb.rs",
            "device/installer.rs",
            "device/portmap.rs",
            "cmd/introspect/effects.rs",
        ];
        let seams: &[(&str, &[(&str, usize)])] = &[
            ("installer::ensure_ready_for_command(", &[("cli.rs", 2)]),
            (
                "installer::ensure_ready(",
                &[("cli.rs", 2), ("cmd/doctor.rs", 1)],
            ),
            (
                "installer::probe_existing(",
                &[
                    ("cmd/collect.rs", 1),
                    ("cmd/doctor.rs", 1),
                    ("cmd/why.rs", 1),
                ],
            ),
            ("adb::forward(", &[("cmd/agent.rs", 1)]),
            (
                "adb::forward_remove(",
                &[("cli.rs", 2), ("cmd/agent.rs", 1)],
            ),
            ("adb::reverse_replace(", &[("net/commands.rs", 4)]),
            ("adb::reverse_remove(", &[("cmd/doctor.rs", 1)]),
            ("adb::install(", &[("cmd/app_install.rs", 1)]),
            ("adb::uninstall(", &[("cmd/app_install.rs", 1)]),
        ];

        fn rust_files(dir: &std::path::Path, output: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read CLI source directory") {
                let path = entry.expect("read CLI source entry").path();
                if path.is_dir() {
                    rust_files(&path, output);
                } else if path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs") {
                    output.push(path);
                }
            }
        }

        let mut files = Vec::new();
        rust_files(&root, &mut files);
        for (needle, expected) in seams {
            let mut actual = std::collections::BTreeMap::new();
            for path in &files {
                let relative = path.strip_prefix(&root).unwrap();
                let relative = relative.to_string_lossy().replace('\\', "/");
                if excluded.contains(&relative.as_str()) {
                    continue;
                }
                let source = std::fs::read_to_string(path).expect("read CLI Rust source");
                let count = source.matches(needle).count();
                if count > 0 {
                    actual.insert(relative, count);
                }
            }
            let expected = expected
                .iter()
                .map(|(path, count)| ((*path).to_string(), *count))
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(
                actual, expected,
                "lifecycle seam {needle:?} changed; update the relevant EffectfulDependency declarations before accepting the new call site"
            );
        }

        // Tie the non-central callers to the dependency table they exercise.
        for (path, dependency) in [
            ("collect", D::ExistingServerProbe),
            ("why", D::ExistingServerProbe),
            ("doctor", D::ServerEnsureReady),
            ("app install", D::PackageInstaller),
            ("app reinstall", D::PackageInstaller),
            ("net start", D::PortMappingMutation),
            ("net stop", D::PortMappingMutation),
            ("aar capture", D::PortMappingMutation),
            ("aar intercept", D::PortMappingMutation),
            ("aar resume", D::PortMappingMutation),
            ("aar drop", D::PortMappingMutation),
            ("aar agent", D::PortMappingMutation),
            ("aar coroutines", D::PortMappingMutation),
        ] {
            assert!(
                leaf_contract(path)
                    .unwrap()
                    .dependencies
                    .contains(&dependency),
                "{path} does not declare reviewed dependency {dependency:?}"
            );
        }
    }
}
