//! Declarative popup-killers. Pure port of movi/watcher.py.
//!
//! Rule shape (same as movi):
//!   {
//!     "name":   "allow_notifications",
//!     "when":   {"text": "Allow"},
//!     "then":   [{"cmd": "tap_text", "value": "Allow"}],
//!     "max_fires": 1
//!   }
//!
//! On every emitted screen, check each rule. If `when` matches an element,
//! mark rule fired (anti-loop: skip if screen_hash unchanged from last fire),
//! dispatch each command in `then`, emit `watcher_fired`.

#![allow(dead_code)]

use crate::proto::Element;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WatcherRule {
    pub name: String,
    pub when: WhenQuery,
    pub then: Vec<serde_json::Value>, // raw CLI command JSON
    #[serde(default)]
    pub max_fires: Option<u32>,
    #[serde(default)]
    pub fire_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WhenQuery {
    pub text: Option<String>,
    pub rid: Option<String>,
    pub desc: Option<String>,
    pub klass: Option<String>,
    pub clickable: Option<bool>,
}

impl WhenQuery {
    pub fn matches(&self, el: &Element) -> bool {
        matches_text(el.text.as_deref(), self.text.as_deref())
            && matches_text(el.rid.as_deref(), self.rid.as_deref())
            && matches_text(el.desc.as_deref(), self.desc.as_deref())
            && matches_text(el.klass.as_deref(), self.klass.as_deref())
            && self.clickable.map(|v| el.clickable == v).unwrap_or(true)
    }
}

#[derive(Default)]
pub struct WatcherSet {
    inner: Mutex<Vec<WatcherRule>>,
}

#[derive(Debug, Clone)]
pub struct WatcherHit {
    pub name: String,
    pub matched: Element,
    pub screen_hash: String,
    pub then: Vec<serde_json::Value>,
}

impl WatcherSet {
    pub fn from_files(paths: &[String]) -> Result<Self> {
        let set = Self::default();
        for path in paths {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading watcher file {path}"))?;
            let mut rules =
                parse_rules(&text).with_context(|| format!("parsing watcher file {path}"))?;
            let mut guard = set.inner.lock().expect("watcher mutex poisoned");
            guard.append(&mut rules);
        }
        Ok(set)
    }

    pub fn add(&self, rule: WatcherRule) {
        let mut guard = self.inner.lock().expect("watcher mutex poisoned");
        if let Some(existing) = guard.iter_mut().find(|r| r.name == rule.name) {
            *existing = rule;
        } else {
            guard.push(rule);
        }
    }

    pub fn remove(&self, name: &str) -> bool {
        let mut guard = self.inner.lock().expect("watcher mutex poisoned");
        let before = guard.len();
        guard.retain(|rule| rule.name != name);
        guard.len() != before
    }

    pub fn clear(&self) {
        self.inner.lock().expect("watcher mutex poisoned").clear();
    }

    pub fn list(&self) -> Vec<WatcherRule> {
        self.inner.lock().expect("watcher mutex poisoned").clone()
    }

    pub fn matches(&self, screen_hash: &str, elements: &[Element]) -> Vec<WatcherHit> {
        let mut hits = Vec::new();
        let mut guard = self.inner.lock().expect("watcher mutex poisoned");
        for rule in guard.iter_mut() {
            if rule
                .max_fires
                .map(|max| rule.fire_count >= max)
                .unwrap_or(false)
            {
                continue;
            }
            if rule.last_fired_hash.as_deref() == Some(screen_hash) {
                continue;
            }
            let Some(matched) = elements.iter().find(|el| rule.when.matches(el)).cloned() else {
                continue;
            };
            rule.fire_count += 1;
            rule.last_fired_hash = Some(screen_hash.to_string());
            hits.push(WatcherHit {
                name: rule.name.clone(),
                matched,
                screen_hash: screen_hash.to_string(),
                then: rule.then.clone(),
            });
        }
        hits
    }
}

fn parse_rules(text: &str) -> Result<Vec<WatcherRule>> {
    if let Ok(rules) = serde_json::from_str::<Vec<WatcherRule>>(text) {
        return Ok(rules);
    }
    Ok(vec![serde_json::from_str::<WatcherRule>(text)?])
}

fn matches_text(actual: Option<&str>, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    actual
        .map(|actual| actual.to_lowercase().contains(&expected.to_lowercase()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{WatcherRule, WatcherSet};
    use crate::proto::Element;

    fn element(text: &str) -> Element {
        Element {
            id: 1,
            text: Some(text.to_string()),
            desc: None,
            klass: Some("Button".to_string()),
            rid: None,
            bounds: [0, 0, 10, 10],
            tap: [5, 5],
            clickable: true,
            long_clickable: false,
            scrollable: false,
            checkable: false,
            focusable: false,
            enabled: true,
            selected: false,
            checked: false,
            focused: false,
            password: false,
            input: false,
        }
    }

    #[test]
    fn fires_once_per_screen_hash() {
        let set = WatcherSet::default();
        set.add(
            serde_json::from_value::<WatcherRule>(serde_json::json!({
                "name": "allow",
                "when": {"text": "allow", "clickable": true},
                "then": [{"cmd": "tap_text", "value": "Allow"}]
            }))
            .unwrap(),
        );
        assert_eq!(set.matches("a", &[element("Allow")]).len(), 1);
        assert_eq!(set.matches("a", &[element("Allow")]).len(), 0);
        assert_eq!(set.matches("b", &[element("Allow")]).len(), 1);
    }
}
