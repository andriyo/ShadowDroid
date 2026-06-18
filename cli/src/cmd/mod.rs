//! Larger, multi-step subcommands that don't fit the one-liner pattern in
//! [crate::cli]. Each module owns one verb (or a small family) and exposes a
//! `run(...)` entry point that `cli::run` dispatches to.
//!
//! These commands are **host-only**: they compose `adb` ([crate::device::adb])
//! and the existing on-device routes, so they ship without an APK change. Most
//! deliberately do *not* go through [crate::device::installer::ensure_ready] —
//! `doctor` diagnoses the very server `ensure_ready` would start, and `collect`
//! must still produce a bundle when the server can't come up.

pub mod aar;
pub mod app_install;
pub mod collect;
pub mod config;
pub mod debug;
pub mod debugger;
pub mod device_profile;
pub mod doctor;
pub mod introspect;
pub mod layout;
pub mod permissions;
pub mod scroll;
pub mod skill;
pub mod studio;
pub mod studio_contract;
