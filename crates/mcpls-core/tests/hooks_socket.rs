//! The hook socket, over a temporary runtime directory.
//!
//! These are integration tests rather than unit tests because what they
//! check is ownership between processes-worth of state: two listeners
//! racing, a stale file, a lock outliving a socket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use mcpls_core::hooks::listener::ServeExit;
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
/// Windows. Derived from the process id and a counter rather than from a
/// new random-number dependency.
fn rand_suffix() -> u64 {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    // The pid separates concurrent processes, the counter separates calls
    // within one. The clock cannot: it ticks every 100ns and
    // `DefaultHasher` is unseeded, so simultaneous starts collide.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
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

/// A client that skips the handshake gets nothing served: the listener
/// cannot tell a hook request from an MCP frame or a newer build's line.
#[tokio::test]
async fn test_a_connection_without_a_handshake_is_not_served() {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let (dir, identity) = temp_identity();
    let listener = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(listener.serve(
        handler(|_| Box::pin(async { Response::Ack })),
        Duration::from_secs(1),
        cancel_rx,
    ));

    #[cfg(not(windows))]
    let mut stream = tokio::net::UnixStream::connect(&identity.socket)
        .await
        .expect("connect");
    #[cfg(windows)]
    let mut stream = tokio::net::windows::named_pipe::ClientOptions::new()
        .open(&identity.socket)
        .expect("connect");
    stream
        .write_all(b"{\"op\":\"status\"}\n")
        .await
        .expect("write");

    let mut line = String::new();
    let read = tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(stream).read_line(&mut line),
    )
    .await
    .expect("the listener answers or hangs up rather than waiting");
    let reply: Option<mcpls_core::backend::HandshakeReply> = serde_json::from_str(&line).ok();
    assert!(
        read.map_or(true, |n| n == 0) || reply.is_some_and(|reply| reply.refusal.is_some()),
        "a request line was served as if it were a handshake: {line}"
    );
    drop(dir);
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
async fn test_two_concurrent_clients_reach_one_owner_before_either_finishes() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity).await.unwrap().unwrap();
    let both = Arc::new(tokio::sync::Barrier::new(2));
    let (cancel, rx) = tokio::sync::watch::channel(false);
    let owner = tokio::spawn(listener.serve(
        handler(move |request| {
            let both = Arc::clone(&both);
            Box::pin(async move {
                both.wait().await;
                let Request::Flush { session, .. } = request else {
                    panic!("expected flush")
                };
                Response::Flush {
                    context: Some(session),
                    token: None,
                }
            })
        }),
        Duration::from_secs(1),
        rx,
    ));
    let first = Request::Flush {
        agent: mcpls_core::bridge::HookAgent::default(),
        session: "first".into(),
    };
    let second = Request::Flush {
        agent: mcpls_core::bridge::HookAgent::default(),
        session: "second".into(),
    };
    let (a, b) = tokio::join!(
        send(&identity, &first, Duration::from_millis(1500)),
        send(&identity, &second, Duration::from_millis(1500)),
    );
    for (response, expected) in [(a, "first"), (b, "second")] {
        assert_eq!(
            response.unwrap(),
            Response::Flush {
                context: Some(expected.into()),
                token: None
            }
        );
    }
    cancel.send(true).unwrap();
    owner.await.unwrap();
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
async fn test_serve_reports_cancelled_when_the_cancel_watch_fires() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
    let serve_task = tokio::spawn(listener.serve(
        handler(|_req| {
            Box::pin(async {
                Response::Flush {
                    context: None,
                    token: None,
                }
            })
        }),
        Duration::from_millis(1500),
        cancel,
    ));

    cancel_tx
        .send(true)
        .expect("the cancel watch still has a receiver");

    let exit = tokio::time::timeout(Duration::from_secs(3), serve_task)
        .await
        .expect("serve must return promptly once cancelled, not hang")
        .expect("the serve task");
    assert_eq!(exit, ServeExit::Cancelled);
}

#[tokio::test]
async fn test_exactly_one_of_many_racing_acquirers_wins() {
    let (_guard, identity) = temp_identity();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let identity = identity.clone();
        set.spawn(async move { HookListener::acquire(&identity).await.expect("acquire") });
    }
    // Retain the winner until every contender has attempted acquisition.
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
                    token: None,
                }
            })
        }),
        Duration::from_millis(1500),
        cancel,
    ));

    let response = send(
        &identity,
        &Request::Flush {
            agent: mcpls_core::bridge::HookAgent::default(),
            session: "s1".to_string(),
        },
        Duration::from_secs(5),
    )
    .await
    .expect("the owner answers");

    assert_eq!(
        response,
        Response::Flush {
            context: Some("hello".to_string()),
            token: None
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
                Response::Flush {
                    context: None,
                    token: None,
                }
            })
        }),
        Duration::from_millis(200),
        cancel,
    ));

    let started = std::time::Instant::now();
    let response = send(
        &identity,
        &Request::Flush {
            agent: mcpls_core::bridge::HookAgent::default(),
            session: "s1".to_string(),
        },
        Duration::from_secs(5),
    )
    .await;

    let Ok(Response::Error { message }) = response else {
        panic!(
            "a hook that hangs blocks the agent, and the host's own timeout is \
             600 seconds, so the bound has to be ours and it has to answer \
             rather than drop the connection; got {response:?}"
        );
    };
    assert_eq!(
        message,
        "op exceeded 200ms; work continues in the background"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "and it has to answer at the deadline, not when the work finishes"
    );
}

