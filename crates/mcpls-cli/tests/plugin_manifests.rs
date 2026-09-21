//! The plugin's manifests are JSON that other programs read, so these check
//! the invariants that break without an error: the version every manifest
//! pins, the `PATH` lookup every entry runs, and the manifest file Codex must
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

/// The bootstrap installs the release named by the Claude Code manifest's
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

/// Every entry a harness runs names `mcpls` on `PATH`, which the bootstrap
/// installs at session start, and each harness's hook file names only events
/// that harness has.
#[test]
fn test_every_entry_runs_mcpls_from_path() {
    assert_eq!(
        json("plugin/.mcp.json")["mcpServers"]["mcpls"],
        serde_json::json!({ "command": "mcpls" })
    );

    let codex = json("plugin/.codex-plugin/plugin.json");
    assert_eq!(codex["hooks"], "./hooks/hooks-codex.json");
    assert_eq!(
        codex["mcpServers"], "./.mcp.json",
        "an inline object would replace the shared registration"
    );

    let harnesses: [(&str, &str, &str, &str, &[&str]); 2] = [
        (
            "plugin/hooks/hooks.json",
            "claude-code",
            "mcpls brief",
            "\"${CLAUDE_PLUGIN_ROOT}/hooks/run-hook.cmd\" bootstrap-binaries claude-code",
            &[
                "PostToolBatch",
                "PostToolUse",
                "SessionEnd",
                "SessionStart",
                "UserPromptSubmit",
            ],
        ),
        (
            "plugin/hooks/hooks-codex.json",
            "codex",
            "mcpls brief --additional-context",
            "\"${PLUGIN_ROOT}/hooks/run-hook.cmd\" bootstrap-binaries codex",
            &[
                "PostToolUse",
                "SessionEnd",
                "SessionStart",
                "UserPromptSubmit",
            ],
        ),
    ];
    for (file, harness, brief, bootstrap, events) in harnesses {
        let hooks = json(file);
        let hooks = hooks["hooks"].as_object().unwrap();
        let mut names: Vec<&str> = hooks.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, events, "{file}");

        let mut bootstraps = Vec::new();
        let mut briefs = Vec::new();
        for (event, groups) in hooks {
            let groups = groups.as_array().unwrap();
            assert!(!groups.is_empty(), "{file}: {event}");
            for group in groups {
                let registrations = group["hooks"].as_array().unwrap();
                assert!(!registrations.is_empty(), "{file}: {event}");
                for registration in registrations {
                    assert_eq!(registration["type"], "command", "{file}: {event}");
                    if registration["command"] == bootstrap {
                        bootstraps.push(event.as_str());
                    } else if registration["command"] == brief {
                        briefs.push(event.as_str());
                    } else {
                        assert_eq!(
                            registration["command"],
                            format!("mcpls hook {} --harness {harness}", kebab(event)),
                            "{file}: {event}"
                        );
                    }
                }
            }
        }
        assert_eq!(
            bootstraps,
            ["SessionStart"],
            "{file}: the bootstrap runs once, at session start"
        );
        assert_eq!(
            briefs,
            ["SessionStart"],
            "{file}: the brief runs once, at session start"
        );
    }

    assert_eq!(
        json("plugin/hooks/hooks-codex.json")["hooks"]["SessionStart"][0]["hooks"][0]["commandWindows"],
        "& \"${PLUGIN_ROOT}/hooks/run-hook.cmd\" bootstrap-binaries codex",
        "Codex runs Windows hooks through PowerShell, where a quoted path needs the call operator"
    );
}

/// The verb a harness event registers under: `PostToolUse` is
/// `post-tool-use`, the spelling devkit's hook verbs use too.
fn kebab(event: &str) -> String {
    let mut verb = String::new();
    for c in event.chars() {
        if c.is_ascii_uppercase() && !verb.is_empty() {
            verb.push('-');
        }
        verb.push(c.to_ascii_lowercase());
    }
    verb
}
