//! `mcpls status`: every backend this user runs, one block each for a
//! person to read.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use mcpls_core::backend::HandshakeReply;
use mcpls_core::bridge::ServerLifecycle;
use mcpls_core::hooks::protocol::ServerStatus;
use mcpls_core::hooks::{ProbeOutcome, Request, Response, SocketIdentity, probe};

use crate::hook::refusal_text;

/// Longer than the backend's default hook `op_deadline_ms`, so a slow
/// backend still gets its block.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);

/// Wide enough for the longest label, the `not installed` state.
const LABEL_WIDTH: usize = 13;

/// What one endpoint's owner said about itself.
enum Answer {
    Serving {
        root: PathBuf,
        pid: u32,
        version: String,
        sessions: usize,
        servers: Vec<ServerStatus>,
    },
    /// It answered the handshake and refused the status request.
    Refused(HandshakeReply),
    /// Something holds the endpoint; why its status is unknown.
    Unreadable(String),
}

struct Backend {
    answer: Answer,
    log: PathBuf,
}

/// Every backend this user runs, ordered by checkout.
///
/// # Errors
///
/// Returns why this user's endpoints cannot be listed.
pub async fn all() -> Result<String, String> {
    let endpoints = mcpls_core::hooks::endpoints()
        .map_err(|error| format!("could not list this user's mcpls endpoints: {error}"))?;
    let mut probes = tokio::task::JoinSet::new();
    for identity in endpoints {
        probes.spawn(survey(identity));
    }
    let mut backends: Vec<Backend> = probes.join_all().await.into_iter().flatten().collect();
    if backends.is_empty() {
        return Ok("no mcpls backend is running\n".to_string());
    }
    backends.sort_by_cached_key(|backend| match &backend.answer {
        Answer::Serving { root, .. } => (false, root.clone()),
        _ => (true, backend.log.clone()),
    });
    Ok(backends
        .iter()
        .map(Backend::block)
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Ask the owner of `identity` for its status, or `None` when nothing owns
/// it.
async fn survey(identity: SocketIdentity) -> Option<Backend> {
    let answer = match probe(&identity, &Request::Status, ANSWER_TIMEOUT).await {
        ProbeOutcome::Answered(Response::Status {
            root,
            pid,
            version,
            sessions,
            servers,
            owner: true,
            ..
        }) => Answer::Serving {
            root,
            pid,
            version,
            sessions: sessions.len(),
            servers,
        },
        ProbeOutcome::Answered(_) => Answer::Unreadable("answered without its status".to_string()),
        ProbeOutcome::Refused(reply) => Answer::Refused(reply),
        ProbeOutcome::Busy => {
            Answer::Unreadable(format!("no answer within {}s", ANSWER_TIMEOUT.as_secs()))
        }
        ProbeOutcome::Unintelligible(error) => {
            Answer::Unreadable(format!("unreadable answer: {error}"))
        }
        ProbeOutcome::NoOwner => return None,
    };
    Some(Backend {
        answer,
        log: identity.log_file(),
    })
}

impl Backend {
    /// The checkout on its own line, then one labelled line per fact.
    fn block(&self) -> String {
        let mut text = String::new();
        match &self.answer {
            Answer::Serving {
                root,
                pid,
                version,
                sessions,
                servers,
            } => {
                let _ = writeln!(text, "{}", root.display());
                field(&mut text, "backend", &process(*pid, version, *sessions));
                if servers.is_empty() {
                    field(&mut text, "servers", "none apply");
                }
                for (state, ids) in by_state(servers) {
                    field(&mut text, &state.to_string(), &ids.join(", "));
                }
            }
            Answer::Refused(reply) => {
                let _ = writeln!(
                    text,
                    "unknown checkout: {}",
                    refusal_text(reply.refusal.as_ref())
                );
                field(
                    &mut text,
                    "backend",
                    &process(reply.pid, &reply.version, reply.sessions),
                );
            }
            Answer::Unreadable(why) => {
                let _ = writeln!(text, "unknown checkout: {why}");
            }
        }
        field(&mut text, "log", &self.log.display().to_string());
        text
    }
}

fn field(text: &mut String, label: &str, value: &str) {
    let _ = writeln!(text, "  {label:<LABEL_WIDTH$}  {value}");
}

fn process(pid: u32, version: &str, sessions: usize) -> String {
    let plural = if sessions == 1 { "" } else { "s" };
    format!("pid {pid}, mcpls {version}, {sessions} session{plural}")
}

/// `servers` grouped by state, the ones doing something first and the
/// idle majority last.
fn by_state(servers: &[ServerStatus]) -> impl Iterator<Item = (ServerLifecycle, Vec<&str>)> {
    let mut groups = BTreeMap::<u8, (ServerLifecycle, Vec<&str>)>::new();
    for server in servers {
        groups
            .entry(rank(server.state))
            .or_insert_with(|| (server.state, Vec::new()))
            .1
            .push(&server.id);
    }
    groups.into_values()
}

const fn rank(state: ServerLifecycle) -> u8 {
    match state {
        ServerLifecycle::Running => 0,
        ServerLifecycle::Starting => 1,
        ServerLifecycle::Failed => 2,
        ServerLifecycle::NotInstalled => 3,
        ServerLifecycle::Stopped => 4,
        ServerLifecycle::Idle => 5,
    }
}

#[cfg(test)]
mod tests {
    use mcpls_core::backend::Refusal;

    use super::*;

    #[test]
    fn test_a_serving_backend_lists_its_servers_by_state() {
        let backend = Backend {
            answer: Answer::Serving {
                root: PathBuf::from("/work/app"),
                pid: 4242,
                version: "0.4.0".to_string(),
                sessions: 2,
                servers: [
                    ("css", ServerLifecycle::Idle),
                    ("rust", ServerLifecycle::Running),
                    ("json", ServerLifecycle::Idle),
                    ("markdown", ServerLifecycle::NotInstalled),
                ]
                .map(|(id, state)| ServerStatus {
                    id: id.to_string(),
                    state,
                })
                .to_vec(),
            },
            log: PathBuf::from("/run/mcpls/aaaa.log"),
        };

        assert_eq!(
            backend.block(),
            "\
/work/app
  backend        pid 4242, mcpls 0.4.0, 2 sessions
  running        rust
  not installed  markdown
  idle           css, json
  log            /run/mcpls/aaaa.log
"
        );
    }

    #[test]
    fn test_a_refusing_backend_names_why_its_checkout_is_unknown() {
        let backend = Backend {
            answer: Answer::Refused(HandshakeReply {
                version: "0.3.0".to_string(),
                pid: 7,
                sessions: 1,
                refusal: Some(Refusal::Build),
                ..HandshakeReply::new(0, None)
            }),
            log: PathBuf::from("/run/mcpls/bbbb.log"),
        };

        assert_eq!(
            backend.block(),
            "\
unknown checkout: the two builds differ
  backend        pid 7, mcpls 0.3.0, 1 session
  log            /run/mcpls/bbbb.log
"
        );
    }
}
