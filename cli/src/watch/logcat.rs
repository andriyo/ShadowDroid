//! Crash detection via `adb logcat`.
//!
//! Tail with `-v threadtime -T 1 AndroidRuntime:E ActivityManager:E libc:F DEBUG:F *:S`.
//! Parser is a direct port of the proven movi `crash.py` regexes.
//!
//! State machine: idle → collecting (after `FATAL EXCEPTION`/`Fatal signal`) →
//! finalise after a quiet window (default 1s) or when another crash starts.
//! On finalise, fetch ~60 lines of broader context via `adb logcat -d -t 60`
//! and `adb shell getprop` for device info, then emit a `crash` event.

#![allow(dead_code)]

use anyhow::Result;

pub async fn run(_serial: String) -> Result<()> {
    todo!("M3 — port from movi/crash.py")
}
