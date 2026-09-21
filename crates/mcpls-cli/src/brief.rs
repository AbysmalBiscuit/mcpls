//! `mcpls brief`: the note a `SessionStart` hook hands an agent session, so
//! the agent learns mcpls serves its checkout without the user writing
//! that into their own instructions.

use std::path::Path;

use mcpls_core::ServerConfig;

/// The brief for a session whose checkout is `root`, or `None` when the
/// configuration switches it off or no installed language server applies
/// there. A checkout mcpls cannot serve gets silence, not a note saying so.
pub fn render(config: &ServerConfig, root: &Path) -> Option<String> {
    if !config.brief.enabled {
        return None;
    }
    let max_depth = Some(config.workspace.heuristics_max_depth);
    let servers: Vec<String> = config
        .lsp_servers
        .iter()
        .filter(|server| server.should_spawn(root, max_depth))
        .filter(|server| crate::hook::resolve_program(&server.command).is_some())
        .map(|server| format!("- {} ({})", server.language_id, server.command))
        .collect();
    if servers.is_empty() {
        return None;
    }
    Some(format!(
        "## mcpls language servers\n\n\
         The mcpls tools answer from these language servers in this checkout:\n\n{}\n\n\
         For a symbol in these languages, start with the mcpls tools.\n",
        servers.join("\n")
    ))
}

/// `text` wrapped the way a Codex `SessionStart` hook returns context.
pub fn session_start_output(text: &str) -> String {
    serde_json::json!({ "hookSpecificOutput": {
        "hookEventName": "SessionStart", "additionalContext": text
    } })
    .to_string()
}
