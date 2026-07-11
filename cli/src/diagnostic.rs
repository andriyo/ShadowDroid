//! Stable, agent-facing command failures.
//!
//! `anyhow` remains useful inside a subsystem, but errors that cross the CLI
//! boundary should become a [`DiagnosticError`].  That keeps automation from
//! scraping prose and gives every known failure an explicit retry posture and
//! recovery path.

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DiagnosticError {
    pub code: String,
    pub stage: String,
    pub retryable: bool,
    pub message: String,
    pub detail: Value,
    pub next_actions: Vec<String>,
    /// Optional process status for commands that intentionally preserve a
    /// child process's failure code (for example `shadowdroid test -- ...`).
    /// This is host control metadata, not part of the JSON error schema.
    pub process_exit_code: Option<i32>,
}

impl DiagnosticError {
    pub fn new(
        code: impl Into<String>,
        stage: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            stage: stage.into(),
            retryable: false,
            message: message.into(),
            detail: Value::Object(Default::default()),
            next_actions: Vec::new(),
            process_exit_code: None,
        }
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn detail(mut self, detail: Value) -> Self {
        self.detail = detail;
        self
    }

    pub fn next_actions<I, S>(mut self, actions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.next_actions = actions.into_iter().map(Into::into).collect();
        self
    }

    pub fn process_exit_code(mut self, code: i32) -> Self {
        self.process_exit_code = Some(code);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_keep_machine_fields_separate_from_message() {
        let error = DiagnosticError::new("wait_timeout", "ui", "element did not appear")
            .retryable(true)
            .detail(serde_json::json!({"timeout_ms": 25}))
            .next_actions(["shadowdroid ui dump"]);

        assert_eq!(error.code, "wait_timeout");
        assert_eq!(error.stage, "ui");
        assert!(error.retryable);
        assert_eq!(error.detail["timeout_ms"], 25);
        assert_eq!(error.next_actions, ["shadowdroid ui dump"]);
        assert_eq!(error.process_exit_code, None);
    }

    #[test]
    fn preserves_a_requested_child_exit_code() {
        let error =
            DiagnosticError::new("test_command_failed", "test", "failed").process_exit_code(17);
        assert_eq!(error.process_exit_code, Some(17));
    }
}
