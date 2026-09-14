#![allow(clippy::unwrap_used, clippy::expect_used)]

use rmcp::ServiceExt as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufStream, DuplexStream};

use super::*;

fn register_starting_configs(translator: &Translator, configs: &[lsp::ServerInitConfig]) {
    for config in configs {
        let id = config.server_config.id();
        translator.register_server_config(id.clone(), config.clone());
        translator.set_lifecycle(&id, bridge::ServerLifecycle::Starting);
    }
}

async fn wait_for_owner_baseline(settle: &bridge::ServerSettle, owner: &ServerId) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while settle.pending_diagnostics_baselines().contains(owner) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the diagnostics owner baseline merge must finish");
}

#[derive(Debug)]
pub struct RegistrationPause {
    server_id: ServerId,
    entered: tokio::sync::oneshot::Sender<lsp::LspClient>,
    resume: std::sync::mpsc::Receiver<()>,
}

pub fn pause_registration(translator: &Translator, id: &ServerId, client: lsp::LspClient) {
    let pause = {
        let mut slot = bridge::lock_std(&translator.registration_pause);
        if slot.as_ref().is_some_and(|pause| &pause.server_id == id) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(pause) = pause {
        pause.entered.send(client).unwrap();
        tokio::task::block_in_place(|| {
            pause
                .resume
                .recv_timeout(Duration::from_secs(5))
                .expect("registration pause was not released");
        });
    }
}

struct LifecyclePause {
    entered: tokio::sync::oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

struct AsyncLifecyclePause {
    entered: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

static OWNER_INSTALLATION_PAUSE: std::sync::Mutex<Option<LifecyclePause>> =
    std::sync::Mutex::new(None);
static BEFORE_OWNER_INSTALLATION_PAUSE: std::sync::Mutex<Option<LifecyclePause>> =
    std::sync::Mutex::new(None);
static RETIREMENT_PAUSE: std::sync::Mutex<Option<LifecyclePause>> = std::sync::Mutex::new(None);
static ASYNC_RETIREMENT_PAUSE: std::sync::Mutex<Option<AsyncLifecyclePause>> =
    std::sync::Mutex::new(None);
static BASELINE_ADOPTED: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>> =
    std::sync::Mutex::new(None);
static WORKSPACE_REQUEST_CANCELLED: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>> =
    std::sync::Mutex::new(None);
static REPLACEMENT_ABORTED: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>> =
    std::sync::Mutex::new(None);

struct BaselineCheckPause {
    entered: tokio::sync::oneshot::Sender<bool>,
    resume: std::sync::mpsc::Receiver<()>,
}

static BASELINE_CHECK_PAUSE: std::sync::Mutex<Option<BaselineCheckPause>> =
    std::sync::Mutex::new(None);

pub fn arm_owner_installation_pause(
    entered: tokio::sync::oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
) {
    *OWNER_INSTALLATION_PAUSE.lock().unwrap() = Some(LifecyclePause { entered, resume });
}

pub fn pause_owner_installation() {
    let pause = OWNER_INSTALLATION_PAUSE.lock().unwrap().take();
    if let Some(pause) = pause {
        pause.entered.send(()).unwrap();
        tokio::task::block_in_place(|| {
            pause
                .resume
                .recv_timeout(Duration::from_secs(5))
                .expect("owner installation pause was not released");
        });
    }
}

pub fn arm_before_owner_installation_pause(
    entered: tokio::sync::oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
) {
    *BEFORE_OWNER_INSTALLATION_PAUSE.lock().unwrap() = Some(LifecyclePause { entered, resume });
}

pub fn pause_before_owner_installation() {
    let pause = BEFORE_OWNER_INSTALLATION_PAUSE.lock().unwrap().take();
    if let Some(pause) = pause {
        pause.entered.send(()).unwrap();
        tokio::task::block_in_place(|| {
            pause
                .resume
                .recv_timeout(Duration::from_secs(5))
                .expect("before-owner installation pause was not released");
        });
    }
}

pub fn arm_retirement_pause(
    entered: tokio::sync::oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
) {
    *RETIREMENT_PAUSE.lock().unwrap() = Some(LifecyclePause { entered, resume });
}

pub fn pause_after_retirement() {
    let pause = RETIREMENT_PAUSE.lock().unwrap().take();
    if let Some(pause) = pause {
        pause.entered.send(()).unwrap();
        tokio::task::block_in_place(|| {
            pause
                .resume
                .recv_timeout(Duration::from_secs(5))
                .expect("retirement pause was not released");
        });
    }
}

pub fn arm_async_retirement_pause(
    entered: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
) {
    *ASYNC_RETIREMENT_PAUSE.lock().unwrap() = Some(AsyncLifecyclePause { entered, resume });
}

pub async fn pause_after_retirement_async() {
    let pause = ASYNC_RETIREMENT_PAUSE.lock().unwrap().take();
    if let Some(pause) = pause {
        pause.entered.send(()).unwrap();
        let _ = pause.resume.await;
    }
}

pub fn arm_workspace_request_cancellation(cancelled: tokio::sync::oneshot::Sender<()>) {
    *WORKSPACE_REQUEST_CANCELLED.lock().unwrap() = Some(cancelled);
}

pub fn mark_workspace_request_cancelled() {
    let cancelled = WORKSPACE_REQUEST_CANCELLED.lock().unwrap().take();
    if let Some(cancelled) = cancelled {
        let _ = cancelled.send(());
    }
}

pub fn arm_replacement_aborted(aborted: tokio::sync::oneshot::Sender<()>) {
    *REPLACEMENT_ABORTED.lock().unwrap() = Some(aborted);
}

pub fn mark_replacement_aborted() {
    let aborted = REPLACEMENT_ABORTED.lock().unwrap().take();
    if let Some(aborted) = aborted {
        let _ = aborted.send(());
    }
}

pub fn arm_baseline_check_pause(
    entered: tokio::sync::oneshot::Sender<bool>,
    resume: std::sync::mpsc::Receiver<()>,
) {
    *BASELINE_CHECK_PAUSE.lock().unwrap() = Some(BaselineCheckPause { entered, resume });
}

pub fn pause_baseline_check(ready: bool) {
    let pause = BASELINE_CHECK_PAUSE.lock().unwrap().take();
    if let Some(pause) = pause {
        pause.entered.send(ready).unwrap();
        tokio::task::block_in_place(|| {
            pause
                .resume
                .recv_timeout(Duration::from_secs(5))
                .expect("baseline check pause was not released");
        });
    }
}

pub fn arm_baseline_adopted(entered: tokio::sync::oneshot::Sender<()>) {
    *BASELINE_ADOPTED.lock().unwrap() = Some(entered);
}

pub fn mark_baseline_adopted() {
    let entered = BASELINE_ADOPTED.lock().unwrap().take();
    if let Some(entered) = entered {
        let _ = entered.send(());
    }
}

async fn wait_for_marker(path: &Path, expected: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_to_string(path).is_ok_and(|value| value == expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "fixture marker {} did not become {expected}",
            path.display()
        )
    });
}

async fn wait_for_baseline(wire: &mut BufStream<DuplexStream>) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = call(wire, "get_new_diagnostics", json!({})).await;
            assert!(
                !response.to_string().contains("rust-generation-2"),
                "replacement startup diagnostics were delivered before the baseline: {response}"
            );
            let text = response["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default();
            if !text.contains("\"note\"") {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("diagnostics baseline was not adopted")
}

#[derive(Clone, Copy)]
enum ReplacementAttempt {
    Succeeds,
    FailsInitialization,
    CancelledBeforeRetirement,
    CancelledAfterRetirement,
}

#[derive(Clone, Copy)]
enum ReplacementOwnerOrder {
    BeforeInitialOwnerInstallation,
    AfterInitialOwnerInstallation,
}

#[tokio::test(flavor = "multi_thread")]
async fn i1_t5_replacement_before_owner_installation_keeps_new_grace() {
    replacement_owner_scenario(
        ReplacementOwnerOrder::BeforeInitialOwnerInstallation,
        ReplacementAttempt::Succeeds,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn i1_t5_replacement_after_owner_installation_keeps_new_grace() {
    replacement_owner_scenario(
        ReplacementOwnerOrder::AfterInitialOwnerInstallation,
        ReplacementAttempt::Succeeds,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i1_t5_failed_replacement_does_not_hold_baseline() {
    replacement_owner_scenario(
        ReplacementOwnerOrder::AfterInitialOwnerInstallation,
        ReplacementAttempt::FailsInitialization,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i1_t5_cancelled_request_keeps_replacement_owned_until_install_finishes() {
    replacement_owner_scenario(
        ReplacementOwnerOrder::AfterInitialOwnerInstallation,
        ReplacementAttempt::CancelledBeforeRetirement,
    )
    .await;
    replacement_owner_scenario(
        ReplacementOwnerOrder::AfterInitialOwnerInstallation,
        ReplacementAttempt::CancelledAfterRetirement,
    )
    .await;
}

#[allow(clippy::too_many_lines)]
async fn replacement_owner_scenario(order: ReplacementOwnerOrder, attempt: ReplacementAttempt) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let rust = root.join("main.rs");
        let python = root.join("other.py");
        std::fs::write(&rust, "fn main() {}\n").unwrap();
        std::fs::write(&python, "pass\n").unwrap();
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/notification_generations.py"
        );
        let mut config = ServerConfig::default();
        config.workspace.roots = vec![root.clone()];
        config.diagnostics.hooks.enabled = false;
        config.diagnostics.settle_quiet_ms = 50;
        config.diagnostics.settle_deadline_ms = 10000;
        let (rust_mode, rust_startup_delay, rust_initialize_delay) = match attempt {
            ReplacementAttempt::Succeeds => ("reporting-then-silent", "1.5", "0"),
            ReplacementAttempt::CancelledAfterRetirement => ("reporting-then-silent", "0", "0"),
            ReplacementAttempt::FailsInitialization => ("fail-initialize", "0", "0"),
            ReplacementAttempt::CancelledBeforeRetirement => ("reporting-then-silent", "0", "3"),
        };
        config.lsp_servers = [
            (
                "rust",
                &rust,
                rust_mode,
                rust_startup_delay,
                rust_initialize_delay,
            ),
            ("python", &python, "reporting", "0", "0"),
        ]
        .into_iter()
        .map(|(language, path, mode, startup_delay, initialize_delay)| {
            let control = root.join(language);
            let initialized_marker = root.join(format!("{language}.initialized"));
            let published_marker = root.join(format!("{language}.published"));
            let initialize_attempt_marker = root.join(format!("{language}.initialize-attempt"));
            serde_json::from_value(json!({
                "language_id": language,
                "command": "python3",
                "args": [fixture, control, bridge::path_to_uri(path).unwrap().as_str(), language,
                    mode, startup_delay, initialize_delay, initialized_marker, published_marker,
                    initialize_attempt_marker],
                "timeout_seconds": 5,
                "request_timeout_seconds": 2
            }))
            .unwrap()
        })
        .collect();
        let watch = Arc::new(lsp::WatchRegistry::new());
        let configs = applicable_server_configs(&config, std::slice::from_ref(&root), None, &watch);
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let translator = build_translator(
            &config,
            vec![root.clone()],
            HashMap::from([("rs".into(), "rust".into()), ("py".into(), "python".into())]),
            ToolRouter::from_configs(&config.lsp_servers).unwrap(),
            Arc::clone(&cache),
            watch,
        );
        register_starting_configs(&translator, &configs);
        let owner_pause = if matches!(order, ReplacementOwnerOrder::AfterInitialOwnerInstallation) {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            arm_owner_installation_pause(entered_tx, resume_rx);
            Some((entered_rx, resume_tx))
        } else {
            None
        };
        let before_owner_pause =
            if matches!(order, ReplacementOwnerOrder::BeforeInitialOwnerInstallation) {
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (resume_tx, resume_rx) = std::sync::mpsc::channel();
                arm_before_owner_installation_pause(entered_tx, resume_rx);
                Some((entered_rx, resume_tx))
            } else {
                None
            };
        let retirement_pause = if matches!(attempt, ReplacementAttempt::Succeeds) {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            arm_retirement_pause(entered_tx, resume_rx);
            Some((entered_rx, resume_tx))
        } else {
            None
        };
        let async_retirement_pause =
            if matches!(attempt, ReplacementAttempt::CancelledAfterRetirement) {
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
                arm_async_retirement_pause(entered_tx, resume_rx);
                Some((entered_rx, resume_tx))
            } else {
                None
            };
        let mut cancellation_ack = if matches!(
            attempt,
            ReplacementAttempt::CancelledBeforeRetirement
                | ReplacementAttempt::CancelledAfterRetirement
        ) {
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            arm_workspace_request_cancellation(ack_tx);
            Some(ack_rx)
        } else {
            None
        };
        let mut replacement_aborted = if matches!(
            attempt,
            ReplacementAttempt::CancelledBeforeRetirement
                | ReplacementAttempt::CancelledAfterRetirement
        ) {
            let (aborted_tx, aborted_rx) = tokio::sync::oneshot::channel();
            arm_replacement_aborted(aborted_tx);
            Some(aborted_rx)
        } else {
            None
        };
        let (baseline_check_entered, baseline_check_resume) = {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            arm_baseline_check_pause(entered_tx, resume_rx);
            (entered_rx, resume_tx)
        };
        let baseline_adopted = {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            arm_baseline_adopted(entered_tx);
            entered_rx
        };
        let subs = Arc::new(ResourceSubscriptions::new());
        let settle = Arc::new(bridge::ServerSettle::new(
            Duration::from_millis(config.diagnostics.settle_quiet_ms),
            Duration::from_millis(config.diagnostics.settle_deadline_ms),
        ));
        let delivery = Arc::new(Mutex::new(bridge::DiagnosticsDelivery::new(
            config.diagnostics,
        )));
        let floors = Arc::new(bridge::FloorTable::new(
            &config.diagnostics,
            &config.lsp_servers,
        ));
        let shared = PumpShared {
            notification_cache: Arc::clone(&cache),
            subs: Arc::clone(&subs),
            workspace_roots: Arc::from(vec![root.clone()]),
            document_tracker: Arc::clone(translator.document_tracker()),
            settle: Arc::clone(&settle),
            delivery: Arc::clone(&delivery),
            floors: Arc::clone(&floors),
        };
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        translator.notification_pumps.get_or_init(|| {
            notification_lifecycle::NotificationPumps::new(shared.clone(), cancel_rx.clone())
        });
        let init =
            spawn_lsp_servers_background(configs, Arc::clone(&translator), cancel_rx, shared);
        let abort_init = AbortOnDrop(&init);
        let server = mcp::McplsServer::new(
            Arc::clone(&translator),
            cache,
            Arc::from(vec![root.clone()]),
            subs,
            false,
            Arc::clone(&delivery),
            floors,
            config.diagnostics,
            settle,
        );
        let (server_io, client_io) = tokio::io::duplex(65_536);
        let started = tokio::spawn(async move { server.serve(server_io).await.unwrap() });
        let mut wire = BufStream::new(client_io);
        let initialized = request(
            &mut wire,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": {"name": "recovery-test", "version": "1"}}}),
        )
        .await;
        assert!(initialized["result"].is_object(), "{initialized}");
        wire.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        wire.flush().await.unwrap();
        let running = started.await.unwrap();

        let rust_control = root.join("rust");
        let rust_initialized = root.join("rust.initialized");
        let rust_published = root.join("rust.published");
        let python_initialized = root.join("python.initialized");
        let python_published = root.join("python.published");
        if let Some((entered, resume)) = before_owner_pause {
            let (retirement_entered, retirement_resume) = retirement_pause.unwrap();
            tokio::time::timeout(Duration::from_secs(5), entered)
                .await
                .unwrap()
                .unwrap();
            wait_for_marker(&rust_initialized, "rust-generation-1").await;
            wait_for_marker(&rust_published, "rust-generation-1").await;
            wait_for_marker(&python_initialized, "python-generation-1").await;
            wait_for_marker(&python_published, "python-generation-1").await;
            std::fs::write(root.join("rust.crash-1"), "").unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !translator.is_server_dead(&ServerId::from("rust")) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let pending = tokio::spawn(async move {
                let response = call(
                    &mut wire,
                    "workspace_symbol_search",
                    json!({"query": "generation"}),
                )
                .await;
                (wire, response)
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while !std::fs::read_to_string(rust_control.with_extension("generation"))
                    .is_ok_and(|value| value == "2")
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), retirement_entered)
                .await
                .unwrap()
                .unwrap();
            resume.send(()).unwrap();
            let first_check = tokio::time::timeout(Duration::from_secs(5), baseline_check_entered)
                .await
                .unwrap()
                .unwrap();
            assert!(!first_check);
            baseline_check_resume.send(()).unwrap();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            arm_baseline_check_pause(entered_tx, resume_rx);
            let second_check = tokio::time::timeout(Duration::from_secs(5), entered_rx)
                .await
                .unwrap()
                .unwrap();
            assert!(!second_check);
            resume_tx.send(()).unwrap();
            retirement_resume.send(()).unwrap();
            let (new_wire, response) = pending.await.unwrap();
            wire = new_wire;
            assert!(
                response.to_string().contains("rust-generation-2"),
                "{response}"
            );
        } else {
            let (entered, resume) = owner_pause.unwrap();
            tokio::time::timeout(Duration::from_secs(5), entered)
                .await
                .unwrap()
                .unwrap();
            wait_for_marker(&rust_initialized, "rust-generation-1").await;
            wait_for_marker(&rust_published, "rust-generation-1").await;
            wait_for_marker(&python_initialized, "python-generation-1").await;
            wait_for_marker(&python_published, "python-generation-1").await;
            std::fs::write(root.join("rust.crash-1"), "").unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !translator.is_server_dead(&ServerId::from("rust")) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let (cancellation_trigger, cancellation) = if matches!(
                attempt,
                ReplacementAttempt::CancelledBeforeRetirement
                    | ReplacementAttempt::CancelledAfterRetirement
            ) {
                let (trigger_tx, trigger_rx) = tokio::sync::oneshot::channel();
                (
                    Some(trigger_tx),
                    Some((trigger_rx, cancellation_ack.take().unwrap())),
                )
            } else {
                (None, None)
            };
            let mut cancellation_retirement_resume = None;
            let pending = tokio::spawn(pending_workspace_call(wire, cancellation));
            if matches!(
                attempt,
                ReplacementAttempt::CancelledBeforeRetirement
                    | ReplacementAttempt::CancelledAfterRetirement
            ) {
                let cancellation_trigger = cancellation_trigger.unwrap();
                if matches!(attempt, ReplacementAttempt::CancelledBeforeRetirement) {
                    wait_for_marker(&root.join("rust.initialize-attempt"), "rust-generation-2")
                        .await;
                    cancellation_trigger.send(()).unwrap();
                } else {
                    let (entered, resume_retirement) = async_retirement_pause.unwrap();
                    tokio::time::timeout(Duration::from_secs(5), entered)
                        .await
                        .unwrap()
                        .unwrap();
                    cancellation_trigger.send(()).unwrap();
                    cancellation_retirement_resume = Some(resume_retirement);
                }
                let (new_wire, response) = tokio::time::timeout(Duration::from_secs(5), pending)
                    .await
                    .unwrap()
                    .unwrap();
                wire = new_wire;
                assert!(
                    response.is_none(),
                    "cancelled workspace call returned: {response:?}"
                );
                let replacement_aborted = tokio::time::timeout(
                    Duration::from_millis(250),
                    replacement_aborted.take().unwrap(),
                )
                .await
                .is_ok();
                assert!(
                    !replacement_aborted,
                    "cancelling the workspace request must not abort the detached replacement"
                );
                let rust_id = ServerId::from("rust");
                assert_eq!(
                    translator.lifecycle_of(&rust_id),
                    Some(bridge::ServerLifecycle::Starting),
                    "the replacement remains active after its waiter is cancelled"
                );
                resume.send(()).unwrap();
                let baseline_ready =
                    tokio::time::timeout(Duration::from_secs(5), baseline_check_entered)
                        .await
                        .unwrap()
                        .unwrap();
                assert!(
                    !baseline_ready,
                    "an active replacement keeps the diagnostics baseline pending"
                );
                baseline_check_resume.send(()).unwrap();

                if let Some(resume_retirement) = cancellation_retirement_resume {
                    resume_retirement.send(()).unwrap();
                }
                tokio::time::timeout(Duration::from_secs(5), async {
                    while translator.lifecycle_of(&rust_id)
                        != Some(bridge::ServerLifecycle::Running)
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                tokio::time::timeout(Duration::from_secs(5), baseline_adopted)
                    .await
                    .unwrap()
                    .unwrap();
                let baseline = call(&mut wire, "get_new_diagnostics", json!({})).await;
                assert!(
                    !baseline.to_string().contains("rust-generation-2"),
                    "{baseline}"
                );
            } else if matches!(attempt, ReplacementAttempt::FailsInitialization) {
                resume.send(()).unwrap();
                wait_for_marker(&root.join("rust.initialize-attempt"), "rust-generation-2").await;
                let (new_wire, response) = tokio::time::timeout(Duration::from_secs(5), pending)
                    .await
                    .unwrap()
                    .unwrap();
                wire = new_wire;
                let response = response.unwrap();
                assert!(response["error"].is_object(), "{response}");
                let baseline_ready =
                    tokio::time::timeout(Duration::from_secs(5), baseline_check_entered)
                        .await
                        .unwrap()
                        .unwrap();
                assert!(baseline_ready, "failed replacement must not hold baseline");
                baseline_check_resume.send(()).unwrap();
                let baseline = call(&mut wire, "get_new_diagnostics", json!({})).await;
                assert!(
                    !baseline.to_string().contains("rust-generation-2"),
                    "{baseline}"
                );
            } else {
                let (retirement_entered, retirement_resume) = retirement_pause.unwrap();
                tokio::time::timeout(Duration::from_secs(5), retirement_entered)
                    .await
                    .unwrap()
                    .unwrap();
                resume.send(()).unwrap();
                let baseline_ready =
                    tokio::time::timeout(Duration::from_secs(5), baseline_check_entered)
                        .await
                        .unwrap()
                        .unwrap();
                baseline_check_resume.send(()).unwrap();
                if baseline_ready {
                    tokio::time::timeout(Duration::from_secs(5), baseline_adopted)
                        .await
                        .unwrap()
                        .unwrap();
                }
                retirement_resume.send(()).unwrap();
                let (new_wire, response) = pending.await.unwrap();
                wire = new_wire;
                assert!(
                    response
                        .as_ref()
                        .unwrap()
                        .to_string()
                        .contains("rust-generation-2"),
                    "{response:?}"
                );
            }
        }

        if matches!(attempt, ReplacementAttempt::Succeeds) {
            wait_for_marker(&rust_published, "rust-generation-2").await;
            let baseline = wait_for_baseline(&mut wire).await;
            assert!(
                !baseline.to_string().contains("rust-generation-2"),
                "replacement startup diagnostics must be adopted by the baseline: {baseline}"
            );
            let after_startup = call(&mut wire, "get_new_diagnostics", json!({})).await;
            assert!(
                !after_startup.to_string().contains("rust-generation-2"),
                "replacement startup diagnostics must be adopted by the baseline: {after_startup}"
            );
        }
        running.cancel().await.unwrap();
        cancel_tx.send(true).unwrap();
        translator.shutdown_servers().await;
        drop(abort_init);
        init.await.unwrap();
    })
    .await
    .expect("MCP replacement owner scenario exceeded deadline");
}

#[derive(Clone, Copy)]
enum StartupMode {
    Ready,
    CacheLocked,
    BetweenPublications,
}

async fn request(wire: &mut BufStream<DuplexStream>, value: Value) -> Value {
    wire.write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
    wire.flush().await.unwrap();
    loop {
        let mut line = String::new();
        assert_ne!(wire.read_line(&mut line).await.unwrap(), 0);
        let response: Value = serde_json::from_str(&line).unwrap();
        if response["id"] == value["id"] {
            return response;
        }
    }
}

async fn call(wire: &mut BufStream<DuplexStream>, name: &str, arguments: Value) -> Value {
    request(
        wire,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}}),
    )
    .await
}

async fn pending_workspace_call(
    mut wire: BufStream<DuplexStream>,
    cancellation: Option<(
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
) -> (BufStream<DuplexStream>, Option<Value>) {
    if let Some((cancel, cancelled)) = cancellation {
        wire.write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"workspace_symbol_search\",\"arguments\":{\"query\":\"generation\"}}}\n",
        )
        .await
        .unwrap();
        wire.flush().await.unwrap();
        cancel.await.unwrap();
        wire.write_all(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":2,\"reason\":\"recovery test\"}}\n",
        )
        .await
        .unwrap();
        wire.flush().await.unwrap();
        cancelled.await.unwrap();
        (wire, None)
    } else {
        let response = call(
            &mut wire,
            "workspace_symbol_search",
            json!({"query": "generation"}),
        )
        .await;
        (wire, Some(response))
    }
}

async fn wait_cached(wire: &mut BufStream<DuplexStream>, path: &Path, sentinel: &str) {
    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = call(wire, "get_cached_diagnostics", json!({"file_path": path})).await;
            if response.to_string().contains(sentinel) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        observed.is_ok(),
        "replacement push diagnostic {sentinel} was not available through MCP"
    );
}

#[tokio::test]
async fn recovery_mcp_preserves_push_across_replacements() {
    recovery_scenario(StartupMode::Ready).await;
}

#[tokio::test]
async fn recovery_mcp_replacement_during_startup_keeps_new_pump() {
    recovery_scenario(StartupMode::CacheLocked).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_workspace_symbols_during_startup_preserve_live_client() {
    recovery_scenario(StartupMode::BetweenPublications).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn lazy_server_triggered_during_initial_batch_spawns_once() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let control = root.join("control");
    std::fs::create_dir(&control).unwrap();
    let script = root.join("fake_lsp.py");
    let python_file = root.join("main.py");
    let typescript_file = root.join("main.ts");
    std::fs::write(&python_file, "pass\n").unwrap();
    std::fs::write(&typescript_file, "const value = 1;\n").unwrap();
    let symbol_uri = bridge::path_to_uri(&python_file).unwrap().to_string();
    std::fs::write(
        &script,
        r#"import json, os, pathlib, sys, time

control = pathlib.Path(sys.argv[1])
role = sys.argv[2]
symbol_uri = sys.argv[3]
with (control / f"{role}.pids").open("a") as count:
    count.write(f"{os.getpid()}\n")
    count.flush()

def receive():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        key, value = line.decode().split(":", 1)
        if key.lower() == "content-length":
            length = int(value.strip())
    return json.loads(sys.stdin.buffer.read(length))

def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()

while True:
    message = receive()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        if role == "eager":
            (control / "eager.initializing").write_text(str(os.getpid()))
            while not (control / "eager.release").exists():
                time.sleep(0.01)
        send({"jsonrpc": "2.0", "id": message["id"], "result": {
            "capabilities": {"positionEncoding": "utf-16", "workspaceSymbolProvider": True}
        }})
    elif method == "workspace/symbol":
        send({"jsonrpc": "2.0", "id": message["id"], "result": [{
            "name": f"{role}-{os.getpid()}",
            "kind": 12,
            "location": {"uri": symbol_uri, "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 1}
            }},
            "containerName": role
        }]})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "exit":
        break
"#,
    )
    .unwrap();

    let make_server =
        |mut config: crate::config::LspServerConfig, role: &str, spawn: SpawnPolicy| {
            config.command = "python3".to_string();
            config.args = vec![
                script.to_string_lossy().into_owned(),
                control.to_string_lossy().into_owned(),
                role.to_string(),
                symbol_uri.clone(),
            ];
            config.heuristics = None;
            config.spawn = Some(spawn);
            config
        };
    let mut eager = make_server(
        crate::config::LspServerConfig::rust_analyzer(),
        "eager",
        SpawnPolicy::Eager,
    );
    eager.handles = Some(vec![crate::config::ToolKind::Hover]);
    let lazy = make_server(
        crate::config::LspServerConfig::pyright(),
        "lazy",
        SpawnPolicy::Lazy,
    );
    let mut untouched_lazy = make_server(
        crate::config::LspServerConfig::typescript(),
        "untouched-lazy",
        SpawnPolicy::Lazy,
    );
    untouched_lazy.handles = Some(vec![crate::config::ToolKind::Hover]);

    let mut config = ServerConfig::default();
    config.workspace.roots = vec![root.clone()];
    config.diagnostics.hooks.enabled = false;
    config.lsp_servers = vec![eager, lazy, untouched_lazy];
    let runtime = Runtime::start(&config, Ok(root.clone())).await.unwrap();

    let eager_started = tokio::time::timeout(Duration::from_secs(5), async {
        while !control.join("eager.initializing").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let first_symbols = if eager_started.is_ok() {
        tokio::time::timeout(
            Duration::from_secs(5),
            runtime
                .translator
                .handle_workspace_symbol("race".to_string(), None, 10),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map(|result| {
            result
                .symbols
                .into_iter()
                .map(|symbol| symbol.name)
                .collect::<Vec<_>>()
        })
    } else {
        None
    };

    std::fs::write(control.join("eager.release"), "release").unwrap();
    let eager_running = tokio::time::timeout(Duration::from_secs(10), async {
        while runtime.translator.lifecycle_of(&ServerId::from("rust"))
            != Some(ServerLifecycle::Running)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    let second_symbols = if eager_running {
        tokio::time::timeout(
            Duration::from_secs(5),
            runtime
                .translator
                .handle_workspace_symbol("race".to_string(), None, 10),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map(|result| {
            result
                .symbols
                .into_iter()
                .map(|symbol| symbol.name)
                .collect::<Vec<_>>()
        })
    } else {
        None
    };
    let untouched_lazy_error = if eager_running {
        tokio::time::timeout(
            Duration::from_secs(2),
            runtime
                .translator
                .handle_hover(typescript_file.to_string_lossy().into_owned(), 1, 1),
        )
        .await
        .ok()
        .and_then(Result::err)
    } else {
        None
    };
    let lazy_processes = std::fs::read_to_string(control.join("lazy.pids"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    runtime.shutdown().await;

    assert!(eager_started.is_ok(), "initial eager spawn did not pause");
    assert!(eager_running, "initial eager batch did not finish");
    assert_eq!(
        lazy_processes.len(),
        1,
        "lazy spawn count: {lazy_processes:?}"
    );
    assert_eq!(
        first_symbols, second_symbols,
        "lazy client changed during batch registration"
    );
    assert!(matches!(
        untouched_lazy_error,
        Some(Error::ServerInitializing { server_id })
            if server_id == ServerId::from("typescript")
    ));
}

#[tokio::test]
async fn all_lazy_runtime_seeds_baseline_and_pumps_without_initial_batch() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    let source = root.join("main.py");
    std::fs::write(&source, "pass\n").unwrap();
    let control = root.join("python");
    let generation = root.join("python.generation");
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/notification_generations.py"
    );
    let mut lazy = crate::config::LspServerConfig::pyright();
    lazy.command = "python3".to_string();
    lazy.args = vec![
        fixture.to_string(),
        control.to_string_lossy().into_owned(),
        bridge::path_to_uri(&source).unwrap().to_string(),
        "python".to_string(),
        "default".to_string(),
    ];
    lazy.heuristics = None;

    let mut config = ServerConfig::default();
    config.workspace.roots = vec![root.clone()];
    config.backend.spawn = SpawnPolicy::Lazy;
    config.diagnostics.hooks.enabled = false;
    config.lsp_servers = vec![lazy];

    let runtime = Runtime::start(&config, Ok(root)).await.unwrap();
    let no_initial_batch = runtime.lsp_init_handle.is_none();
    let pumps_initialized = runtime.translator.notification_pumps.get().is_some();
    let empty_baseline = runtime.context.delivery.lock().await.has_baseline();
    let no_spawn_before_trigger = !generation.exists();
    let symbols = tokio::time::timeout(
        Duration::from_secs(5),
        runtime
            .translator
            .handle_workspace_symbol("lazy".to_string(), None, 10),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .map(|result| {
        result
            .symbols
            .into_iter()
            .map(|symbol| symbol.name)
            .collect::<Vec<_>>()
    });
    runtime.shutdown().await;

    assert!(no_initial_batch);
    assert!(pumps_initialized);
    assert!(empty_baseline);
    assert!(no_spawn_before_trigger);
    assert_eq!(symbols, Some(vec!["python-generation-1".to_string()]));
    assert_eq!(std::fs::read_to_string(generation).unwrap(), "1");
}

#[allow(clippy::too_many_lines)]
async fn recovery_scenario(startup: StartupMode) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let rust = root.join("main.rs");
        let python = root.join("other.py");
        std::fs::write(&rust, "fn main() {}\n").unwrap();
        std::fs::write(&python, "pass\n").unwrap();
        let mut config = ServerConfig::default();
        config.workspace.roots = vec![root.clone()];
        config.diagnostics.hooks.enabled = false;
        config.lsp_servers = [("rust", &rust), ("python", &python)].into_iter().map(|(language, path)| {
            serde_json::from_value(json!({"language_id": language, "command": "python3",
                "args": [concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/notification_generations.py"),
                    root.join(language), bridge::path_to_uri(path).unwrap().as_str(), language],
                "timeout_seconds": 5, "request_timeout_seconds": 2})).unwrap()
        }).collect();
        if matches!(startup, StartupMode::BetweenPublications) {
            config.lsp_servers.retain(|server| server.language_id == "rust");
        }
        let cache = Arc::new(Mutex::new(NotificationCache::new()));
        let startup_cache = Arc::clone(&cache);
        let mut held_cache = if matches!(startup, StartupMode::CacheLocked) { Some(startup_cache.lock().await) } else { None };
        let watch = Arc::new(lsp::WatchRegistry::new());
        let configs = applicable_server_configs(&config, std::slice::from_ref(&root), None, &watch);
        let translator = build_translator(&config, vec![root.clone()],
            HashMap::from([("rs".into(), "rust".into()), ("py".into(), "python".into())]),
            ToolRouter::from_configs(&config.lsp_servers).unwrap(), Arc::clone(&cache), watch);
        register_starting_configs(&translator, &configs);
        let publication_pause = if matches!(startup, StartupMode::BetweenPublications) {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            *bridge::lock_std(&translator.registration_pause) = Some(RegistrationPause {
                server_id: ServerId::from("rust"), entered: entered_tx, resume: resume_rx,
            });
            Some((entered_rx, resume_tx))
        } else { None };
        let subs = Arc::new(ResourceSubscriptions::new());
        let settle = Arc::new(bridge::ServerSettle::new(Duration::from_millis(config.diagnostics.settle_quiet_ms),
            Duration::from_millis(config.diagnostics.settle_deadline_ms)));
        let delivery = Arc::new(Mutex::new(bridge::DiagnosticsDelivery::new(config.diagnostics)));
        let floors = Arc::new(bridge::FloorTable::new(&config.diagnostics, &config.lsp_servers));
        let shared = PumpShared { notification_cache: Arc::clone(&cache), subs: Arc::clone(&subs),
            workspace_roots: Arc::from(vec![root.clone()]),
            document_tracker: Arc::clone(translator.document_tracker()), settle: Arc::clone(&settle),
            delivery: Arc::clone(&delivery), floors: Arc::clone(&floors) };
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        translator.notification_pumps.get_or_init(|| {
            notification_lifecycle::NotificationPumps::new(shared.clone(), cancel_rx.clone())
        });
        let init = spawn_lsp_servers_background(
            configs,
            Arc::clone(&translator),
            cancel_rx,
            shared,
        );
        let abort_init = AbortOnDrop(&init);
        let server = mcp::McplsServer::new(Arc::clone(&translator), cache, Arc::from(vec![root.clone()]),
            subs, false, Arc::clone(&delivery), floors, config.diagnostics, Arc::clone(&settle));
        let (server_io, client_io) = tokio::io::duplex(65_536);
        let started = tokio::spawn(async move { server.serve(server_io).await.unwrap() });
        let mut wire = BufStream::new(client_io);
        let initialized = request(&mut wire, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "recovery-test", "version": "1"}}})).await;
        assert!(initialized["result"].is_object(), "{initialized}");
        wire.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n").await.unwrap();
        wire.flush().await.unwrap();
        let running = started.await.unwrap();
        if let Some((entered, resume)) = publication_pause {
            let original = tokio::time::timeout(Duration::from_secs(5), entered).await.unwrap().unwrap();
            std::fs::write(root.join("rust.crash-1"), "").unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while original.notify("test/probe", json!({})).await.is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if translator.registered_server_count() != 0 {
                    while !translator.is_server_dead(&ServerId::from("rust")) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }).await.expect("original fixture must exit while startup is paused");
            let _concurrent = call(&mut wire, "workspace_symbol_search", json!({"query": "generation"})).await;
            resume.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !delivery.lock().await.has_baseline() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            for _ in 0..2 {
                let response = call(&mut wire, "workspace_symbol_search", json!({"query": "generation"})).await;
                assert_eq!(response["result"]["isError"], false, "startup must preserve the live replacement client: {response}");
                assert!(response.to_string().contains("rust-generation-2"), "{response}");
            }
            running.cancel().await.unwrap();
            cancel_tx.send(true).unwrap();
            translator.shutdown_servers().await;
            drop(abort_init);
            init.await.unwrap();
            return;
        }
        let first_generation = if let Some(held_cache) = held_cache.take() {
            tokio::time::timeout(Duration::from_secs(5), async {
                while translator.registered_server_count() != 2 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            std::fs::write(root.join("rust.crash-1"), "").unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !translator.is_server_dead(&ServerId::from("rust")) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            let request_path = rust.clone();
            let pending = tokio::spawn(async move {
                loop {
                    let result = call(&mut wire, "get_diagnostics", json!({"file_path": request_path})).await;
                    if result["result"]["isError"] == false {
                        return wire;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while std::fs::read_to_string(root.join("rust.generation")).unwrap() != "2" {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            drop(held_cache);
            wire = pending.await.unwrap();
            2
        } else { 1 };
        drop(held_cache);
        wait_cached(&mut wire, &rust, &format!("rust-generation-{first_generation}")).await;
        wait_cached(&mut wire, &python, "python-generation-1").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !delivery.lock().await.has_baseline() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        wait_for_owner_baseline(&settle, &ServerId::from("rust")).await;
        for generation in first_generation..first_generation + 2 {
            tokio::time::sleep(Duration::from_millis(1100)).await;
            std::fs::write(root.join(format!("rust.crash-{generation}")), "").unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let _ = call(&mut wire, "get_diagnostics", json!({"file_path": rust})).await;
                    if std::fs::read_to_string(root.join("rust.generation")).unwrap() == (generation + 1).to_string() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
            let sentinel = format!("rust-generation-{}", generation + 1);
            wait_cached(&mut wire, &rust, &sentinel).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let logs = call(&mut wire, "get_server_logs", json!({})).await;
                    if logs.to_string().contains(&sentinel) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.expect("replacement log sentinel must reach the public MCP tool");
            wait_cached(&mut wire, &python, "python-generation-1").await;
            wait_for_owner_baseline(&settle, &ServerId::from("rust")).await;
            let first = call(&mut wire, "get_new_diagnostics", json!({})).await;
            assert!(first.to_string().contains(&sentinel), "{first}");
            let second = call(&mut wire, "get_new_diagnostics", json!({})).await;
            assert!(!second.to_string().contains(&sentinel), "{second}");
        }
        running.cancel().await.unwrap();
        cancel_tx.send(true).unwrap();
        translator.shutdown_servers().await;
        drop(abort_init);
        init.await.unwrap();
    }).await.expect("MCP recovery scenario exceeded deadline");
}
