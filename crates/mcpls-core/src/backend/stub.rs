//! What the frontend answers when no backend is attached: the frozen tool
//! surface, and the reason every tool call fails.
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::OnceLock;

use serde_json::{Value, json};

use crate::backend::handshake::VERSION;
use crate::mcp::INSTRUCTIONS;

fn tools() -> &'static Value {
    static TOOLS: OnceLock<Value> = OnceLock::new();
    TOOLS.get_or_init(|| {
        serde_json::from_str(include_str!("../mcp/tool_surface.json")).unwrap_or(Value::Null)
    })
}

/// The response to `message` for a session with no backend, because of
/// `reason`. `None` for a notification or a response to nothing.
///
/// A tool call fails at once, whether the session is waiting for a backend
/// or has none, so a wait reads like any other unreachable backend.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn answer(message: &Value, reason: &str) -> Option<Value> {
    let id = message.get("id")?;
    let method = message.get("method").and_then(Value::as_str)?;
    let result = match method {
        "initialize" => json!({
            "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}, "resources": {"subscribe": true}},
            "serverInfo": {"name": "mcpls", "version": VERSION},
            "instructions": format!("{INSTRUCTIONS} {reason}"),
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "resources/list" => json!({"resources": []}),
        "resources/templates/list" => json!({"resourceTemplates": []}),
        "tools/call" => json!({
            "content": [{"type": "text", "text": reason}],
            "isError": true,
        }),
        _ => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": reason},
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_initialize_carries_the_reason_in_the_instructions() {
        let reply = answer(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
            "No backend.",
        )
        .expect("initialize is answered");
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        assert!(
            reply["result"]["instructions"]
                .as_str()
                .unwrap()
                .ends_with(" No backend.")
        );
        assert_eq!(reply["result"]["serverInfo"]["name"], "mcpls");
    }

    #[test]
    fn test_the_tool_list_is_the_frozen_surface() {
        let reply = answer(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}), "r")
            .expect("tools/list is answered");
        let expected: Value =
            serde_json::from_str(include_str!("../mcp/tool_surface.json")).unwrap();
        assert_eq!(reply["result"]["tools"], expected);
    }

    #[test]
    fn test_a_tool_call_fails_at_once_with_the_reason() {
        let call =
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_hover"}});
        let reply = answer(&call, "Backend stopped.").expect("a tool call is answered");
        assert_eq!(reply["id"], 3);
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["content"][0]["text"], "Backend stopped.");
    }

    #[test]
    fn test_notifications_are_dropped_and_unknown_requests_fail() {
        assert_eq!(
            answer(
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                "r"
            ),
            None
        );
        let reply = answer(
            &json!({"jsonrpc":"2.0","id":"x","method":"resources/read"}),
            "gone",
        )
        .expect("a request gets a response");
        assert_eq!(reply["error"]["message"], "gone");
        assert_eq!(reply["id"], "x");
    }
}
