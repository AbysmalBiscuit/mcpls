//! `mcpls lsp`: read and change the language servers a checkout's
//! backend runs.

use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use mcpls_core::bridge::{LspAction, ServerLifecycle};
use mcpls_core::hooks::protocol::ServerStatus;
use mcpls_core::hooks::{ProbeOutcome, Request, Response, SocketIdentity, probe};

/// Longer than the backend's default hook `op_deadline_ms`, so its own
/// deadline error arrives instead of a local timeout.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What a command prints, and whether it exits 0.
pub struct Outcome {
    /// The report, newline-terminated.
    pub text: String,
    /// Whether every server reached the state asked for.
    pub success: bool,
}

impl Outcome {
    fn failed(text: &str) -> Self {
        Self {
            text: format!("{text}\n"),
            success: false,
        }
    }
}

/// Print every applicable server and its state.
pub async fn status(identity: &SocketIdentity, root: &Path) -> Outcome {
    match ask(identity, root, &Request::Status).await {
        Ok(Response::Status { servers, .. }) => Outcome {
            text: render(&servers),
            success: true,
        },
        Ok(other) => Outcome::failed(&format!("the backend answered unexpectedly: {other:?}")),
        Err(text) => Outcome::failed(&text),
    }
}

/// Apply `action` to `servers`, every applicable server when empty, then
/// wait up to `wait` for started servers to settle.
pub async fn control(
    identity: &SocketIdentity,
    root: &Path,
    action: LspAction,
    servers: Vec<String>,
    wait: Option<Duration>,
) -> Outcome {
    let request = Request::Lsp { action, servers };
    let states = match ask(identity, root, &request).await {
        Ok(Response::Lsp { servers }) => servers,
        Ok(Response::Error { message }) => return Outcome::failed(&message),
        Ok(other) => {
            return Outcome::failed(&format!("the backend answered unexpectedly: {other:?}"));
        }
        Err(text) => return Outcome::failed(&text),
    };
    let states = match wait {
        Some(ceiling) => {
            let current = move || async move {
                match ask(identity, root, &Request::Status).await {
                    Ok(Response::Status { servers, .. }) => Some(servers),
                    _ => None,
                }
            };
            settle(current, states, ceiling).await
        }
        None => states,
    };
    Outcome {
        success: reached(action, &states),
        text: render(&states),
    }
}

async fn ask(
    identity: &SocketIdentity,
    root: &Path,
    request: &Request,
) -> Result<Response, String> {
    match probe(identity, request, ANSWER_TIMEOUT).await {
        ProbeOutcome::Answered(response) => Ok(response),
        ProbeOutcome::NoOwner => Err(format!(
            "no backend serves {}; one starts with an agent session",
            root.display()
        )),
        ProbeOutcome::Refused(reply) => Err(format!(
            "the backend (mcpls {}) refused this build ({}); run `mcpls doctor`",
            reply.version,
            mcpls_core::backend::VERSION
        )),
        ProbeOutcome::Busy => Err(format!(
            "the backend did not answer within {}s",
            ANSWER_TIMEOUT.as_secs()
        )),
        ProbeOutcome::Unintelligible(error) => {
            Err(format!("could not read the backend's answer: {error}"))
        }
    }
}

/// Poll until no server in `states` is `starting`, `ceiling` passes, or
/// the backend stops answering.
async fn settle<F, Fut>(
    mut current: F,
    mut states: Vec<ServerStatus>,
    ceiling: Duration,
) -> Vec<ServerStatus>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<Vec<ServerStatus>>>,
{
    let deadline = Instant::now() + ceiling;
    while states.iter().any(|s| s.state == ServerLifecycle::Starting) && Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
        let Some(latest) = current().await else {
            break;
        };
        for state in &mut states {
            if let Some(found) = latest.iter().find(|latest| latest.id == state.id) {
                state.state = found.state;
            }
        }
    }
    states
}

fn reached(action: LspAction, states: &[ServerStatus]) -> bool {
    let target = match action {
        LspAction::Stop => ServerLifecycle::Stopped,
        LspAction::Start | LspAction::Restart => ServerLifecycle::Running,
    };
    states.iter().all(|s| s.state == target)
}

fn render(states: &[ServerStatus]) -> String {
    if states.is_empty() {
        return "no language servers apply here\n".to_string();
    }
    states.iter().fold(String::new(), |mut text, s| {
        let _ = writeln!(text, "{}  {}", s.id, s.state);
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(id: &str, state: ServerLifecycle) -> ServerStatus {
        ServerStatus {
            id: id.to_string(),
            state,
        }
    }

    #[test]
    fn test_a_server_still_starting_misses_its_target() {
        let states = vec![status("fake", ServerLifecycle::Starting)];
        assert!(!reached(LspAction::Start, &states));
        assert_eq!(render(&states), "fake  starting\n");
    }

    #[test]
    fn test_success_needs_every_server_at_the_target() {
        assert!(reached(
            LspAction::Stop,
            &[
                status("a", ServerLifecycle::Stopped),
                status("b", ServerLifecycle::Stopped)
            ]
        ));
        assert!(!reached(
            LspAction::Restart,
            &[
                status("a", ServerLifecycle::Running),
                status("b", ServerLifecycle::Failed)
            ]
        ));
    }

    #[test]
    fn test_no_applicable_servers_is_a_success_that_says_so() {
        assert!(reached(LspAction::Stop, &[]));
        assert_eq!(render(&[]), "no language servers apply here\n");
    }

    #[tokio::test]
    async fn test_settling_gives_up_at_the_ceiling() {
        let started = std::time::Instant::now();
        let states = settle(
            || async { Some(vec![status("fake", ServerLifecycle::Starting)]) },
            vec![status("fake", ServerLifecycle::Starting)],
            Duration::from_millis(300),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(states, vec![status("fake", ServerLifecycle::Starting)]);
    }

    #[tokio::test]
    async fn test_settling_picks_up_the_latest_state() {
        let states = settle(
            || async { Some(vec![status("fake", ServerLifecycle::Running)]) },
            vec![status("fake", ServerLifecycle::Starting)],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(states, vec![status("fake", ServerLifecycle::Running)]);
    }
}
