//! The daemon's bounded logpoint event stream, with the same paging contract
//! as the Studio plugin's `LogpointEventLog.kt`: sequence numbers only for
//! accepted events, a per-stream id that changes when the daemon restarts,
//! `after`-cursor reads with an optional long-poll, and eviction/rate-limit
//! counters so a follower can report loss instead of hiding it.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value as Json, json};
use tokio::sync::Notify;

pub const DEFAULT_CAPACITY: usize = 512;
pub const DEFAULT_MAX_MESSAGE_CHARS: u32 = 16_384;
pub const MAX_MESSAGE_CHARS: u32 = 65_536;
/// Default hit rate for suspending logpoints and conditional breakpoints.
/// Each suspending hit costs the app ~5 ms median / 8 ms p90 on its main
/// loop (emulator measurement), so the default stays low.
pub const DEFAULT_MAX_EVENTS_PER_SECOND: u32 = 20;
/// Hard ceiling for `--max-events-per-second` on the jdwp backend.
pub const MAX_EVENTS_PER_SECOND: u32 = 100;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub breakpoint_id: Option<String>,
    pub owner: Option<String>,
    pub session: Option<String>,
}

struct Entry {
    seq: u64,
    breakpoint_id: String,
    owner: Option<String>,
    session_id: String,
    payload: Json,
}

impl Entry {
    fn matches(&self, filter: &Filter) -> bool {
        filter
            .breakpoint_id
            .as_ref()
            .is_none_or(|id| *id == self.breakpoint_id)
            && filter
                .owner
                .as_ref()
                .is_none_or(|owner| Some(owner) == self.owner.as_ref())
            && filter
                .session
                .as_ref()
                .is_none_or(|session| *session == self.session_id)
    }
}

struct Inner {
    entries: VecDeque<Entry>,
    next_seq: u64,
    evicted_total: u64,
    rate_limited_total: u64,
}

pub struct LogpointLog {
    inner: Mutex<Inner>,
    notify: Notify,
    stream_id: String,
    capacity: usize,
}

impl LogpointLog {
    pub fn new(stream_id: String, capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: VecDeque::new(),
                next_seq: 1,
                evicted_total: 0,
                rate_limited_total: 0,
            }),
            notify: Notify::new(),
            stream_id,
            capacity: capacity.max(1),
        }
    }

    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }

    /// Count a callback the per-breakpoint rate limit dropped.
    pub fn note_rate_limited(&self) {
        self.inner.lock().expect("log lock").rate_limited_total += 1;
    }

    /// Accept an event; `payload` gets its `seq`. Returns the sequence.
    pub fn append(
        &self,
        breakpoint_id: &str,
        owner: Option<&str>,
        session_id: &str,
        mut payload: Json,
    ) -> u64 {
        let seq = {
            let mut inner = self.inner.lock().expect("log lock");
            let seq = inner.next_seq;
            inner.next_seq += 1;
            payload["seq"] = json!(seq);
            inner.entries.push_back(Entry {
                seq,
                breakpoint_id: breakpoint_id.to_string(),
                owner: owner.map(str::to_string),
                session_id: session_id.to_string(),
                payload,
            });
            while inner.entries.len() > self.capacity {
                inner.entries.pop_front();
                inner.evicted_total += 1;
            }
            seq
        };
        self.notify.notify_waiters();
        seq
    }

    fn bounds(inner: &Inner) -> (u64, u64) {
        let latest = inner.next_seq - 1;
        let oldest = inner
            .entries
            .front()
            .map(|e| e.seq)
            .unwrap_or(if latest == 0 { 0 } else { latest + 1 });
        (latest, oldest)
    }

    fn ready(&self, after: u64, filter: &Filter) -> bool {
        let inner = self.inner.lock().expect("log lock");
        let (_, oldest) = Self::bounds(&inner);
        let overflowed = oldest > 0 && after < oldest.saturating_sub(1);
        overflowed
            || inner
                .entries
                .iter()
                .any(|e| e.seq > after && e.matches(filter))
    }

    /// One page. `after == None` is a tail read of the newest matches;
    /// otherwise events strictly after the cursor, waiting up to `timeout`
    /// for one to arrive.
    pub async fn read(
        &self,
        after: Option<u64>,
        limit: usize,
        filter: &Filter,
        timeout: Duration,
    ) -> Json {
        let limit = limit.max(1);
        let mut timed_out = false;
        if let Some(after) = after
            && !timeout.is_zero()
        {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let notified = self.notify.notified();
                tokio::pin!(notified);
                // Registered before the check, so an append in between wakes us.
                notified.as_mut().enable();
                if self.ready(after, filter) {
                    break;
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    timed_out = !self.ready(after, filter);
                    break;
                }
            }
        }
        let inner = self.inner.lock().expect("log lock");
        let (latest, oldest) = Self::bounds(&inner);
        let overflowed = after.is_some_and(|after| oldest > 0 && after < oldest - 1);
        let (events, next_cursor): (Vec<Json>, u64) = match after {
            None => {
                let matched: Vec<&Entry> =
                    inner.entries.iter().filter(|e| e.matches(filter)).collect();
                let skip = matched.len().saturating_sub(limit);
                (
                    matched[skip..].iter().map(|e| e.payload.clone()).collect(),
                    latest,
                )
            }
            Some(after) => {
                let mut selected = Vec::new();
                let mut examined = after.min(latest);
                for entry in inner.entries.iter().filter(|e| e.seq > after) {
                    examined = entry.seq;
                    if entry.matches(filter) {
                        selected.push(entry.payload.clone());
                        if selected.len() >= limit {
                            break;
                        }
                    }
                }
                let next = if selected.len() >= limit {
                    examined
                } else {
                    latest
                };
                (selected, next)
            }
        };
        json!({
            "stream_id": self.stream_id,
            "events": events,
            "next_cursor": next_cursor,
            "latest_cursor": latest,
            "oldest_cursor": oldest,
            "overflowed": overflowed,
            "evicted_total": inner.evicted_total,
            "rate_limited_total": inner.rate_limited_total,
            "timed_out": timed_out,
            "buffer_capacity": self.capacity,
        })
    }
}

