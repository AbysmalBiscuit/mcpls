//! `mcpls doctor` and `mcpls config` run from a plain shell.
//!
//! Both answer questions a user asks when nothing works, so both are
//! driven here as a user runs them: the real binary, a real checkout, no
//! agent session and no backend anywhere.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt as _;
use tempfile::TempDir;

/// A checkout with its own runtime directory and user name, so a doctor
/// run here can never meet a real backend or another test's.
fn checkout() -> (TempDir, TempDir) {
    let dir = TempDir::new().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    (dir, TempDir::new().unwrap())
}

fn run(cwd: &Path, runtime: &Path, args: &[&str]) -> Output {
    let mut command = Command::cargo_bin("mcpls").unwrap();
    command
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .env_remove("MCPLS_LOG")
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", runtime)
        .env("USER", format!("mcpls-doctor-test-{}", std::process::id()))
        .env(
            "USERNAME",
            format!("mcpls-doctor-test-{}", std::process::id()),
        )
        .current_dir(cwd)
        .args(args);
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_doctor_and_config_appear_in_the_top_level_help() {
    let mut command = Command::cargo_bin("mcpls").unwrap();
    let help = stdout(&command.arg("--help").output().unwrap());

    assert!(help.contains("doctor"), "{help}");
    assert!(help.contains("config"), "{help}");
}

/// The doctor used to report on `CLAUDE_PROJECT_DIR` whatever directory
/// the reader was standing in, with nothing on the report saying so.
#[test]
fn the_doctor_names_the_directory_it_examined_and_where_that_came_from() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();

    let from_cwd = stdout(&run(&root, runtime.path(), &["doctor"]));
    assert!(
        from_cwd.contains(&format!(
            "hook sees: {} (the working directory; CLAUDE_PROJECT_DIR is unset)",
            root.display()
        )),
        "{from_cwd}"
    );

    let elsewhere = TempDir::new().unwrap();
    let named = stdout(&run(
        elsewhere.path(),
        runtime.path(),
        &["doctor", &root.display().to_string()],
    ));
    assert!(
        named.contains(&format!(
            "hook sees: {} (given on the command line)",
            root.display()
        )),
        "{named}"
    );
}

/// A configured server whose binary is not installed reads exactly like
/// one that was never configured, which is the state this line exists to
/// break apart.
#[test]
fn the_doctor_says_a_configured_server_is_not_installed_and_exits_non_zero() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    std::fs::write(root.join("present.marker"), "").unwrap();
    let config = root.join("test-mcpls.toml");
    std::fs::write(
        &config,
        "[[lsp_servers]]\nlanguage_id = \"invented\"\n\
         command = \"mcpls-no-such-language-server\"\n\
         file_patterns = [\"**/*.invented\"]\n\
         [lsp_servers.heuristics]\nproject_markers = [\"present.marker\"]\n",
    )
    .unwrap();

    let output = run(
        &root,
        runtime.path(),
        &["--config", &config.display().to_string(), "doctor"],
    );
    let report = stdout(&output);

    assert!(
        report.contains("invented (not installed: mcpls-no-such-language-server is not on PATH)"),
        "{report}"
    );
    assert!(
        report.contains("problems: your configuration asks for invented"),
        "{report}"
    );
    assert!(
        !output.status.success(),
        "a fault the doctor found belongs in its exit status: {report}"
    );
}

/// A server that does not serve this checkout is not a fault, so a
/// working install exits 0 with nothing to report.
#[test]
fn a_server_that_does_not_apply_here_is_not_a_problem() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = root.join("test-mcpls.toml");
    std::fs::write(
        &config,
        "[[lsp_servers]]\nlanguage_id = \"invented\"\n\
         command = \"mcpls-no-such-language-server\"\n\
         file_patterns = [\"**/*.invented\"]\n\
         [lsp_servers.heuristics]\nproject_markers = [\"absent.marker\"]\n",
    )
    .unwrap();

    let report = stdout(&run(
        &root,
        runtime.path(),
        &["--config", &config.display().to_string(), "doctor"],
    ));

    assert!(
        report.contains("invented (not applicable here)"),
        "{report}"
    );
}

