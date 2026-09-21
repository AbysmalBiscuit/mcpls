//! `mcpls brief` run the way a `SessionStart` hook runs it: the real
//! binary, a real checkout, the hook's environment.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt as _;
use tempfile::TempDir;

/// A checkout holding `marker`, and a config naming one language server
/// that applies only where `marker` exists. The server's command is the
/// mcpls binary itself, an executable on every platform, and `PATH` is
/// emptied so no built-in server resolves on the test machine.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    config: PathBuf,
}

fn fixture(extra: &str) -> Fixture {
    let dir = TempDir::new().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join("fake.marker"), "").unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("src").join("main.fake"), "").unwrap();
    let command = assert_cmd::cargo::cargo_bin("mcpls");
    let config = root.join("brief.toml");
    std::fs::write(
        &config,
        format!(
            "[[lsp_servers]]\nname = \"fake-ls\"\nlanguage_id = \"fake\"\ncommand = {command:?}\n\
             file_patterns = [\"**/*.fake\"]\n\
             [lsp_servers.heuristics]\nproject_markers = [\"fake.marker\"]\n{extra}",
            command = command.display().to_string(),
        ),
    )
    .unwrap();
    Fixture {
        _dir: dir,
        root,
        config,
    }
}

fn brief(cwd: &Path, project_dir: Option<&Path>, config: &Path, args: &[&str]) -> Output {
    let mut command = Command::cargo_bin("mcpls").unwrap();
    command
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .env_remove("CLAUDE_PROJECT_DIR")
        .env("PATH", "")
        .current_dir(cwd)
        .arg("--config")
        .arg(config)
        .arg("brief")
        .args(args);
    if let Some(project_dir) = project_dir {
        command.env("CLAUDE_PROJECT_DIR", project_dir);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_brief_names_the_language_servers_that_serve_this_checkout() {
    let fixture = fixture("");

    let output = brief(&fixture.root, None, &fixture.config, &[]);

    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.starts_with("## mcpls"), "{text}");
    assert!(text.contains("- fake ("), "{text}");
}

/// Claude Code starts hooks in the session's directory, which need not be
/// the project; `CLAUDE_PROJECT_DIR` names the project.
#[test]
fn the_brief_describes_the_project_the_host_names() {
    let fixture = fixture("");
    let elsewhere = TempDir::new().unwrap();

    let named = brief(elsewhere.path(), Some(&fixture.root), &fixture.config, &[]);
    let unnamed = brief(elsewhere.path(), None, &fixture.config, &[]);

    assert!(stdout(&named).contains("- fake ("), "{}", stdout(&named));
    assert_eq!(stdout(&unnamed), "", "no marker there, no server applies");
}

#[test]
fn a_checkout_no_installed_server_serves_gets_no_brief() {
    let fixture = fixture("");
    std::fs::remove_file(fixture.root.join("fake.marker")).unwrap();

    let output = brief(&fixture.root, None, &fixture.config, &[]);

    assert!(output.status.success());
    assert_eq!(stdout(&output), "");
}

/// A marker says a server could start here, not that its language is
/// here: a `.git` marker matches every checkout.
#[test]
fn a_server_whose_language_has_no_files_here_is_left_out() {
    let fixture = fixture("");
    std::fs::remove_file(fixture.root.join("src").join("main.fake")).unwrap();

    let output = brief(&fixture.root, None, &fixture.config, &[]);

    assert!(output.status.success());
    assert_eq!(stdout(&output), "");
}

#[test]
fn a_gitignored_file_does_not_put_its_language_in_the_brief() {
    let fixture = fixture("");
    std::fs::remove_file(fixture.root.join("src").join("main.fake")).unwrap();
    std::fs::create_dir(fixture.root.join("out")).unwrap();
    std::fs::write(fixture.root.join("out").join("built.fake"), "").unwrap();
    std::fs::write(fixture.root.join(".gitignore"), "out/\n").unwrap();

    let output = brief(&fixture.root, None, &fixture.config, &[]);

    assert_eq!(stdout(&output), "");
}

#[test]
fn the_brief_is_switched_off_in_config() {
    let fixture = fixture("[brief]\nenabled = false\n");

    let output = brief(&fixture.root, None, &fixture.config, &[]);

    assert!(output.status.success());
    assert_eq!(stdout(&output), "");
}

/// Codex reads a `SessionStart` hook's context out of a JSON field, and
/// rejects an object carrying any key it does not know.
#[test]
fn additional_context_wraps_the_brief_in_the_session_start_envelope() {
    let fixture = fixture("");

    let plain = stdout(&brief(&fixture.root, None, &fixture.config, &[]));
    let wrapped = stdout(&brief(
        &fixture.root,
        None,
        &fixture.config,
        &["--additional-context"],
    ));

    let json: serde_json::Value = serde_json::from_str(&wrapped).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": plain,
        } })
    );
}

/// A brief is context, never a gate: a config that fails to load leaves
/// the session to start without one, and `mcpls doctor` says why.
#[test]
fn a_config_that_fails_to_load_gives_no_brief_and_exits_zero() {
    let fixture = fixture("");
    std::fs::write(&fixture.config, "not = [valid").unwrap();

    let output = brief(&fixture.root, None, &fixture.config, &[]);

    assert!(output.status.success());
    assert_eq!(stdout(&output), "");
}
