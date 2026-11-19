//! Dispatching one Grok hook invocation.
//!
//! Grok reads hook context only from `PostToolUse`, which delivers it beside
//! the tool's result; it discards what a `UserPromptSubmit` or `SessionStart`
//! hook returns. So the edit's own hook carries its diagnostics back, and a
//! prompt has nothing to flush them into. A subagent's events name it only by
//! its own session id, with no link to the session that spawned it.
//!
//! File events and flushes reach the backend of each file's checkout, as Claude Code's do.

use std::path::{Path, PathBuf};

use anyhow::Result;
use mcpls_core::bridge::HookHost;
use mcpls_core::hooks::{Request, SocketIdentity, send};
use pabal::{Fields, Grok, GrokView, Payload, Response as HookOutput};

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
    let payload = Payload::<Grok>::parse_named(event.wire_name(), stdin)?;
    let Some(identity) = identity else {
        return Ok(HookOutput::none());
    };
    // The events answered here carry no agent id, so the host never scopes
    // one. The wire has no Grok host, which keeps older backends reading it.
    let (session, agent) = session_and_agent(&payload, HookHost::Codex);

    match payload.view() {
        GrokView::PostToolUse(post) => {
            // A tool's relative `file_path` is written against the session's
            // `cwd`.
            let cwd = post.cwd().unwrap_or(project_dir);
            let paths: Vec<PathBuf> = written_paths(post.tool())
                .into_iter()
                .map(|path| cwd.join(path))
                .collect();
            let backends = backends_for(project_dir, identity, paths);
            let context = flush_backends(backends, agent, session, Changes::Attributed).await;
            Ok(context_output(&post, context))
        }

        GrokView::SessionEnd(_) => {
            send(identity, &Request::EndSession { session }, SOCKET_TIMEOUT).await?;
            Ok(HookOutput::none())
        }

        _ => Ok(HookOutput::none()),
    }
}
