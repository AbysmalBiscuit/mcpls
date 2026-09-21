//! `mcpls brief`: the note a `SessionStart` hook hands an agent session, so
//! the agent learns mcpls serves its checkout without the user writing
//! that into their own instructions.

use std::path::Path;

use mcpls_core::ServerConfig;

/// The brief for a session whose checkout is `root`, or `None` when the
/// configuration switches it off or no installed language server applies
/// there. A checkout mcpls cannot serve gets silence, not a note saying so.
///
/// A server is listed only when its language has files here: a project
/// marker such as `.git` matches checkouts that hold none.
pub fn render(config: &ServerConfig, root: &Path) -> Option<String> {
    if !config.brief.enabled {
        return None;
    }
    let max_depth = Some(config.workspace.heuristics_max_depth);
    let candidates: Vec<_> = config
        .lsp_servers
        .iter()
        .filter(|server| server.should_spawn(root, max_depth))
        .collect();
    let wanted = candidates
        .iter()
        .map(|server| server.language_id.clone())
        .collect();
    // A missing binary stats every `PATH` entry, so each lookup runs on its
    // own thread beside the scan.
    let servers: Vec<String> = std::thread::scope(|scope| {
        #[expect(
            clippy::needless_collect,
            reason = "collecting spawns every lookup before the scan starts"
        )]
        let lookups: Vec<_> = candidates
            .into_iter()
            .map(|server| {
                let installed =
                    scope.spawn(|| crate::hook::resolve_program(&server.command, root).is_some());
                (server, installed)
            })
            .collect();
        let present = config.languages_present(root, &wanted);
        lookups
            .into_iter()
            .filter(|(server, _)| present.contains(&server.language_id))
            .filter_map(|(server, installed)| {
                installed
                    .join()
                    .unwrap_or(false)
                    .then(|| format!("- {} ({})", server.language_id, server.command))
            })
            .collect()
    });
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
