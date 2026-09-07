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
    /// Sent by the `FileChanged` hook for one path, and by the
    /// `PostToolBatch` hook for a batch's paths ahead of its `flush`.
    Changed {
        /// The Claude Code session that made the edit.
        session: String,
        /// The files the host reports as touched.
        paths: Vec<PathBuf>,
        /// The kind of change the host observed.
        event: ChangeEvent,
    },
    /// Sent by the `PostToolBatch` hook after its `changed` events, and by
    /// the `UserPromptSubmit` hook, asking for the diagnostics context to
    /// inject before the next turn.
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

/// The `Response` literals asserted below are the wire contract for this
/// protocol, not merely a record of how `serde` happens to serialize these
/// types today. The design spec pins the three `Request` lines under its
/// Protocol heading; it does not pin `Response`, so these literals are what
/// defines the response side. Changing one of them changes the protocol.
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

    #[test]
    fn test_the_changed_response_pins_the_wire_shape() {
        let literal = r#"{"op":"changed","queued":3}"#;
        let value = Response::Changed { queued: 3 };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_the_flush_response_pins_the_wire_shape_with_context_present() {
        let literal = r#"{"op":"flush","context":"2 errors in a.rs"}"#;
        let value = Response::Flush {
            context: Some("2 errors in a.rs".to_string()),
        };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_the_flush_response_pins_the_wire_shape_with_context_absent() {
        // `Option<String>` has no `skip_serializing_if` here, so an absent
        // context is a present `context` key holding JSON `null`, not an
        // omitted key.
        let literal = r#"{"op":"flush","context":null}"#;
        let value = Response::Flush { context: None };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_the_end_session_response_pins_the_wire_shape() {
        let literal = r#"{"op":"end_session"}"#;
        let value = Response::EndSession;
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_the_status_response_pins_the_wire_shape() {
        let literal =
            r#"{"op":"status","hash":"abc123","socket":"mcpls.sock","pid":42,"owner":true}"#;
        let value = Response::Status {
            hash: "abc123".to_string(),
            socket: PathBuf::from("mcpls.sock"),
            pid: 42,
            owner: true,
        };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_the_error_response_pins_the_wire_shape() {
        let literal = r#"{"op":"error","message":"boom"}"#;
        let value = Response::Error {
            message: "boom".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_the_status_request_pins_the_wire_shape() {
        let literal = r#"{"op":"status"}"#;
        let value = Request::Status;
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Request>(literal).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_a_multiline_string_field_does_not_produce_a_raw_newline() {
        let value = Response::Error {
            message: "line one\nline two".to_string(),
        };
        let line = serde_json::to_string(&value).expect("serialize");
        assert!(
            !line.contains('\n'),
            "an embedded newline must be escaped, not left raw in the framing"
        );
        assert_eq!(
            serde_json::from_str::<Response>(&line).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_an_omitted_context_key_also_deserializes_to_none() {
        let literal = r#"{"op":"flush"}"#;
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            Response::Flush { context: None }
        );
    }
}
