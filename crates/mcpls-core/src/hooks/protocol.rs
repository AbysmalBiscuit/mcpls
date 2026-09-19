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

use crate::bridge::{HookAgent, ServerLifecycle};

/// A message sent from a Claude Code hook to a running mcpls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Sent by the `PostToolBatch` hook for a batch's paths ahead of its
    /// `flush`.
    Changed {
        /// Whether a tool payload identifies the writer of these paths.
        #[serde(default)]
        attributed: bool,
        /// Agent identity supplied by the host, absent for root hooks.
        #[serde(flatten)]
        agent: HookAgent,
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
        /// Agent whose delivery record is read.
        #[serde(flatten)]
        agent: HookAgent,
        /// The Claude Code session to flush.
        session: String,
    },
    /// Sent by a hook once a `flush` answer carrying a token is in its
    /// hands, so the owner can mark that report delivered. Sent on the
    /// same connection as the `flush`. Never sent for an answer with no
    /// token, which had nothing to mark.
    Ack {
        /// Agent whose report is acknowledged.
        #[serde(flatten)]
        agent: HookAgent,
        /// The Claude Code session the acknowledged flush was for.
        session: String,
        /// The token the `flush` answer carried.
        token: u64,
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

/// One language server on a status response.
///
/// A struct rather than a pair, so future server details can live here
/// instead of growing parallel lists on the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    /// The server's routing identity.
    pub id: String,
    /// What the server is doing.
    pub state: ServerLifecycle,
}

/// What the backend's filesystem watcher is doing.
///
/// A struct rather than a rendered line, following [`ServerStatus`], so
/// later detail lands on the type instead of growing a parallel field on
/// the response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherStatus {
    /// Whether a watcher is running at all.
    pub watching: bool,
    /// How many directories carry a watch.
    pub directories: usize,
    /// Why no watcher runs, absent while one does.
    #[serde(default)]
    pub unwatched_reason: Option<String>,
    /// Why the directories watched are not the whole checkout, absent
    /// while they are.
    ///
    /// A subtree the walk could not traverse -- a permissions failure, a
    /// broken mount -- carries no watch, and a count on its own reads the
    /// same whether the walk reached everything or half of it.
    #[serde(default)]
    pub incomplete_reason: Option<String>,
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
        /// Names the record changes this answer implies, for the
        /// [`Request::Ack`] that commits them. Absent when the answer
        /// implies none, which is also when no acknowledgement is owed.
        token: Option<u64>,
    },
    /// Answers a [`Request::Ack`], whether or not anything was left to
    /// commit: the client cannot act on the difference.
    Ack,
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
        /// The owner's canonicalized startup directory, so `mcpls hook
        /// doctor` can print what the server sees beside what the hook
        /// sees.
        root: PathBuf,
        /// How many `Changed`, `Flush`, or `EndSession` requests this owner
        /// has answered since it started, so `mcpls hook doctor` can tell a
        /// server that is up but has never been sent a hook apart from one
        /// that is actually wired up to a host.
        hooks_seen: u64,
        /// This mcpls's version.
        #[serde(default)]
        version: String,
        /// How long the backend has run.
        #[serde(default)]
        uptime_ms: u64,
        /// The sessions attached.
        #[serde(default)]
        sessions: Vec<String>,
        /// The language servers registered and their current lifecycle.
        #[serde(default)]
        servers: Vec<ServerStatus>,
        /// The configuration fingerprint the backend started with.
        #[serde(default)]
        config_fingerprint: String,
        /// What the backend's filesystem watcher is doing.
        #[serde(default)]
        ///
        /// Boxed: it is the largest thing on the largest variant, and an
        /// unboxed one makes every `Response` the size of a status.
        watcher: Box<WatcherStatus>,
    },
    /// Reports a failure or a response deadline exceeded while work continues.
    Error {
        /// A human-readable description of what went wrong.
        message: String,
    },
}