/// The alias is what the plugin's troubleshooting text tells people to
/// run, and every hook registration depends on its exit status.
#[test]
fn hook_doctor_stays_an_alias_that_always_exits_zero() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    std::fs::write(root.join("present.marker"), "").unwrap();
    let config = root.join("test-mcpls.toml");
    std::fs::write(
        &config,
        "[[lsp_servers]]\nlanguage_id = \"invented\"\n\
         command = \"mcpls-no-such-language-server\"\n\
         file_patterns = [\"**/*.invented\"]\n\
         [lsp_servers.heuristics]\nproject_markers = [\"present.marker\"]\n",
    )
    .unwrap();

    let output = run(
        &root,
        runtime.path(),
        &["--config", &config.display().to_string(), "hook", "doctor"],
    );

    assert!(
        output.status.success(),
        "the alias must never exit non-zero"
    );
    assert!(stdout(&output).contains("problems: your configuration asks for invented"));
}

#[test]
fn config_prints_the_tier_the_source_and_a_configuration_that_parses_back() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = root.join("test-mcpls.toml");
    std::fs::write(&config, "[backend]\nidle_shutdown_ms = 4321\n").unwrap();

    let out = stdout(&run(
        &root,
        runtime.path(),
        &["--config", &config.display().to_string(), "config"],
    ));

    assert!(out.contains("# tier: explicit"), "{out}");
    assert!(
        out.contains(&format!("# source: {}", config.display())),
        "{out}"
    );

    let body: String = out
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let parsed: mcpls_core::ServerConfig = toml::from_str(&body).unwrap();
    assert_eq!(parsed.backend.idle_shutdown_ms, 4321);
}

/// An `mcpls.toml` discovered at the checkout root and skipped is the
/// single most confusing state the configuration can be in: the file the
/// reader just edited has no effect and nothing says why.
#[test]
fn both_commands_name_a_project_config_they_ignored() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let planted = root.join("mcpls.toml");
    std::fs::write(&planted, "[backend]\nidle_shutdown_ms = 4321\n").unwrap();

    let config = stdout(&run(&root, runtime.path(), &["config"]));
    assert!(
        config.contains(&format!("# ignored: {}", planted.display())),
        "{config}"
    );
    assert!(config.contains("--trust-project-config"), "{config}");

    let doctor = stdout(&run(&root, runtime.path(), &["doctor"]));
    assert!(
        doctor.contains(&format!("project config: {}", planted.display())),
        "{doctor}"
    );

    let trusted = stdout(&run(
        &root,
        runtime.path(),
        &["--trust-project-config", "config"],
    ));
    assert!(!trusted.contains("# ignored:"), "{trusted}");
    assert!(trusted.contains("# tier: project"), "{trusted}");
}

/// A script deserializes `--json` straight back into a configuration, so
/// nothing may wrap it.
#[test]
fn config_json_is_the_configuration_itself() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = root.join("test-mcpls.toml");
    std::fs::write(&config, "[backend]\nidle_shutdown_ms = 4321\n").unwrap();

    let out = stdout(&run(
        &root,
        runtime.path(),
        &[
            "--config",
            &config.display().to_string(),
            "config",
            "--json",
        ],
    ));

    let parsed: mcpls_core::ServerConfig = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed.backend.idle_shutdown_ms, 4321);
}

#[test]
fn config_origin_tells_a_setting_the_file_made_from_one_the_builtins_did() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = root.join("test-mcpls.toml");
    std::fs::write(&config, "[backend]\nidle_shutdown_ms = 4321\n").unwrap();

    let out = stdout(&run(
        &root,
        runtime.path(),
        &[
            "--config",
            &config.display().to_string(),
            "config",
            "--origin",
        ],
    ));

    let decided = out
        .lines()
        .find(|line| line.starts_with("backend.idle_shutdown_ms ="))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(decided.contains("= 4321"), "{decided}");
    assert!(
        decided.contains(&format!("# from {}", config.display())),
        "{decided}"
    );

    let inherited = out
        .lines()
        .find(|line| line.starts_with("backend.spawn ="))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(inherited.contains("# (default)"), "{inherited}");
}
