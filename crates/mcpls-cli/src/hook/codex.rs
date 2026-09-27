//! Dispatching one Codex hook invocation.
//!
//! Codex's payloads differ from Claude Code's in the fields read here: a
//! subagent carries its own `agent_id`, and the `apply_patch` edit tool names
//! its files inside a patch envelope rather than in a `file_path`. Codex
//! fails a hook whose JSON output carries a field it does not know.

use std::path::{Path, PathBuf};

use anyhow::Result;
use mcpls_core::bridge::HookHost;
use mcpls_core::hooks::{ChangeEvent, Request, SocketIdentity, send, send_and_acknowledge};
use pabal::{Codex, CodexView, Fields, Payload, Response as HookOutput};

use super::{
    FLUSH_SOCKET_TIMEOUT, HookEvent, SOCKET_TIMEOUT, flush_output, session_and_agent, written_paths,
};

pub(super) async fn run(
    event: HookEvent,
    stdin: &str,
    project_dir: &Path,
    identity: Option<&SocketIdentity>,
) -> Result<HookOutput> {
    let payload = Payload::<Codex>::parse_named(event.wire_name(), stdin)?;
    let Some(identity) = identity else {
        return Ok(HookOutput::none());
    };
    let (session, agent) = session_and_agent(&payload, HookHost::Codex);

    match payload.view() {
        CodexView::PostToolUse(post) => {
            // The envelope's relative paths are written against the
            // session's `cwd`.
            let cwd = post.cwd().unwrap_or(project_dir);
            let paths: Vec<PathBuf> = written_paths(post.tool())
                .into_iter()
                .map(|path| cwd.join(path))
                .collect();
            let requests = [
                Request::Changed {
                    attributed: true,
                    agent: agent.clone(),
                    session: session.clone(),
                    paths,
                    event: ChangeEvent::Change,
                },
                Request::Flush { agent, session },
            ];
            let responses = send_and_acknowledge(identity, &requests, FLUSH_SOCKET_TIMEOUT).await?;
            Ok(flush_output(&post, responses.into_iter().nth(1)))
        }

        CodexView::UserPromptSubmit(prompt) => {
            let responses = send_and_acknowledge(
                identity,
                &[Request::Flush { agent, session }],
                FLUSH_SOCKET_TIMEOUT,
            )
            .await?;
            Ok(flush_output(&prompt, responses.into_iter().next()))
        }

        CodexView::SessionEnd(_) => {
            send(identity, &Request::EndSession { session }, SOCKET_TIMEOUT).await?;
            Ok(HookOutput::none())
        }

        _ => Ok(HookOutput::none()),
    }
}
