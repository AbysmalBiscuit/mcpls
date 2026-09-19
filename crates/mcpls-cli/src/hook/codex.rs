//! Dispatching one Codex hook invocation.
//!
//! Codex's payloads differ from Claude Code's in the fields read here: a
//! subagent carries its own `agent_id`, and the `apply_patch` edit tool names
//! its files inside a patch envelope rather than in a `file_path`. Codex
//! fails a hook whose JSON output carries a field it does not know.

use std::path::{Path, PathBuf};

use anyhow::Result;
use mcpls_core::hooks::{ChangeEvent, Request, SocketIdentity, send, send_and_acknowledge};
use serde::Deserialize;
use serde_json::Value;

use super::{FLUSH_SOCKET_TIMEOUT, SOCKET_TIMEOUT, additional_context_output, flush_context};

/// Envelope headers that name a file. `Move to` is a rename's destination,
/// so a rename reports both ends.
const PATCH_VERBS: [&str; 4] = ["Add File", "Update File", "Delete File", "Move to"];

/// The Codex hook payload, keeping only the fields the dispatch below reads.
/// Every field but the event name is defaulted, since which ones are present
/// depends on the event.
#[derive(Debug, Deserialize)]
struct CodexPayload {
    hook_event_name: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: Value,
}

impl CodexPayload {
    /// The session a request is filed under. A subagent files under
    /// `session/agent`, so its diagnostics reach it rather than its parent.
    fn session(&self) -> String {
        match self.agent_id.as_deref() {
            Some(agent) if !agent.is_empty() => format!("{}/{agent}", self.session_id),
            _ => self.session_id.clone(),
        }
    }

    /// The files an `apply_patch` call touched, resolved against the
    /// session's `cwd`, which the envelope's relative paths are written
    /// against.
    fn patched_paths(&self, project_dir: &Path) -> Vec<PathBuf> {
        if self.tool_name != "apply_patch" {
            return Vec::new();
        }
        let cwd = self.cwd.as_deref().unwrap_or(project_dir);
        self.tool_input
            .get("command")
            .and_then(Value::as_str)
            .map(apply_patch_paths)
            .unwrap_or_default()
            .into_iter()
            .map(|path| cwd.join(path))
            .collect()
    }
}

/// Every file path an `apply_patch` envelope names, in order, verbatim.
fn apply_patch_paths(envelope: &str) -> Vec<&str> {
    envelope
        .lines()
        .filter_map(|line| {
            let (verb, path) = line.trim().strip_prefix("*** ")?.split_once(": ")?;
            let path = path.trim();
            (PATCH_VERBS.contains(&verb) && !path.is_empty()).then_some(path)
        })
        .collect()
}

pub(super) async fn run(
    stdin: &str,
    project_dir: &Path,
    identity: Option<&SocketIdentity>,
) -> Result<String> {
    let payload: CodexPayload = serde_json::from_str(stdin)?;
    let Some(identity) = identity else {
        return Ok(String::new());
    };
    let session = payload.session();

    match payload.hook_event_name.as_str() {
        "PostToolUse" => {
            let requests = [
                Request::Changed {
                    session: session.clone(),
                    paths: payload.patched_paths(project_dir),
                    event: ChangeEvent::Change,
                },
                Request::Flush { session },
            ];
            let responses = send_and_acknowledge(identity, &requests, FLUSH_SOCKET_TIMEOUT).await?;
            let context = responses.into_iter().nth(1).and_then(flush_context);
            Ok(additional_context_output("PostToolUse", context))
        }

        "UserPromptSubmit" => {
            let responses = send_and_acknowledge(
                identity,
                &[Request::Flush { session }],
                FLUSH_SOCKET_TIMEOUT,
            )
            .await?;
            let context = responses.into_iter().next().and_then(flush_context);
            Ok(additional_context_output("UserPromptSubmit", context))
        }

        "SubagentStop" | "SessionEnd" => {
            send(identity, &Request::EndSession { session }, SOCKET_TIMEOUT).await?;
            Ok(String::new())
        }

        _ => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_patch_paths_names_every_verb_and_both_ends_of_a_rename() {
        let patch = "*** Begin Patch\n\
                     *** Add File: src/new.rs\n\
                     +*** Update File: not/a/header.rs\n\
                     *** Update File: src/old.rs\n\
                     *** Move to: src/renamed.rs\n\
                     @@\n\
                     -a\n\
                     +b\n\
                     *** Delete File: src/gone.rs\n\
                     *** End Patch\n";
        assert_eq!(
            apply_patch_paths(patch),
            vec!["src/new.rs", "src/old.rs", "src/renamed.rs", "src/gone.rs"]
        );
    }
}
