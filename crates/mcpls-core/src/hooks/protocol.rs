//! The requests and responses that cross the hook socket.
//!
//! Framing is newline-delimited JSON: one request per line, one response per
//! line, so a single connection can carry a batch's `changed` events and the
//! `flush` that follows them without either side needing message-length
//! prefixes.
//!
//! The host's [`ChangeEvent`] is a hint only. A formatter that saves through
//! a temporary file and a rename produces an `unlink` for a path that exists
//! again by the time the hook connects, so the sweep derives the real kind
//! from a stat rather than trusting the wire value.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A message sent from a Claude Code hook to a running mcpls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Sent by the `PostToolUse` hook after an edit tool touches files.
    Changed {
        /// The Claude Code session that made the edit.
        session: String,
        /// The files the host reports as touched.
        paths: Vec<PathBuf>,
        /// The kind of change the host observed.
        event: ChangeEvent,
    },
    /// Sent by the `Stop` hook once a turn's edits are done, asking for the
    /// diagnostics context to inject before the next turn.
    Flush {
        /// The Claude Code session to flush.
        session: String,
    },
    /// Sent by the `SessionEnd` hook so mcpls can drop session-scoped state.
    EndSession {
        /// The Claude Code session that ended.
        session: String,
    },
    /// Sent by `mcpls hook doctor` to check whether a socket has a live
    /// owner before falling back to a cold start.
    Status,
}

/// The kind of filesystem change a host reports for a [`Request::Changed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeEvent {
    /// The file's contents changed.
    Change,
    /// The file was created.
    Add,
    /// The file was removed, or removed and not yet recreated.
    Unlink,
}

/// A message sent from a running mcpls back to a Claude Code hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Response {
    /// Answers a [`Request::Changed`].
    Changed {
        /// How many paths were queued for the sweep.
        queued: usize,
    },
    /// Answers a [`Request::Flush`].
    Flush {
        /// The diagnostics context to inject before the next turn, absent
        /// when there is nothing new to report.
        context: Option<String>,
    },
    /// Answers a [`Request::EndSession`].
    EndSession,
    /// Answers a [`Request::Status`].
    Status {
        /// The requesting project's socket identity hash.
        hash: String,
        /// The socket or named pipe this mcpls is listening on.
        socket: PathBuf,
        /// The process ID of the owning mcpls.
        pid: u32,
        /// Whether the responding process is itself the socket's owner.
        owner: bool,
    },
    /// Reports that a request could not be carried out.
    Error {
        /// A human-readable description of what went wrong.
        message: String,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// An absolute path with a drive letter on Windows, where
    /// `Url::from_file_path` fails without one.
    fn abs(rel: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:\\work\\{}", rel.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/work/{rel}"))
        }
    }

    #[test]
    fn test_a_changed_request_round_trips() {
        let request = Request::Changed {
            session: "s1".to_string(),
            paths: vec![abs("src/a.rs")],
            event: ChangeEvent::Change,
        };
        let line = serde_json::to_string(&request).expect("serialize");
        assert!(!line.contains('\n'), "the framing is one request per line");
        assert_eq!(
            serde_json::from_str::<Request>(&line).expect("deserialize"),
            request
        );
    }

    #[test]
    fn test_the_wire_names_match_the_spec() {
        let line = serde_json::to_string(&Request::Flush {
            session: "s1".to_string(),
        })
        .expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(parsed["op"], serde_json::json!("flush"));
        assert_eq!(parsed["session"], serde_json::json!("s1"));
    }

    #[test]
    fn test_an_unknown_op_is_an_error_rather_than_a_panic() {
        let parsed = serde_json::from_str::<Request>(r#"{"op":"explode"}"#);
        assert!(parsed.is_err());
    }

    #[test]
    fn test_the_three_change_events_the_host_sends_all_parse() {
        for (wire, expected) in [
            ("change", ChangeEvent::Change),
            ("add", ChangeEvent::Add),
            ("unlink", ChangeEvent::Unlink),
        ] {
            let line = format!(r#"{{"op":"changed","session":"s","paths":[],"event":"{wire}"}}"#);
            let Request::Changed { event, .. } = serde_json::from_str(&line).expect("deserialize")
            else {
                panic!("a changed request parsed as something else");
            };
            assert_eq!(event, expected);
        }
    }
}
