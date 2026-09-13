//! The plugin's manifests are JSON that other programs read, so these check
//! the invariants that break without an error: the version every manifest
//! pins, the launcher every entry runs, and the manifest file Codex must
//! never find.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn json(relative: &str) -> Value {
    let path = repo_root().join(relative);
    let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// A schema-bearing `plugin.json` at the plugin root switches Codex to the
/// Agent Plugins format, which starts the MCP server inside the plugin
/// directory and loads no hooks at all.
#[test]
fn test_the_plugin_root_has_no_plugin_json() {
    assert!(!repo_root().join("plugin/plugin.json").exists());
}

/// The launcher downloads the release named by the Claude Code manifest's
/// version, so every file repeating that version must agree with the
/// workspace the release is built from.
#[test]
fn test_every_manifest_pins_the_workspace_version() {
    let cargo: toml::Table = fs::read_to_string(repo_root().join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let workspace = cargo["workspace"]["package"]["version"].as_str().unwrap();

    let pinned = [
        (
            "plugin/.claude-plugin/plugin.json",
            json("plugin/.claude-plugin/plugin.json")["version"].clone(),
        ),
        (
            "plugin/.codex-plugin/plugin.json",
            json("plugin/.codex-plugin/plugin.json")["version"].clone(),
        ),
        (
            ".claude-plugin/marketplace.json",
            json(".claude-plugin/marketplace.json")["plugins"][0]["version"].clone(),
        ),
    ];
    for (file, version) in pinned {
        assert_eq!(version.as_str(), Some(workspace), "{file}");
    }
}

/// Nothing puts `mcpls` itself on `PATH`, so every entry a harness runs goes
/// through the launcher, and each harness's hook file names only events
/// that harness has.
#[test]
fn test_every_entry_runs_the_launcher_for_its_harness() {
    assert_eq!(
        json("plugin/.mcp.json")["mcpServers"]["mcpls"]["command"],
        "${CLAUDE_PLUGIN_ROOT}/bin/mcpls"
    );

    let codex = json("plugin/.codex-plugin/plugin.json");
    assert_eq!(codex["hooks"], "./hooks/hooks-codex.json");
    assert_eq!(codex["mcpServers"]["mcpls"]["command"], "sh");
    assert_eq!(
        codex["mcpServers"]["mcpls"]["env_vars"],
        serde_json::json!(["CODEX_HOME", "MCPLS_BIN", "MCPLS_HOME"]),
        "Codex starts an MCP server with none of these unless the entry names them"
    );
    assert_eq!(
        codex["mcpServers"]["mcpls"]["startup_timeout_sec"], 300,
        "Codex kills a server still starting after 30 seconds, and a cold start downloads for up to 300"
    );

    let harnesses: [(&str, &str, &[&str]); 2] = [
        (
            "plugin/hooks/hooks.json",
            "\"${CLAUDE_PLUGIN_ROOT}/bin/mcpls\" hook",
            &[
                "FileChanged",
                "PostToolBatch",
                "SessionEnd",
                "SessionStart",
                "UserPromptSubmit",
            ],
        ),
        (
            "plugin/hooks/hooks-codex.json",
            "\"${PLUGIN_ROOT}/bin/mcpls\" hook --host codex",
            &[
                "PostToolUse",
                "SessionEnd",
                "SubagentStop",
                "UserPromptSubmit",
            ],
        ),
    ];
    for (file, command, events) in harnesses {
        let hooks = json(file);
        let hooks = hooks["hooks"].as_object().unwrap();
        let mut names: Vec<&str> = hooks.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, events, "{file}");
        for group in hooks.values().flat_map(|groups| groups.as_array().unwrap()) {
            for hook in group["hooks"].as_array().unwrap() {
                assert_eq!(hook["command"], command, "{file}");
            }
        }
    }
}
