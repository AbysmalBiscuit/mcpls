//! Dispatching one Grok hook invocation.
//!
//! Grok reads hook context only from `PostToolUse`, which delivers it beside
//! the tool's result; it discards what a `UserPromptSubmit` or `SessionStart`
//! hook returns. So the edit's own hook carries its diagnostics back, and a
//! prompt has nothing to flush them into. A subagent's events name it only by
//! its own session id, with no link to the session that spawned it.

use std::path::{Path, PathBuf};

use anyhow::Result;
use mcpls_core::bridge::HookHost;
use mcpls_core::hooks::{ChangeEvent, Request, SocketIdentity, send, send_and_acknowledge};
use pabal::{Fields, Grok, GrokView, Payload, Response as HookOutput};

use super::{
    FLUSH_SOCKET_TIMEOUT, HookEvent, SOCKET_TIMEOUT, flush_output, session_and_agent, written_paths,
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

        GrokView::SessionEnd(_) => {
            send(identity, &Request::EndSession { session }, SOCKET_TIMEOUT).await?;
            Ok(HookOutput::none())
        }

        _ => Ok(HookOutput::none()),
    }
}