/// The `Response` literals asserted below are the wire contract for this
/// protocol, not merely a record of how `serde` happens to serialize these
/// types today. The design spec pins the four `Request` lines under its
/// Protocol heading; it does not pin `Response`, so these literals are what
/// defines the response side. Changing one of them changes the protocol.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bridge::ServerLifecycle;

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
            attributed: false,
            agent: crate::bridge::HookAgent::default(),
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
            agent: crate::bridge::HookAgent::default(),
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
        let literal = r#"{"op":"flush","context":"2 errors in a.rs","token":7}"#;
        let value = Response::Flush {
            context: Some("2 errors in a.rs".to_string()),
            token: Some(7),
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
        // Neither `Option` has `skip_serializing_if`, so an absent context
        // or token is a present key holding JSON `null`, not an omitted
        // key.
        let literal = r#"{"op":"flush","context":null,"token":null}"#;
        let value = Response::Flush {
            context: None,
            token: None,
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
        let literal = r#"{"op":"status","hash":"abc123","socket":"mcpls.sock","pid":42,"owner":true,"root":"/work","hooks_seen":7,"version":"0.3.9","uptime_ms":61000,"sessions":["s1","connection-4"],"servers":[{"id":"rust","state":"running"},{"id":"lua","state":"not_installed"}],"config_fingerprint":"00000000000000ff","watcher":{"watching":true,"directories":56,"unwatched_reason":null,"incomplete_reason":"1 path(s) could not be walked: /work/vendor: permission denied"}}"#;
        let value = Response::Status {
            hash: "abc123".to_string(),
            socket: PathBuf::from("mcpls.sock"),
            pid: 42,
            owner: true,
            root: PathBuf::from("/work"),
            hooks_seen: 7,
            version: "0.3.9".to_string(),
            uptime_ms: 61_000,
            sessions: vec!["s1".to_string(), "connection-4".to_string()],
            servers: vec![
                ServerStatus {
                    id: "rust".to_string(),
                    state: ServerLifecycle::Running,
                },
                ServerStatus {
                    id: "lua".to_string(),
                    state: ServerLifecycle::NotInstalled,
                },
            ],
            watcher: Box::new(WatcherStatus {
                watching: true,
                directories: 56,
                unwatched_reason: None,
                incomplete_reason: Some(
                    "1 path(s) could not be walked: /work/vendor: permission denied".to_string(),
                ),
            }),
            config_fingerprint: "00000000000000ff".to_string(),
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
    fn test_every_server_lifecycle_round_trips_on_status() {
        use strum::IntoEnumIterator;

        for state in ServerLifecycle::iter() {
            let value = Response::Status {
                hash: "abc123".to_string(),
                socket: PathBuf::from("mcpls.sock"),
                pid: 42,
                owner: true,
                root: PathBuf::from("/work"),
                hooks_seen: 7,
                version: "0.3.9".to_string(),
                uptime_ms: 61_000,
                sessions: vec!["s1".to_string()],
                servers: vec![ServerStatus {
                    id: "language".to_string(),
                    state,
                }],
                config_fingerprint: "00000000000000ff".to_string(),
                watcher: Box::new(WatcherStatus::default()),
            };

            let wire = serde_json::to_value(&value).expect("serialize");
            assert_eq!(
                serde_json::from_value::<Response>(wire).expect("deserialize"),
                value
            );
        }
    }

    /// A status from a build that predates the backend fields still parses.
    #[test]
    fn test_an_older_status_parses_with_empty_backend_fields() {
        let literal = r#"{"op":"status","hash":"a","socket":"s","pid":1,"owner":true,"root":"/w","hooks_seen":0}"#;
        let Response::Status {
            sessions,
            version,
            watcher,
            ..
        } = serde_json::from_str::<Response>(literal).expect("deserialize")
        else {
            panic!("a status");
        };
        assert!(sessions.is_empty());
        assert!(version.is_empty());
        assert!(
            !watcher.watching,
            "a backend from before this field existed reports no watcher, \
             which is exactly what it has; a doctor that failed to parse it \
             would report nothing at all instead"
        );
    }

    /// A backend that knows about the watcher but not about incomplete
    /// coverage: the field is newer than the struct it sits on, and a
    /// doctor that failed to parse it would report nothing at all.
    #[test]
    fn test_a_watcher_status_without_the_incomplete_field_parses() {
        let literal = r#"{"watching":true,"directories":56,"unwatched_reason":null}"#;

        let watcher = serde_json::from_str::<WatcherStatus>(literal).expect("deserialize");

        assert!(watcher.watching);
        assert_eq!(watcher.directories, 56);
        assert_eq!(watcher.incomplete_reason, None);
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
    fn test_the_ack_request_pins_the_wire_shape() {
        let literal = r#"{"op":"ack","host":"claude","session":"s1","token":7}"#;
        let value = Request::Ack {
            agent: crate::bridge::HookAgent::default(),
            session: "s1".to_string(),
            token: 7,
        };
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
    fn test_the_ack_response_pins_the_wire_shape() {
        let literal = r#"{"op":"ack"}"#;
        let value = Response::Ack;
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
            Response::Flush {
                context: None,
                token: None
            }
        );
    }
}
