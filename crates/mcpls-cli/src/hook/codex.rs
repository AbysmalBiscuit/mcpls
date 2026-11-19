//! Dispatching one Codex hook invocation.
//!
//! Codex's payloads differ from Claude Code's in the fields read here: a
//! subagent carries its own `agent_id`, and the `apply_patch` edit tool names
//! its files inside a patch envelope rather than in a `file_path`. Codex
//! fails a hook whose JSON output carries a field it does not know.
//!
//! File events and flushes reach the backend of each file's checkout, as Claude Code's do.

use std::path::{Path, PathBuf};

use anyhow::Result;
use mcpls_core::bridge::HookHost;
use mcpls_core::hooks::{Request, SocketIdentity, send};
use pabal::{Codex, CodexView, Fields, Payload, Response as HookOutput};

use super::{
    Changes, HookEvent, SOCKET_TIMEOUT, backends_for, context_output, flush_backends,
    session_and_agent, written_paths,
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
            let backends = backends_for(project_dir, identity, paths);
            let context = flush_backends(backends, agent, session, Changes::Attributed).await;
            Ok(context_output(&post, context))
        }

        CodexView::UserPromptSubmit(prompt) => {
            let backends = backends_for(project_dir, identity, Vec::new());
            let context = flush_backends(backends, agent, session, Changes::None).await;
            Ok(context_output(&prompt, context))
        }

        CodexView::SessionEnd(_) => {
            send(identity, &Request::EndSession { session }, SOCKET_TIMEOUT).await?;
            Ok(HookOutput::none())
        }

        _ => Ok(HookOutput::none()),
    }
}
