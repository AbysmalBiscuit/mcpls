//! The hook socket, over a temporary runtime directory.
//!
//! These are integration tests rather than unit tests because what they
//! check is ownership between processes-worth of state: two listeners
//! racing, a stale file, a lock outliving a socket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::future::BoxFuture;
use mcpls_core::hooks::{HookListener, Request, Response, SocketIdentity, send};
use tempfile::TempDir;

/// A `SocketIdentity` whose socket and lock live inside a `TempDir`, so no
/// test touches the real runtime directory.
///
/// The guard is returned rather than dropped: dropping it deletes the
/// directory the socket lives in.
///
/// On Windows the socket is a pipe name, which is not a filesystem path, so
/// the guard covers only the lock there and the pipe name is made unique
/// with the same random suffix.
fn temp_identity() -> (TempDir, SocketIdentity) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let hash = format!("{:016x}", rand_suffix());
    #[cfg(windows)]
    let socket = std::path::PathBuf::from(format!(r"\\.\pipe\mcpls-test-{hash}"));
    #[cfg(not(windows))]
    let socket = dir.path().join(format!("{hash}.sock"));
    let identity = SocketIdentity {
        socket,
        lock: dir.path().join(format!("{hash}.lock")),
        hash,
    };
    (dir, identity)
}

/// A per-test suffix, so two tests running in parallel never collide on a
/// Windows pipe name, which is process-global rather than directory-scoped.
///
/// `TempDir` already gives uniqueness on Unix; this is what gives it on
/// Windows. Derived from the current thread id and the clock rather than
/// from a new random-number dependency.
fn rand_suffix() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    std::time::SystemTime::now().hash(&mut hasher);
    hasher.finish()
}

/// A handler literal, annotated so it coerces to the `Fn(Request) ->
/// BoxFuture<'static, Response>` bound `serve` declares.
///
/// Without the return-type annotation the closure's opaque future type does
/// not unify with `BoxFuture`, and the error points at `serve` rather than
/// at the closure.
fn handler(
    f: impl Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static,
) -> impl Fn(Request) -> BoxFuture<'static, Response> + Send + Sync + 'static {
    f
}

#[tokio::test]
async fn test_one_listener_acquires_and_a_second_defers() {
    let (_guard, identity) = temp_identity();
    let first = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("the first owns it");
    let second = HookListener::acquire(&identity).await.expect("acquire");

    assert!(second.is_none(), "the lock is held");
    drop(first);
}

#[tokio::test]
async fn test_a_second_listener_acquires_after_the_owner_drops() {
    let (_guard, identity) = temp_identity();
    let first = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    drop(first);

    let second = HookListener::acquire(&identity).await.expect("acquire");
    assert!(
        second.is_some(),
        "an owner exiting must not strand every later instance"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn test_a_stale_socket_file_does_not_block_acquisition() {
    // A named pipe is not a filesystem object, so a crashed owner on
    // Windows leaves nothing behind for this test to strand.
    let (_guard, identity) = temp_identity();
    std::fs::create_dir_all(identity.socket.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&identity.socket, b"").expect("a stale file where a socket used to be");

    let listener = HookListener::acquire(&identity).await.expect("acquire");
    assert!(
        listener.is_some(),
        "a crashed owner leaves its socket file behind and nothing else \
         will ever clean it up"
    );
}

#[tokio::test]
async fn test_exactly_one_of_many_racing_acquirers_wins() {
    let (_guard, identity) = temp_identity();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let identity = identity.clone();
        set.spawn(async move { HookListener::acquire(&identity).await.expect("acquire") });
    }
    // Every acquired listener is kept, not just counted, until every racer
    // has reported in: dropping the winner as soon as it is known would
    // release its lock while stragglers are still attempting theirs, and a
    // straggler that then acquires the now-free lock is a second success
    // that never overlapped the first. Holding every winner open for the
    // whole race is what makes "how many attempts overlapped a held lock"
    // the thing being measured, rather than "how many attempts landed
    // after some earlier one let go."
    let mut acquired = Vec::new();
    while let Some(result) = set.join_next().await {
        acquired.push(result.expect("the task"));
    }
    let winners = acquired
        .iter()
        .filter(|listener| listener.is_some())
        .count();
    assert_eq!(
        winners, 1,
        "renaming a temp socket into place is atomic but not exclusive, so \
         several racers would each believe they won and all but one would \
         be orphaned with no way to notice"
    );
}

#[tokio::test]
async fn test_a_request_reaches_the_owner_and_is_answered() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    tokio::spawn(listener.serve(
        handler(|_req| {
            Box::pin(async {
                Response::Flush {
                    context: Some("hello".to_string()),
                }
            })
        }),
        Duration::from_millis(1500),
        cancel,
    ));

    let response = send(
        &identity,
        &Request::Flush {
            session: "s1".to_string(),
        },
        Duration::from_secs(5),
    )
    .await
    .expect("the owner answers");

    assert_eq!(
        response,
        Response::Flush {
            context: Some("hello".to_string())
        }
    );
}

