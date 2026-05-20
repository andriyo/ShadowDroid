//! Device-facing layer. Two responsibilities:
//!   1. `adb`     — talk to the host `adbd` over the ADB protocol (port 5037),
//!                  not by shelling out. Used for forward/install/push/pull/logcat.
//!   2. `client`  — HTTP client to the on-device ShadowDroid server (port 7912
//!                  after `adb forward`), used for every UI operation.
//!
//! The `installer` submodule wraps both: it checks whether our APK is present
//! and the right version, downloads + installs from GitHub releases if not,
//! then `am instrument`s the runner and verifies the HTTP server is up.

pub mod actions;
pub mod adb;
pub mod client;
pub mod installer;
