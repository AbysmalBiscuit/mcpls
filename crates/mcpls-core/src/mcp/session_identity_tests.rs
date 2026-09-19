use rmcp::ServiceExt as _;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt as _, BufStream, DuplexStream};

use super::tests::{mcp_test_request, server_with_one_error};
use super::*;

struct Client {
    wire: BufStream<DuplexStream>,
    running: rmcp::service::RunningService<RoleServer, McplsServer>,
}

impl Client {
    async fn connect(server: McplsServer, name: &str) -> Self {
        let (server_io, client_io) = tokio::io::duplex(65_536);
        let started = tokio::spawn(async move { server.serve(server_io).await.unwrap() });
        let mut wire = BufStream::new(client_io);
        let response = mcp_test_request(
            &mut wire,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": name, "version": "0.155.1"}}
            }),
        )
        .await;
        assert!(response["result"].is_object(), "{response}");
        wire.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        wire.flush().await.unwrap();
        Self {
            wire,
            running: started.await.unwrap(),
        }
    }

    async fn call(&mut self, name: &str, meta: Option<Value>) -> Value {
        let mut params = json!({"name": name, "arguments": {}});
        if let Some(meta) = meta {
            params["_meta"] = meta;
        }
        let response = mcp_test_request(
            &mut self.wire,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": params
            }),
        )
        .await;
        assert!(response.get("error").is_none(), "{response}");
        assert_ne!(response["result"]["isError"], true, "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    async fn diagnostics(&mut self, meta: impl Into<Option<Value>>) -> Value {
        self.call("get_new_diagnostics", meta.into()).await
    }

    async fn close(self) {
        self.running.cancel().await.unwrap();
    }
}

fn metadata(thread: &str) -> Value {
    json!({"threadId": thread, "x-codex-turn-metadata": {
        "thread_id": thread, "session_id": "root"
    }})
}

fn changed(result: &Value) -> usize {
    result["changed"].as_array().unwrap().len()
}

#[tokio::test]
async fn codex_adopts_anonymous_history_only_once() {
    let server = server_with_one_error().await;
    let mut client = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    assert_eq!(changed(&client.diagnostics(metadata("first")).await), 1);
    assert_eq!(changed(&client.diagnostics(None).await), 1);
    assert_eq!(changed(&client.diagnostics(metadata("second")).await), 1);
    assert_eq!(changed(&client.diagnostics(None).await), 0);
    client.close().await;
}

#[tokio::test]
async fn codex_invalid_top_level_thread_uses_valid_nested_thread() {
    let server = server_with_one_error().await;
    let mut client = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    assert_eq!(changed(&client.diagnostics(metadata("thread")).await), 1);
    for invalid in [json!(""), json!(42), json!(null)] {
        let meta =
            Some(json!({"threadId": invalid, "x-codex-turn-metadata": {"thread_id": "thread"}}));
        assert_eq!(changed(&client.diagnostics(meta).await), 0);
    }
    client.close().await;
}

#[tokio::test]
async fn codex_merges_anonymous_history_with_an_existing_thread_record() {
    let server = server_with_one_error().await;
    let mut existing = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    assert_eq!(changed(&existing.diagnostics(metadata("thread")).await), 1);
    let first_uri: Uri = if cfg!(windows) {
        "file:///C:/workspace/broken.rs"
    } else {
        "file:///workspace/broken.rs"
    }
    .parse()
    .unwrap();
    let second_uri: Uri = if cfg!(windows) {
        "file:///C:/workspace/second.rs"
    } else {
        "file:///workspace/second.rs"
    }
    .parse()
    .unwrap();
    let error = lsp_types::Diagnostic {
        severity: Some(lsp_types::DiagnosticSeverity::ERROR),
        message: "broken".to_string(),
        ..lsp_types::Diagnostic::default()
    };
    {
        let mut cache = server.context.notification_cache.lock().await;
        cache.store_diagnostics(&ServerId::from("rust"), &first_uri, Some(2), vec![]);
        cache.store_diagnostics(
            &ServerId::from("rust"),
            &second_uri,
            Some(1),
            vec![error.clone()],
        );
    }
    let mut anonymous = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    assert_eq!(changed(&anonymous.diagnostics(None).await), 1);
    server
        .context
        .notification_cache
        .lock()
        .await
        .store_diagnostics(&ServerId::from("rust"), &first_uri, Some(3), vec![error]);
    assert_eq!(changed(&anonymous.diagnostics(metadata("thread")).await), 0);
    assert_eq!(changed(&existing.diagnostics(metadata("thread")).await), 0);
    anonymous.close().await;
    existing.close().await;
}