/// Detached handler work survives the response deadline.
#[tokio::test]
async fn test_overrunning_work_completes_after_its_deadline_answered() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    tokio::spawn(listener.serve(
        handler(move |_req| {
            let done_tx = done_tx.clone();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                let _ = done_tx.send(()).await;
                Response::Flush {
                    context: None,
                    token: None,
                }
            })
        }),
        Duration::from_millis(100),
        cancel,
    ));

    let answer = send(
        &identity,
        &Request::Flush {
            agent: mcpls_core::bridge::HookAgent::default(),
            session: "s1".to_string(),
        },
        Duration::from_secs(5),
    )
    .await
    .expect("the owner answers at its deadline");
    assert!(matches!(answer, Response::Error { .. }), "got {answer:?}");

    let landed = tokio::time::timeout(Duration::from_secs(3), done_rx.recv()).await;
    assert_eq!(
        landed.expect("the overrunning work must not be cancelled at the deadline"),
        Some(()),
        "the deadline answers; it does not cancel"
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
                        token: None,
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
                agent: mcpls_core::bridge::HookAgent::default(),
                session: "s1".to_string(),
                paths: vec![std::path::PathBuf::from("a.rs")],
                event: mcpls_core::hooks::ChangeEvent::Change,
            },
            Request::Flush {
                agent: mcpls_core::bridge::HookAgent::default(),
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
                context: Some("drained".to_string()),
                token: None
            },
        ],
        "the spec's PostToolBatch sends changed then flush on one \
         connection, and the answers come back in the order they were sent"
    );
}

#[tokio::test]
async fn test_send_and_acknowledge_acks_a_tokened_flush_on_the_same_connection() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    let seen: Arc<std::sync::Mutex<Vec<Request>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    tokio::spawn(listener.serve(
        handler(move |req| {
            recorder.lock().expect("seen").push(req.clone());
            Box::pin(async move {
                match req {
                    Request::Flush { .. } => Response::Flush {
                        context: Some("2 errors in a.rs".to_string()),
                        token: Some(7),
                    },
                    Request::Ack { .. } => Response::Ack,
                    _ => Response::Error {
                        message: "unexpected".to_string(),
                    },
                }
            })
        }),
        Duration::from_millis(1500),
        cancel,
    ));

    let answers = mcpls_core::hooks::send_and_acknowledge(
        &identity,
        &[Request::Flush {
            agent: mcpls_core::bridge::HookAgent::default(),
            session: "s1".to_string(),
        }],
        Duration::from_secs(5),
    )
    .await
    .expect("the owner answers");

    assert_eq!(
        answers,
        vec![Response::Flush {
            context: Some("2 errors in a.rs".to_string()),
            token: Some(7),
        }],
        "the caller gets the flush answers and nothing else; the \
         acknowledgement's own answer is consumed on its behalf"
    );
    let seen = seen.lock().expect("seen").clone();
    assert_eq!(
        seen,
        vec![
            Request::Flush {
                agent: mcpls_core::bridge::HookAgent::default(),
                session: "s1".to_string(),
            },
            Request::Ack {
                agent: mcpls_core::bridge::HookAgent::default(),
                session: "s1".to_string(),
                token: 7,
            },
        ],
        "the acknowledgement names the session and the token the answer \
         carried, and follows the flush on the connection it came in on"
    );
}

/// The acknowledgement gets its own allowance after a slow flush consumes the caller's.
#[tokio::test]
async fn test_an_acknowledgement_outlasts_a_flush_that_spent_the_callers_bound() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    let acked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let recorder = Arc::clone(&acked);
    tokio::spawn(listener.serve(
        handler(move |req| {
            let recorder = Arc::clone(&recorder);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                match req {
                    Request::Flush { .. } => Response::Flush {
                        context: Some("2 errors in a.rs".to_string()),
                        token: Some(7),
                    },
                    Request::Ack { .. } => {
                        recorder.store(true, std::sync::atomic::Ordering::SeqCst);
                        Response::Ack
                    }
                    _ => Response::Error {
                        message: "unexpected".to_string(),
                    },
                }
            })
        }),
        Duration::from_millis(1500),
        cancel,
    ));

    let started = std::time::Instant::now();
    let answers = mcpls_core::hooks::send_and_acknowledge(
        &identity,
        &[Request::Flush {
            agent: mcpls_core::bridge::HookAgent::default(),
            session: "s1".to_string(),
        }],
        Duration::from_millis(250),
    )
    .await
    .expect("the flush lands inside the caller's bound");
    let elapsed = started.elapsed();

    assert_eq!(
        answers,
        vec![Response::Flush {
            context: Some("2 errors in a.rs".to_string()),
            token: Some(7),
        }]
    );
    assert!(
        elapsed >= Duration::from_millis(350),
        "the acknowledgement has to wait out the owner's own 200ms, which the \
         caller's bound no longer has room for; returning at about 250ms means \
         it was cut off, leaving the commit to whether the write happened to \
         complete before the deadline was consulted: {elapsed:?}"
    );
    assert!(
        acked.load(std::sync::atomic::Ordering::SeqCst),
        "and the owner has to have been given the acknowledgement to answer"
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