/// The server's own deadline, not the client's.
///
/// The client timeout here is 5 seconds and the server deadline is 200
/// milliseconds, so a run with no server-side deadline sits for the whole 5
/// seconds and then returns `Err`. Both assertions below fail in that case.
/// The reverse arrangement, a 30 second handler under a 1500 ms client
/// timeout, would be satisfied by the client erroring and would prove
/// nothing about the server.
#[tokio::test]
async fn test_an_op_answers_within_its_deadline_while_its_work_runs_on() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    tokio::spawn(listener.serve(
        handler(|_req| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Response::Flush { context: None }
            })
        }),
        Duration::from_millis(200),
        cancel,
    ));

    let started = std::time::Instant::now();
    let response = send(
        &identity,
        &Request::Flush {
            session: "s1".to_string(),
        },
        Duration::from_secs(5),
    )
    .await;

    assert!(
        matches!(response, Ok(Response::Error { .. })),
        "a hook that hangs blocks the agent, and the host's own timeout is \
         600 seconds, so the bound has to be ours and it has to answer \
         rather than drop the connection; got {response:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "and it has to answer at the deadline, not when the work finishes"
    );
}

#[tokio::test]
async fn test_two_requests_share_one_connection() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    tokio::spawn(listener.serve(
        handler(|req| {
            Box::pin(async move {
                // clippy::single_match_else's own rewrite for a two-arm
                // match with a wildcard arm.
                if let Request::Changed { paths, .. } = req {
                    Response::Changed {
                        queued: paths.len(),
                    }
                } else {
                    Response::Flush {
                        context: Some("drained".to_string()),
                    }
                }
            })
        }),
        Duration::from_millis(1500),
        cancel,
    ));

    let answers = mcpls_core::hooks::send_many(
        &identity,
        &[
            Request::Changed {
                session: "s1".to_string(),
                paths: vec![std::path::PathBuf::from("a.rs")],
                event: mcpls_core::hooks::ChangeEvent::Change,
            },
            Request::Flush {
                session: "s1".to_string(),
            },
        ],
        Duration::from_secs(5),
    )
    .await
    .expect("the owner answers both");

    assert_eq!(
        answers,
        vec![
            Response::Changed { queued: 1 },
            Response::Flush {
                context: Some("drained".to_string())
            },
        ],
        "the spec's PostToolBatch sends changed then flush on one \
         connection, and the answers come back in the order they were sent"
    );
}

#[tokio::test]
async fn test_sending_to_nobody_fails_fast() {
    let (_guard, identity) = temp_identity();
    let started = std::time::Instant::now();
    let result = send(&identity, &Request::Status, Duration::from_millis(50)).await;

    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an edit must never wait on diagnostics that are not there"
    );
}
