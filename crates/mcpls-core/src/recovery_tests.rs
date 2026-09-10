#![allow(clippy::unwrap_used, clippy::expect_used)]

use rmcp::ServiceExt as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufStream, DuplexStream};

use super::*;

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
        let translator = Arc::new(build_translator(&config, vec![root.clone()],
            HashMap::from([("rs".into(), "rust".into()), ("py".into(), "python".into())]),
            ToolRouter::from_configs(&config.lsp_servers).unwrap(), Arc::clone(&cache), watch));
        let publication_pause = if matches!(startup, StartupMode::BetweenPublications) {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            *bridge::lock_std(&translator.registration_pause) = Some(RegistrationPause {
                server_id: ServerId::from("rust"), entered: entered_tx, resume: resume_rx,
            });
            Some((entered_rx, resume_tx))
        } else { None };
        let subs = Arc::new(ResourceSubscriptions::new());
        let peer_cell = Arc::new(OnceCell::new());
        let settle = Arc::new(bridge::ServerSettle::new(Duration::from_millis(config.diagnostics.settle_quiet_ms),
            Duration::from_millis(config.diagnostics.settle_deadline_ms)));
        let delivery = Arc::new(Mutex::new(bridge::DiagnosticsDelivery::new(config.diagnostics)));
        let floors = Arc::new(bridge::FloorTable::new(&config.diagnostics, &config.lsp_servers));
        let shared = PumpShared { notification_cache: Arc::clone(&cache), subs: Arc::clone(&subs),
            peer_cell: Arc::clone(&peer_cell), workspace_roots: Arc::from(vec![root.clone()]),
            document_tracker: Arc::clone(translator.document_tracker()), settle: Arc::clone(&settle),
            delivery: Arc::clone(&delivery), floors: Arc::clone(&floors) };
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let init = spawn_lsp_servers_background(configs, Arc::clone(&translator), cancel_rx, shared);
        let abort_init = AbortOnDrop(&init);
        let server = mcp::McplsServer::new(Arc::clone(&translator), cache, Arc::from(vec![root.clone()]),
            subs, false, Arc::clone(&delivery), floors, config.diagnostics, settle);
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
        peer_cell.set(running.peer().clone()).unwrap();
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
