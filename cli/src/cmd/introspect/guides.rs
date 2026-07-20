//! On-demand per-domain driving guides (`commands --guide <topic>`).
//!
//! The always-loaded skill body carries only pointer stubs; the full guidance
//! for a domain lives here and is served from the CLI itself, so its context
//! cost is paid only when the domain is actually touched. Guides are keyed by
//! topic and by every command group they cover, so `--guide aar` and
//! `--guide net` land on the same document. Content is curated markdown beside
//! this file, compiled in so it always matches the installed CLI version.

pub(super) struct Guide {
    /// Canonical topic name (what the skill body points at).
    pub(super) topic: &'static str,
    /// Command groups this guide covers; each is also accepted as an alias.
    pub(super) covers: &'static [&'static str],
    pub(super) content: &'static str,
}

pub(super) const GUIDES: &[Guide] = &[
    Guide {
        topic: "net",
        covers: &["net", "aar"],
        content: include_str!("guides/net.md"),
    },
    Guide {
        topic: "debugger",
        covers: &["studio", "debug", "layout"],
        content: include_str!("guides/debugger.md"),
    },
    Guide {
        topic: "state",
        covers: &["app", "device", "perm", "appops", "profile", "files"],
        content: include_str!("guides/state.md"),
    },
];

/// Resolve a guide by canonical topic or by any command group it covers.
pub(super) fn find_guide(raw_topic: &str) -> Option<&'static Guide> {
    let topic = raw_topic.trim().to_lowercase();
    GUIDES
        .iter()
        .find(|guide| guide.topic == topic)
        .or_else(|| {
            GUIDES
                .iter()
                .find(|guide| guide.covers.contains(&topic.as_str()))
        })
}