#[tokio::test]
async fn codex_root_and_child_use_their_thread_records() {
    let server = server_with_one_error().await;
    let session = || SessionId::named(Some("root".to_string()));
    let mut root = Client::connect(server.for_connection(session()), "codex-mcp-client").await;
    let mut child = Client::connect(server.for_connection(session()), "codex-mcp-client").await;
    assert_eq!(changed(&root.diagnostics(metadata("root")).await), 1);
    assert_eq!(changed(&child.diagnostics(metadata("child")).await), 1);
    assert_eq!(changed(&root.diagnostics(metadata("root")).await), 0);
    assert_eq!(changed(&child.diagnostics(metadata("child")).await), 0);
    root.close().await;
    child.close().await;
}

#[tokio::test]
async fn codex_reconnect_recovers_the_existing_thread_record() {
    let server = server_with_one_error().await;
    let mut first = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    assert_eq!(changed(&first.diagnostics(metadata("thread")).await), 1);
    first.close().await;
    let mut replacement = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    assert_eq!(
        changed(&replacement.diagnostics(metadata("thread")).await),
        0
    );
    replacement.close().await;
}

#[tokio::test]
async fn codex_adopts_anonymous_history_on_the_first_identified_call() {
    let server = server_with_one_error().await;
    let mut first = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    assert_eq!(changed(&first.diagnostics(None).await), 1);
    first
        .call("get_server_messages", Some(json!({"threadId": "thread"})))
        .await;
    let mut second = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    assert_eq!(changed(&second.diagnostics(metadata("thread")).await), 0);
    first.close().await;
    second.close().await;
}

#[tokio::test]
async fn codex_thread_metadata_falls_back_and_prefers_thread_id() {
    let server = server_with_one_error().await;
    let mut client = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    let nested = || Some(json!({"x-codex-turn-metadata": {"thread_id": "nested"}}));
    assert_eq!(changed(&client.diagnostics(nested()).await), 1);
    assert_eq!(
        changed(
            &client
                .diagnostics(Some(json!({
                    "threadId": "top", "x-codex-turn-metadata": {"thread_id": "nested"}
                })))
                .await
        ),
        1
    );
    assert_eq!(changed(&client.diagnostics(metadata("top")).await), 0);
    assert_eq!(changed(&client.diagnostics(nested()).await), 0);
    client.close().await;
}

#[tokio::test]
async fn codex_missing_or_invalid_metadata_keeps_the_connection_fallback() {
    let server = server_with_one_error().await;
    let mut client = Client::connect(server.for_connection(None), "codex-mcp-client").await;
    assert_eq!(changed(&client.diagnostics(metadata("thread")).await), 1);
    assert_eq!(changed(&client.diagnostics(None).await), 1);
    for meta in [
        json!({}),
        json!({"threadId": ""}),
        json!({"threadId": 7}),
        json!({"x-codex-turn-metadata": {"thread_id": false}}),
    ] {
        assert_eq!(changed(&client.diagnostics(Some(meta)).await), 0);
    }
    client.close().await;
}

#[tokio::test]
async fn other_clients_keep_the_handshake_identity() {
    let server = server_with_one_error().await;
    let session = || SessionId::named(Some("claude-session".to_string()));
    let mut first = Client::connect(server.for_connection(session()), "claude-code").await;
    let mut second = Client::connect(server.for_connection(session()), "claude-code").await;
    assert_eq!(
        changed(&first.diagnostics(metadata("unrelated-a")).await),
        1
    );
    assert_eq!(
        changed(&second.diagnostics(metadata("unrelated-b")).await),
        0
    );
    first.close().await;
    second.close().await;
}

#[tokio::test]
async fn other_anonymous_clients_keep_separate_connection_records() {
    let server = server_with_one_error().await;
    let mut first = Client::connect(server.for_connection(None), "other-client").await;
    let mut second = Client::connect(server.for_connection(None), "other-client").await;
    assert_eq!(changed(&first.diagnostics(metadata("same")).await), 1);
    assert_eq!(changed(&second.diagnostics(metadata("same")).await), 1);
    first.close().await;
    second.close().await;
}
