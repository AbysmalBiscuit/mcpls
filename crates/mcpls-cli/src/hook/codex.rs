//! Dispatching one Codex hook invocation.
//!
//! Codex's payloads differ from Claude Code's in the fields read here: a
//! subagent carries its own `agent_id`, and the `apply_patch` edit tool names
//! its files inside a patch envelope rather than in a `file_path`. Codex
//! fails a hook whose JSON output carries a field it does not know.

use std::path::{Path, PathBuf};

use anyhow::Result;
use mcpls_core::bridge::{HookAgent, HookHost};
use mcpls_core::hooks::{ChangeEvent, Request, SocketIdentity, send, send_and_acknowledge};
use serde::Deserialize;
use serde_json::Value;

use super::{FLUSH_SOCKET_TIMEOUT, SOCKET_TIMEOUT, additional_context_output, flush_context};

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

/// Every file path an `apply_patch` envelope names, in its order and
/// spelling. An envelope that never opens or never closes names nothing.
///
/// Only an unprefixed line inside the envelope reads as a header, since
/// patch content always sits behind `+`, `-` or a space. Body lines go
/// uninspected: producers disagree on whether a blank context line keeps
/// its leading space, and refusing an envelope over one would drop the
/// attribution for every file it names.
fn apply_patch_paths(envelope: &str) -> Vec<&str> {
    let mut lines = envelope.lines();
    if lines.next() != Some("*** Begin Patch") {
        return Vec::new();
    }
    let mut paths = Vec::new();
    let mut after_update = false;
    for line in lines {
        if line == "*** End Patch" {
            return paths;
        }
        let Some((verb, path)) = line
            .strip_prefix("*** ")
            .and_then(|header| header.split_once(": "))
            .filter(|(_, path)| !path.is_empty())
        else {
            after_update = false;
            continue;
        };
        match verb {
            "Add File" | "Delete File" => after_update = false,
            "Update File" => after_update = true,
            // A rename names its destination only straight after the
            // `Update File` whose source it replaces.
            "Move to" if after_update => after_update = false,
            _ => {
                after_update = false;
                continue;
            }
        }
        paths.push(path);
    }
    Vec::new()
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
    let session = payload.session_id.clone();
    let agent = HookAgent {
        agent_id: payload.agent_id.clone(),
        host: HookHost::Codex,
    };

    match payload.hook_event_name.as_str() {
        "PostToolUse" => {
            let requests = [
                Request::Changed {
                    attributed: true,
                    agent: agent.clone(),
                    session: session.clone(),
                    paths: payload.patched_paths(project_dir),
                    event: ChangeEvent::Change,
                },
                Request::Flush {
                    agent: agent.clone(),
                    session,
                },
            ];
            let responses = send_and_acknowledge(identity, &requests, FLUSH_SOCKET_TIMEOUT).await?;
            let context = responses.into_iter().nth(1).and_then(flush_context);
            Ok(additional_context_output("PostToolUse", context))
        }

        "UserPromptSubmit" => {
            let responses = send_and_acknowledge(
                identity,
                &[Request::Flush {
                    agent: agent.clone(),
                    session,
                }],
                FLUSH_SOCKET_TIMEOUT,
            )
            .await?;
            let context = responses.into_iter().next().and_then(flush_context);
            Ok(additional_context_output("UserPromptSubmit", context))
        }

        "SessionEnd" => {
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

    /// A producer that strips trailing whitespace leaves a blank context
    /// line bare, with no leading space. The files the patch names are
    /// still the files it wrote.
    #[test]
    fn test_a_bare_blank_context_line_keeps_the_patch_attributed() {
        let patch = "*** Begin Patch\n\
                     *** Update File: src/a.rs\n\
                     @@\n\
                     -old\n\
                     \n\
                     +new\n\
                     *** End Patch\n";
        assert_eq!(apply_patch_paths(patch), vec!["src/a.rs"]);
    }

    #[test]
    fn test_crlf_and_spaces_in_paths_survive() {
        let patch = "*** Begin Patch\r\n\
                     *** Update File: src/old name.rs\r\n\
                     *** Move to: src/new name.rs\r\n\
                     @@\r\n\
                     -a\r\n\
                     +b\r\n\
                     *** End Patch\r\n";
        assert_eq!(
            apply_patch_paths(patch),
            vec!["src/old name.rs", "src/new name.rs"]
        );
    }

    #[test]
    fn test_an_unclosed_or_unopened_envelope_names_nothing() {
        assert!(apply_patch_paths("*** Update File: outside-an-envelope.rs").is_empty());
        assert!(
            apply_patch_paths("*** Begin Patch\n*** Update File: src/a.rs\n@@\n+b\n").is_empty()
        );
    }
}
