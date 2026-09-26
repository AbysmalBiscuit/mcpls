//! `mcpls status`: every backend this user runs, as one table for a person
//! to read.

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
/// backend still gets its row.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);

const HEADER: [&str; 4] = ["ROOT", "PID", "VERSION", "SESSIONS"];

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

/// The table of every backend this user runs.
///
/// # Errors
///
/// Returns an error when this user's endpoints cannot be listed.
pub async fn overview() -> std::io::Result<String> {
    let mut probes = tokio::task::JoinSet::new();
    for identity in mcpls_core::hooks::endpoints()? {
        probes.spawn(survey(identity));
    }
    let mut backends: Vec<Backend> = probes.join_all().await.into_iter().flatten().collect();
    backends.sort_by_cached_key(|backend| match &backend.answer {
        Answer::Serving { root, .. } => (false, root.clone()),
        _ => (true, backend.log.clone()),
    });
    Ok(render(&backends))
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
    fn cells(&self) -> [String; 4] {
        match &self.answer {
            Answer::Serving {
                root,
                pid,
                version,
                sessions,
                ..
            } => [
                root.display().to_string(),
                pid.to_string(),
                version.clone(),
                sessions.to_string(),
            ],
            Answer::Refused(reply) => [
                format!("unknown: {}", refusal_text(reply.refusal.as_ref())),
                reply.pid.to_string(),
                reply.version.clone(),
                reply.sessions.to_string(),
            ],
            Answer::Unreadable(why) => [
                format!("unknown: {why}"),
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
            ],
        }
    }

    fn servers(&self) -> String {
        match &self.answer {
            Answer::Serving { servers, .. } if servers.is_empty() => "none apply".to_string(),
            Answer::Serving { servers, .. } => {
                let mut groups = BTreeMap::<u8, (ServerLifecycle, Vec<&str>)>::new();
                for server in servers {
                    groups
                        .entry(rank(server.state))
                        .or_insert_with(|| (server.state, Vec::new()))
                        .1
                        .push(&server.id);
                }
                groups
                    .values()
                    .map(|(state, ids)| format!("{state}: {}", ids.join(", ")))
                    .collect::<Vec<_>>()
                    .join("; ")
            }
            Answer::Refused(_) | Answer::Unreadable(_) => "unknown".to_string(),
        }
    }
}

/// Where a state's servers sit on the line: the ones doing something
/// first, the idle majority last.
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

fn render(backends: &[Backend]) -> String {
    if backends.is_empty() {
        return "no mcpls backend is running\n".to_string();
    }
    let header = HEADER.map(str::to_string);
    let rows: Vec<[String; 4]> = backends.iter().map(Backend::cells).collect();
    let widths: [usize; 4] = std::array::from_fn(|column| {
        std::iter::once(&header)
            .chain(&rows)
            .map(|row| row[column].chars().count())
            .max()
            .unwrap_or(0)
    });
    let mut text = row_line(&header, &widths);
    for (backend, row) in backends.iter().zip(&rows) {
        text.push_str(&row_line(row, &widths));
        let _ = writeln!(text, "  servers  {}", backend.servers());
        let _ = writeln!(text, "  log      {}", backend.log.display());
    }
    text
}

fn row_line(cells: &[String; 4], widths: &[usize; 4]) -> String {
    let line = cells
        .iter()
        .zip(widths)
        .map(|(cell, &width)| format!("{cell:<width$}"))
        .collect::<Vec<_>>()
        .join("  ");
    format!("{}\n", line.trim_end())
}

#[cfg(test)]
mod tests {
    use mcpls_core::backend::Refusal;

    use super::*;

    #[test]
    fn test_backends_render_as_an_aligned_table_with_their_servers_and_log() {
        let serving = Backend {
            answer: Answer::Serving {
                root: PathBuf::from("/work/app"),
                pid: 4242,
                version: "0.4.0".to_string(),
                sessions: 2,
                servers: [
                    ("css", ServerLifecycle::Idle),
                    ("rust", ServerLifecycle::Running),
                    ("json", ServerLifecycle::Idle),
                    ("toml", ServerLifecycle::Stopped),
                ]
                .map(|(id, state)| ServerStatus {
                    id: id.to_string(),
                    state,
                })
                .to_vec(),
            },
            log: PathBuf::from("/run/mcpls/aaaa.log"),
        };
        let refused = Backend {
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
            render(&[serving, refused]),
            "\
ROOT                            PID   VERSION  SESSIONS
/work/app                       4242  0.4.0    2
  servers  running: rust; stopped: toml; idle: css, json
  log      /run/mcpls/aaaa.log
unknown: the two builds differ  7     0.3.0    1
  servers  unknown
  log      /run/mcpls/bbbb.log
"
        );
    }
}