/// Truncate to at most `max` chars; returns `(text, truncated, original_chars)`.
pub fn truncate_message(message: &str, max: u32) -> (String, bool, usize) {
    let max = max.clamp(1, MAX_MESSAGE_CHARS) as usize;
    let original = message.chars().count();
    if original <= max {
        (message.to_string(), false, original)
    } else {
        (message.chars().take(max).collect(), true, original)
    }
}

/// Per-breakpoint one-second rate window.
#[derive(Clone, Debug, Default)]
pub struct RateWindow {
    second: u64,
    accepted: u32,
}

impl RateWindow {
    /// Whether one more event fits in the current second.
    pub fn admit(&mut self, now_ms: u64, max_per_second: u32) -> bool {
        let second = now_ms / 1000;
        if second != self.second {
            self.second = second;
            self.accepted = 0;
        }
        if self.accepted >= max_per_second.max(1) {
            return false;
        }
        self.accepted += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(capacity: usize) -> LogpointLog {
        LogpointLog::new("logpoints_test".into(), capacity)
    }

    #[tokio::test]
    async fn pages_follow_the_cursor_contract() {
        let log = log(3);
        for i in 0..5 {
            let owner = if i % 2 == 0 { Some("a") } else { Some("b") };
            log.append(&format!("bp_{i}"), owner, "s", json!({"i": i}));
        }
        // Capacity 3: seqs 3, 4, 5 retained; 1 and 2 evicted.
        let tail = log.read(None, 2, &Filter::default(), Duration::ZERO).await;
        assert_eq!(tail["events"].as_array().unwrap().len(), 2);
        assert_eq!(tail["events"][0]["seq"], 4);
        assert_eq!(tail["next_cursor"], 5);
        assert_eq!(tail["oldest_cursor"], 3);
        assert_eq!(tail["evicted_total"], 2);
        assert_eq!(tail["stream_id"], "logpoints_test");

        let gap = log
            .read(Some(0), 10, &Filter::default(), Duration::ZERO)
            .await;
        assert_eq!(gap["overflowed"], true);
        assert_eq!(gap["events"].as_array().unwrap().len(), 3);

        let owned = Filter {
            owner: Some("a".into()),
            ..Filter::default()
        };
        let page = log.read(Some(2), 10, &owned, Duration::ZERO).await;
        let seqs: Vec<_> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, [3, 5]);
        assert_eq!(page["next_cursor"], 5);

        let limited = log
            .read(Some(2), 1, &Filter::default(), Duration::ZERO)
            .await;
        assert_eq!(limited["next_cursor"], 3, "stops at the last examined");
    }

    #[tokio::test]
    async fn reads_long_poll_until_a_matching_event_or_timeout() {
        let log = std::sync::Arc::new(log(8));
        let timed = log
            .read(Some(0), 5, &Filter::default(), Duration::from_millis(30))
            .await;
        assert_eq!(timed["timed_out"], true);
        assert_eq!(timed["events"].as_array().unwrap().len(), 0);

        let writer = log.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            writer.append("bp_1", None, "s", json!({}));
        });
        let page = log
            .read(Some(0), 5, &Filter::default(), Duration::from_secs(5))
            .await;
        assert_eq!(page["timed_out"], false);
        assert_eq!(page["events"][0]["seq"], 1);
        log.note_rate_limited();
        let page = log.read(None, 1, &Filter::default(), Duration::ZERO).await;
        assert_eq!(page["rate_limited_total"], 1);
    }

    #[test]
    fn truncation_and_rate_windows_are_bounded() {
        assert_eq!(truncate_message("héllo", 3), ("hél".into(), true, 5));
        assert_eq!(truncate_message("ok", 300), ("ok".into(), false, 2));
        let mut window = RateWindow::default();
        assert!(window.admit(1_000, 2));
        assert!(window.admit(1_500, 2));
        assert!(!window.admit(1_900, 2));
        assert!(window.admit(2_000, 2), "a new second resets the window");
    }
}
