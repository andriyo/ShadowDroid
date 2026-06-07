//! Fallback XML hierarchy parser.
//!
//! Normally the on-device server returns elements pre-flattened in `/v1/screen`.
//! This module is the safety net for `format=xml` mode and for any caller that
//! wants raw control.

#![allow(dead_code)]

use crate::proto::Element;
use anyhow::Result;

pub fn parse_hierarchy(_xml: &str, _viewport: (u32, u32)) -> Result<Vec<Element>> {
    todo!("M2 — quick_xml event-stream walk")
}

pub fn screen_hash(xml: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(xml.as_bytes());
    let digest = hasher.finalize();
    // first 8 bytes hex, matching the public screen_hash length
    let bytes = &digest.as_bytes()[..8];
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
