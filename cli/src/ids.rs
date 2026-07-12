//! `Serial` — a newtype around an adb device serial.
//!
//! Serials and package names are both bare `String`s that flow through much of
//! the call graph, and `fn foo(serial, package)` would happily accept them
//! swapped. Wrapping the serial in a distinct type makes such a swap a compile
//! error (the package stays a `&str`, so the two parameters are no longer
//! interchangeable) at near-zero runtime cost. `Deref<Target = str>` +
//! `Display` + `From<&Serial> for String` keep call sites ergonomic: a `&Serial`
//! prints, compares, and passes into `adb`'s `impl Into<String>` parameters
//! without ceremony.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::ops::Deref;

/// `#[serde(transparent)]`: a `Serial` serializes as its bare string, so it drops
/// into `json!({"device": serial})` exactly like the `String` it replaced, and
/// round-trips through `DaemonConfig` (which is serialized to the detached daemon
/// and read back) without a custom impl.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Serial(String);

impl Serial {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for Serial {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Serial {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Serial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Serial {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Serial {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Lets a `&Serial` flow into `adb`'s `impl Into<String>` parameters unchanged.
impl From<&Serial> for String {
    fn from(s: &Serial) -> Self {
        s.0.clone()
    }
}

/// A readable, bounded, collision-resistant filename component for external
/// identifiers such as device serials and AVD names. Replacing punctuation
/// alone is not injective (`a:b` and `a/b` both become `a_b`), so every prefix
/// carries a short digest of the original value.
pub(crate) fn stable_file_component(value: &str) -> String {
    let mut prefix = value
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if prefix.is_empty() {
        prefix.push('_');
    }
    let digest = Sha256::digest(value.as_bytes());
    use std::fmt::Write as _;
    for byte in &digest[..8] {
        let _ = write!(prefix, "{byte:02x}");
    }
    let split = prefix.len() - 16;
    prefix.insert(split, '-');
    prefix
}

impl PartialEq<str> for Serial {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

#[cfg(test)]
mod tests {
    use super::stable_file_component;

    #[test]
    fn stable_components_are_safe_bounded_and_collision_resistant() {
        let name = stable_file_component("Pixel/9; $(unsafe)");
        assert!(!name.contains('/'));
        assert!(!name.contains(';'));
        assert!(name.len() <= 57);
        assert_ne!(
            stable_file_component("device:5555"),
            stable_file_component("device/5555")
        );
        assert_eq!(name, stable_file_component("Pixel/9; $(unsafe)"));
    }
}
