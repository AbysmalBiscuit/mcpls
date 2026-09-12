//! Integration tests for the mcpls CLI binary.

#![allow(clippy::unwrap_used)]
#![allow(deprecated)]

use std::fs;
use std::process::Command;
use std::time::Duration;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

const MCP_INPUT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"mcpls-cli-test","version":"0.1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}
"#;

/// Vars that could leak in from the ambient environment (e.g. a developer's
/// shell, or a repo `.envrc`) and change these tests' outcome: `MCPLS_LOG`
/// could suppress a warning a test greps for, `MCPLS_CONFIG` could redirect
/// config loading away from the CWD/`--config` path under test,
/// `MCPLS_TRUST_PROJECT_CONFIG` could flip the trust decision a test is
/// specifically exercising, and `MCPLS_LOG_JSON` could flip the log output
/// format a test asserts on. `assert_cmd::Command`/`std::process::Command`
/// inherit the parent's full environment by default, so every test must
/// clear these before setting the ones it actually wants.
fn clear_ambient_env(cmd: &mut Command) -> &mut Command {
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .env_remove("MCPLS_LOG_JSON")
}

/// A session started in a subdirectory of a checkout derives the endpoint
/// the checkout's own root derives, which is what lets the two share one
/// backend.
#[test]
fn hook_doctor_reports_the_checkout_root_from_a_subdirectory() {
    let project = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let root = dunce::canonicalize(project.path()).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    let nested = root.join("crates").join("core");
    fs::create_dir_all(&nested).unwrap();

    let root_hash = mcpls_core::hooks::identity_hash(&root).unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    let output = clear_ambient_env(&mut cmd)
        .env("CLAUDE_PROJECT_DIR", &nested)
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", runtime.path())
        .env("USER", "mcpls-test")
        .args(["hook", "doctor"])
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&output.stdout);

    assert!(
        report.contains(&root_hash),
        "doctor reported a hash other than the checkout root's: {report}"
    );
    assert!(
        report.contains(&format!("root: {}", root.display())),
        "doctor did not name the checkout root: {report}"
    );
}

#[test]
fn test_help_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn test_version_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_version_short_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("-V")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_help_short_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("-h")
        .assert()
        .success()
        .stdout(predicate::str::contains("--config"));
}

#[test]
fn test_invalid_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--invalid-flag")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn test_config_file_not_found() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load config"));
}

#[test]
fn test_config_with_invalid_toml() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("invalid.toml");

    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--config")
        .arg(&config_path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load config"));
}

#[test]
fn test_config_short_flag() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("-c")
        .arg("/nonexistent/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load config"));
}

#[test]
fn test_config_with_empty_file() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("empty.toml");

    fs::write(&config_path, "").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--config")
        .arg(&config_path)
        // Default config starts hook service, so isolate its runtime directory.
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", temp_dir.path())
        .env("USER", "mcpls-test")
        .assert()
        .failure();
}

#[test]
fn i1_t1_novel_file_language_requires_mapping() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(
        &config_path,
        r#"[diagnostics.hooks]
enabled = false

[[lsp_servers]]
language_id = "elixir"
command = "true"
"#,
    )
    .unwrap();

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    let output = cmd
        .env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .env_remove("MCPLS_LOG_JSON")
        .arg("--config")
        .arg(&config_path)
        .current_dir(temp_dir.path())
        .timeout(Duration::from_secs(5))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "an unroutable file-tool language must fail at startup; stderr: {stderr}"
    );
    assert!(
        stderr.contains("elixir"),
        "error should name elixir: {stderr}"
    );
    assert!(
        stderr.contains("file_patterns"),
        "error should explain how to add file patterns: {stderr}"
    );
    assert!(
        stderr.contains("workspace"),
        "error should explain how to add a workspace mapping: {stderr}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn i1_t2_generated_config_inherits_builtins() {
    let user_config = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let first_runtime = TempDir::new().unwrap();

    #[cfg(target_os = "linux")]
    let existing_path = user_config.path().join("mcpls").join("mcpls.toml");
    #[cfg(target_os = "macos")]
    let existing_path = user_config
        .path()
        .join("Library")
        .join("Application Support")
        .join("mcpls")
        .join("mcpls.toml");
    #[cfg(target_os = "linux")]
    let config_env = "XDG_CONFIG_HOME";
    #[cfg(target_os = "macos")]
    let config_env = "HOME";

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .env(config_env, user_config.path())
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", first_runtime.path())
        .env("USER", "mcpls-test")
        .current_dir(workspace.path());
    let first_output = assert_cmd::Command::from_std(cmd)
        .write_stdin(MCP_INPUT)
        .timeout(Duration::from_secs(5))
        .output()
        .unwrap();
    assert!(
        first_output.status.success(),
        "first-run CLI invocation failed: {}",
        String::from_utf8_lossy(&first_output.stderr)
    );

    let generated = fs::read_to_string(&existing_path).unwrap();
    let parsed: toml::Value = toml::from_str(&generated).unwrap();
    assert!(parsed.get("lsp_servers").is_none(), "{generated}");

    let loaded = mcpls_core::ServerConfig::load_from(&existing_path).unwrap();
    assert_eq!(
        serde_json::to_value(&loaded.lsp_servers).unwrap(),
        serde_json::to_value(&mcpls_core::ServerConfig::default().lsp_servers).unwrap()
    );
}

#[test]
fn i1_t2_explicit_config_remains_unchanged() {
    let config_dir = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let config_path = config_dir.path().join("mcpls.toml");
    let original_bytes = br#"[[lsp_servers]]
language_id = "rust"
command = "custom-rust-analyzer"
"#
    .to_vec();
    fs::write(&config_path, &original_bytes).unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", runtime.path())
        .env("USER", "mcpls-test")
        .current_dir(workspace.path())
        .arg("--config")
        .arg(&config_path);
    let output = assert_cmd::Command::from_std(cmd)
        .write_stdin(MCP_INPUT)
        .timeout(Duration::from_secs(5))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "existing-config CLI invocation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&config_path).unwrap(), original_bytes);
}

/// A CWD-discovered `./mcpls.toml` is untrusted by default: it must not be
/// parsed at all, regardless of `--config`/`MCPLS_CONFIG` (which are
/// unaffected by trust and aren't exercised here). We assert this by
/// planting an invalid TOML file and confirming the process does *not* fail
/// with a config-parse error — instead it logs the ignore-warning and
/// proceeds to serve (which blocks on stdio, so it's killed once the
/// timeout elapses; the kill itself is expected, not a test failure).
#[test]
fn test_trust_project_config_env_false_does_not_grant_trust() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG");
    let output = cmd
        .current_dir(temp_dir.path())
        // Isolate the hook socket when startup proceeds past config loading.
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", temp_dir.path())
        .env("USER", "mcpls-test")
        .env("MCPLS_TRUST_PROJECT_CONFIG", "false")
        // Allow startup to log before killing the process blocked on stdio.
        .timeout(Duration::from_secs(5))
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ignoring untrusted project-local config"),
        "expected the untrusted-ignore warning, got stderr: {stderr}"
    );
    assert!(
        !stderr.contains("failed to load configuration"),
        "MCPLS_TRUST_PROJECT_CONFIG=false must not grant trust; stderr: {stderr}"
    );
}

/// `parse_bool_flag` (the custom `value_parser` on this field, see
/// `args.rs`) accepts `1`/`0`, `true`/`false`, `yes`/`no`, `y`/`n`, and
/// `on`/`off`, case-insensitively — but a value outside that set must still
/// be *rejected* outright rather than silently coerced to either trust
/// state. This is the strongest form of "does not grant trust": the process
/// never gets far enough to load anything, trusted or not. We don't pin the
/// exact clap error wording (brittle across clap upgrades); it's enough
/// that the process fails before either trust branch's log line could
/// appear, since argument parsing runs before logging is even initialized.
#[test]
fn test_trust_project_config_env_invalid_value_rejected() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(temp_dir.path())
        .env("MCPLS_TRUST_PROJECT_CONFIG", "banana")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load configuration").not())
        .stderr(predicate::str::contains("ignoring untrusted project-local config").not());
}

/// Companion to `test_trust_project_config_env_false_does_not_grant_trust`:
/// `0` is one of the numeric spellings `parse_bool_flag` accepts as falsy
/// (issue #295), so it must behave identically to `false` — proceed to the
/// untrusted-config path, not get rejected as an invalid value like
/// `test_trust_project_config_env_invalid_value_rejected` above.
#[test]
fn test_trust_project_config_env_0_does_not_grant_trust() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG");
    let output = cmd
        .current_dir(temp_dir.path())
        // See test_trust_project_config_env_false_does_not_grant_trust
        // above for why this redirects the real hook socket.
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", temp_dir.path())
        .env("USER", "mcpls-test")
        .env("MCPLS_TRUST_PROJECT_CONFIG", "0")
        // See test_trust_project_config_env_false_does_not_grant_trust above
        // for why this timeout is expected to always elapse.
        .timeout(Duration::from_secs(5))
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ignoring untrusted project-local config"),
        "expected the untrusted-ignore warning, got stderr: {stderr}"
    );
    assert!(
        !stderr.contains("failed to load configuration"),
        "MCPLS_TRUST_PROJECT_CONFIG=0 must not grant trust; stderr: {stderr}"
    );
}

#[test]
fn test_trust_project_config_flag_grants_trust() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(temp_dir.path())
        .arg("--trust-project-config")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load configuration"));
}

#[test]
fn test_trust_project_config_env_true_grants_trust() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(temp_dir.path())
        .env("MCPLS_TRUST_PROJECT_CONFIG", "true")
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load configuration"));
}

/// A missing `MCPLS_CONFIG` file takes precedence over malformed trusted CWD config.
#[test]
fn test_mcpls_config_env_wins_over_cwd_file_even_when_trusted() {
    let temp_dir = TempDir::new().unwrap();
    let cwd_config_path = temp_dir.path().join("mcpls.toml");
    fs::write(&cwd_config_path, "this is not valid TOML {{{{").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd)
        .current_dir(temp_dir.path())
        .arg("--trust-project-config")
        .env("MCPLS_CONFIG", "/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("configuration file not found"))
        .stderr(predicate::str::contains("TOML parsing error").not());
}

#[test]
fn test_config_file_with_spaces_in_path() {
    let temp_dir = TempDir::new().unwrap();
    let subdir = temp_dir.path().join("path with spaces");
    fs::create_dir(&subdir).unwrap();
    let config_path = subdir.join("config.toml");

    fs::write(&config_path, "invalid content").unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--config")
        .arg(&config_path)
        .assert()
        .failure();
}

/// A missing config makes startup and fatal-error logging observable without a timeout.
#[test]
fn test_log_json_flag_emits_json_formatted_logs() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--log-json")
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\""))
        .stderr(predicate::str::contains(
            "\"message\":\"mcpls exited with an error\"",
        ));
}

/// Same as `test_log_json_flag_emits_json_formatted_logs`, but via the
/// `MCPLS_LOG_JSON` env var (clap's `env` attribute on `Args::log_json`)
/// instead of the `--log-json` flag, since the two are parsed through
/// separate clap code paths that both need to reach `logging::init`.
#[test]
fn test_log_json_env_var_emits_json_formatted_logs() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .env("MCPLS_LOG_JSON", "true")
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\""))
        .stderr(predicate::str::contains(
            "\"message\":\"mcpls exited with an error\"",
        ));
}

/// Issue #295: `MCPLS_LOG_JSON` must accept boolean spellings beyond the
/// exact literal `true` already covered above. Exercises the real
/// `env = "MCPLS_LOG_JSON"` + `value_parser = parse_bool_flag` wiring on
/// `Args::log_json` end-to-end (through clap, not just `parse_bool_flag` in
/// isolation) for each truthy spelling, guarding against a regression that
/// drops either attribute from the field.
#[test]
fn test_log_json_env_var_accepts_truthy_conventions() {
    for value in ["1", "TRUE", "yes", "on", "Y"] {
        let mut cmd = Command::cargo_bin("mcpls").unwrap();

        clear_ambient_env(&mut cmd)
            .env("MCPLS_LOG_JSON", value)
            .arg("--config")
            .arg("/nonexistent/path/to/config.toml")
            .assert()
            .failure()
            .stderr(predicate::str::contains("\"message\":\"starting mcpls\""));
    }
}

/// Falsy counterpart of `test_log_json_env_var_accepts_truthy_conventions`:
/// each spelling must be accepted (the process reaches `logging::init`, not
/// a clap parse error) and select the compact non-JSON formatter, same as
/// the unset-env default.
#[test]
fn test_log_json_env_var_accepts_falsy_conventions() {
    for value in ["0", "no", "off", "N"] {
        let mut cmd = Command::cargo_bin("mcpls").unwrap();

        clear_ambient_env(&mut cmd)
            .env("MCPLS_LOG_JSON", value)
            .arg("--config")
            .arg("/nonexistent/path/to/config.toml")
            .assert()
            .failure()
            .stderr(predicate::str::contains("starting mcpls"))
            .stderr(predicate::str::contains("\"message\":\"starting mcpls\"").not());
    }
}

/// A value outside `parse_bool_flag`'s accepted set must still be rejected
/// at argument-parsing time, before `logging::init` (or anything else)
/// runs — mirrors
/// `test_trust_project_config_env_invalid_value_rejected` for the other
/// bool+env field.
#[test]
fn test_log_json_env_var_rejects_invalid_value() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .env("MCPLS_LOG_JSON", "banana")
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\"").not());
}

/// Complements the two tests above: without `--log-json`/`MCPLS_LOG_JSON`,
/// output must stay in the default compact format. Guards against a
/// regression that flips the default (e.g. an inverted `if log_json`
/// condition), which the JSON-mode tests alone wouldn't catch.
#[test]
fn test_default_logging_is_not_json() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("--config")
        .arg("/nonexistent/path/to/config.toml")
        .assert()
        .failure()
        .stderr(predicate::str::contains("starting mcpls"))
        .stderr(predicate::str::contains("\"message\":\"starting mcpls\"").not());
}

/// Every shell `clap_complete` supports must produce a non-empty script and
/// exit 0. The subcommand short-circuits before config loading and before
/// the MCP server starts, so it works with no `mcpls.toml` present and
/// terminates on its own rather than blocking on stdio like a bare `mcpls`.
#[test]
fn test_completions_subcommand_emits_script_for_every_shell() {
    for shell in ["bash", "elvish", "fish", "nushell", "powershell", "zsh"] {
        let mut cmd = Command::cargo_bin("mcpls").unwrap();

        let output = clear_ambient_env(&mut cmd)
            .arg("completions")
            .arg(shell)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "completions {shell} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stdout.is_empty(),
            "completions {shell} wrote an empty script"
        );
    }
}

/// The script is generated from the live `Args` definition rather than
/// checked in, so a flag added to `Args` shows up in completions without a
/// second edit. `--trust-project-config` stands in for that: it is the
/// longest-lived flag with no short form, so its presence means the real
/// argument definitions were walked.
#[test]
fn test_completions_script_covers_current_flags() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("completions")
        .arg("bash")
        .assert()
        .success()
        .stdout(predicate::str::contains("--trust-project-config"))
        .stdout(predicate::str::contains("--log-json"));
}

/// An unsupported shell name is a parse error, not an empty script written
/// to stdout that a user would source without noticing.
#[test]
fn test_completions_rejects_unknown_shell() {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();

    clear_ambient_env(&mut cmd)
        .arg("completions")
        .arg("tcsh")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty());
}

/// A reader that closes the pipe early (`mcpls completions bash | head`) must
/// not crash the writer. The generators panic on a failed write -- some of
/// them from inside the fallible `try_generate` as well -- so
/// `completions::emit` renders into memory and performs the one write to
/// stdout itself, treating a broken pipe as the reader being done.
///
/// Every shell is covered because each has its own generator, and the write
/// that panics lives in that per-shell code: passing for one says nothing
/// about the rest.
#[cfg(unix)]
#[test]
fn test_completions_survives_a_closed_pipe() {
    use std::process::Stdio;

    for shell in ["bash", "elvish", "fish", "nushell", "powershell", "zsh"] {
        // Close the reader before spawning mcpls so its first write must fail.
        let mut departed_reader = Command::new("true").stdin(Stdio::piped()).spawn().unwrap();
        let closed_pipe = departed_reader.stdin.take().unwrap();
        departed_reader.wait().unwrap();

        let mut cmd = Command::cargo_bin("mcpls").unwrap();
        let output = clear_ambient_env(&mut cmd)
            .arg("completions")
            .arg(shell)
            .stdout(Stdio::from(closed_pipe))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
            .wait_with_output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            !stderr.contains("panicked"),
            "{shell}: a closed pipe must not panic: {stderr}"
        );
        assert!(
            output.status.success(),
            "{shell}: a closed pipe is a clean exit, got {:?}: {stderr}",
            output.status
        );
    }
}

/// Unset `CLAUDE_PROJECT_DIR` exercises canonicalization of the relative CWD fallback.
#[test]
fn test_hook_session_start_emits_absolute_watch_paths() {
    let temp_dir = TempDir::new().unwrap();
    fs::create_dir_all(temp_dir.path().join("src")).unwrap();

    let mut cmd = assert_cmd::Command::cargo_bin("mcpls").unwrap();
    cmd.env_remove("MCPLS_LOG")
        .env_remove("MCPLS_CONFIG")
        .env_remove("MCPLS_TRUST_PROJECT_CONFIG")
        .env_remove("MCPLS_LOG_JSON")
        .env_remove("CLAUDE_PROJECT_DIR");
    let assert = cmd
        .arg("hook")
        .current_dir(temp_dir.path())
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#)
        .assert()
        .success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        parsed["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    let paths = parsed["hookSpecificOutput"]["watchPaths"]
        .as_array()
        .unwrap();
    assert!(!paths.is_empty());
    for path in paths {
        let path = path.as_str().unwrap();
        assert!(
            std::path::Path::new(path).is_absolute(),
            "a watch path must be absolute, not relative to the hook \
             process's own working directory: {path}"
        );
    }
}

fn session_start(project: &std::path::Path) -> serde_json::Value {
    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    clear_ambient_env(&mut cmd);
    let output = assert_cmd::Command::from_std(cmd)
        .env("CLAUDE_PROJECT_DIR", project)
        .arg("hook")
        .write_stdin(r#"{"hook_event_name":"SessionStart"}"#)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn doctor_output(project: &std::path::Path) -> String {
    let runtime = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    let output = clear_ambient_env(&mut cmd)
        .env("CLAUDE_PROJECT_DIR", project)
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", runtime.path())
        .env("USER", "mcpls-test")
        .env(
            "USERNAME",
            format!(
                "mcpls-test-{}",
                runtime.path().file_name().unwrap().to_string_lossy()
            ),
        )
        .args(["hook", "doctor"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn test_doctor_does_not_claim_a_path_candidate_can_launch() {
    let project = TempDir::new().unwrap();
    let bin = TempDir::new().unwrap();
    let name = if cfg!(windows) { "mcpls.exe" } else { "mcpls" };
    let candidate = bin.path().join(name);
    fs::write(&candidate, "not an executable").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    let output = clear_ambient_env(&mut cmd)
        .env("CLAUDE_PROJECT_DIR", project.path())
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", bin.path())
        .env("USER", "mcpls-test")
        .env("USERNAME", bin.path().file_name().unwrap())
        .env("PATH", bin.path())
        .args(["hook", "doctor"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout
            .lines()
            .find(|line| line.starts_with("mcpls on PATH:")),
        Some(format!("mcpls on PATH: {}; launch not checked", candidate.display()).as_str())
    );
}

#[test]
fn test_watch_scan_distinguishes_missing_and_empty_roots_through_cli() {
    let project = TempDir::new().unwrap();
    let empty = session_start(project.path());
    assert_eq!(
        empty,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart", "watchPaths": []
        }})
    );
    assert!(doctor_output(project.path()).contains("watch scan: no eligible top-level paths"));

    let missing = project.path().join("missing");
    let failed = session_start(&missing);
    assert_eq!(
        failed["hookSpecificOutput"]["watchPaths"],
        serde_json::json!([])
    );
    assert!(
        failed["systemMessage"]
            .as_str()
            .unwrap()
            .contains("watch-path scan incomplete")
    );
    assert!(
        failed["systemMessage"]
            .as_str()
            .unwrap()
            .contains("missing")
    );
    let doctor = doctor_output(&missing);
    assert!(doctor.contains("watch scan: incomplete"), "{doctor}");
    assert!(!doctor.contains("watch scan: no eligible"), "{doctor}");
}

#[test]
fn test_watch_scan_reports_ignore_errors_without_losing_valid_paths() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::create_dir(project.path().join("target")).unwrap();
    fs::create_dir(project.path().join(".gitignore")).unwrap();
    let output = session_start(project.path());
    let root = dunce::canonicalize(project.path()).unwrap();
    assert_eq!(
        output["hookSpecificOutput"]["watchPaths"],
        serde_json::json!([root.join("src")])
    );
    assert!(
        output["systemMessage"]
            .as_str()
            .unwrap()
            .contains(".gitignore")
    );
    assert!(
        doctor_output(project.path())
            .contains("watch scan: incomplete; selected 1 top-level path(s)")
    );
}

#[test]
fn test_watch_scan_rejects_a_file_as_project_root() {
    let project = TempDir::new().unwrap();
    let file = project.path().join("file");
    fs::write(&file, "").unwrap();
    let output = session_start(&file);
    assert!(
        output["systemMessage"]
            .as_str()
            .unwrap()
            .contains("watch-path scan incomplete")
    );
    assert!(
        output["systemMessage"]
            .as_str()
            .unwrap()
            .contains("project root is not a directory")
    );
    let doctor = doctor_output(&file);
    assert!(doctor.contains("watch scan: incomplete"), "{doctor}");
    assert!(
        doctor.contains("project root is not a directory"),
        "{doctor}"
    );
}

#[test]
fn test_watch_scan_describes_hidden_only_tree_as_filtered() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join(".github")).unwrap();
    fs::create_dir(project.path().join(".git")).unwrap();
    let output = session_start(project.path());
    assert_eq!(
        output["hookSpecificOutput"]["watchPaths"],
        serde_json::json!([])
    );
    assert!(output.get("systemMessage").is_none());
    let doctor = doctor_output(project.path());
    assert!(
        doctor.contains("hidden entries excluded by default; ignore rules applied"),
        "{doctor}"
    );
}

#[test]
fn test_watch_scan_reports_explicitly_allowed_hidden_paths_through_cli() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join(".git")).unwrap();
    fs::create_dir(project.path().join(".github")).unwrap();
    fs::write(project.path().join(".gitignore"), "!.github/\n").unwrap();

    let output = session_start(project.path());
    let root = dunce::canonicalize(project.path()).unwrap();
    assert_eq!(
        output["hookSpecificOutput"]["watchPaths"],
        serde_json::json!([root.join(".github")])
    );
    assert!(output.get("systemMessage").is_none());
    let doctor = doctor_output(project.path());
    assert!(doctor.contains("selected 1 top-level path(s)"), "{doctor}");
    assert!(
        doctor.contains("hidden entries excluded by default"),
        "{doctor}"
    );
}

#[cfg(unix)]
#[test]
fn test_watch_scan_reports_unreadable_root_through_cli() {
    use std::os::unix::fs::PermissionsExt as _;
    let project = TempDir::new().unwrap();
    fs::set_permissions(project.path(), fs::Permissions::from_mode(0o000)).unwrap();
    let inaccessible = fs::read_dir(project.path()).is_err();
    let output = session_start(project.path());
    let doctor = doctor_output(project.path());
    fs::set_permissions(project.path(), fs::Permissions::from_mode(0o700)).unwrap();
    if inaccessible {
        assert!(
            output["systemMessage"]
                .as_str()
                .unwrap()
                .contains("watch-path scan incomplete")
        );
        assert!(doctor.contains("watch scan: incomplete"), "{doctor}");
    } else {
        assert!(
            output.get("systemMessage").is_none(),
            "privileged reader can inspect this directory"
        );
    }
}

#[cfg(unix)]
#[test]
fn test_doctor_identity_uses_current_user_without_xdg_runtime_dir() {
    let project = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    let output = clear_ambient_env(&mut cmd)
        .env("CLAUDE_PROJECT_DIR", project.path())
        .env("USER", "mcpls-followup-user")
        .args(["hook", "doctor"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let expected = std::env::temp_dir().join("mcpls-mcpls-followup-user");
    assert!(
        stdout.starts_with(&format!("socket: {}/", expected.display())),
        "{stdout}"
    );
}

#[cfg(unix)]
#[test]
fn test_doctor_identity_rejects_socket_path_that_cannot_bind() {
    let project = TempDir::new().unwrap();
    let runtime = project.path().join("x".repeat(120));
    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    let output = clear_ambient_env(&mut cmd)
        .env("CLAUDE_PROJECT_DIR", project.path())
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", &runtime)
        .env("USER", "mcpls-test")
        .args(["hook", "doctor"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with("socket: none; could not derive an identity"),
        "{stdout}"
    );
    assert!(stdout.contains("socket path exceeds"), "{stdout}");
}

#[tokio::test]
async fn test_hook_context_outputs_name_the_triggering_event_through_cli() {
    use mcpls_core::hooks::{HookListener, Request, Response};
    let project = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    #[cfg(windows)]
    let identity = mcpls_core::hooks::identity_for(project.path()).unwrap();
    // The child derives its socket from the temporary directory set below,
    // which `identity_for` in this process would not read.
    #[cfg(not(windows))]
    let identity = {
        let hash = mcpls_core::hooks::identity_hash(project.path()).unwrap();
        mcpls_core::hooks::SocketIdentity {
            socket: runtime
                .path()
                .join("mcpls-mcpls-test")
                .join(format!("{hash}.sock")),
            lock: runtime
                .path()
                .join("mcpls-mcpls-test")
                .join(format!("{hash}.lock")),
            hash,
        }
    };
    let listener = HookListener::acquire(&identity).await.unwrap().unwrap();
    let (cancel, rx) = tokio::sync::watch::channel(false);
    let owner = tokio::spawn(listener.serve(
        |request| {
            Box::pin(async move {
                match request {
                    Request::Changed { .. } => Response::Changed { queued: 0 },
                    Request::Flush { .. } => Response::Flush {
                        context: Some("diagnostic".into()),
                        token: None,
                    },
                    _ => unreachable!(),
                }
            })
        },
        Duration::from_secs(1),
        rx,
    ));
    tokio::task::spawn_blocking(move || {
        for event in ["UserPromptSubmit", "PostToolBatch"] {
            let mut cmd = Command::cargo_bin("mcpls").unwrap();
            clear_ambient_env(&mut cmd);
            let output = assert_cmd::Command::from_std(cmd)
                .env("CLAUDE_PROJECT_DIR", project.path())
                .env_remove("XDG_RUNTIME_DIR")
                .env("TMPDIR", runtime.path())
                .env("USER", "mcpls-test")
                .arg("hook")
                .write_stdin(
                    serde_json::json!({"hook_event_name": event, "session_id": "test"}).to_string(),
                )
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
            let parsed: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(
                parsed,
                serde_json::json!({"hookSpecificOutput": {
                    "hookEventName": event, "additionalContext": "diagnostic"
                }})
            );
        }
    })
    .await
    .unwrap();
    cancel.send(true).unwrap();
    owner.await.unwrap();
}

/// A hook invocation must never panic on a closed stdout, the same
/// guarantee `test_completions_survives_a_closed_pipe` proves for
/// `completions`: a hook branch that wrote with `print!` would panic past
/// the `LineWriter`'s buffer, so the project directory here has enough
/// top-level entries to force a real write rather than one that sits in
/// that buffer until the ignored exit-time flush.
#[cfg(unix)]
#[test]
fn test_hook_survives_a_closed_pipe() {
    use std::io::Write as _;
    use std::process::Stdio;

    let temp_dir = TempDir::new().unwrap();
    for i in 0..200 {
        fs::create_dir_all(temp_dir.path().join(format!("dir{i}"))).unwrap();
    }

    let mut departed_reader = Command::new("true").stdin(Stdio::piped()).spawn().unwrap();
    let closed_pipe = departed_reader.stdin.take().unwrap();
    departed_reader.wait().unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    let mut child = clear_ambient_env(&mut cmd)
        .arg("hook")
        .env("CLAUDE_PROJECT_DIR", temp_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::from(closed_pipe))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"SessionStart"}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("panicked"),
        "a closed pipe must not panic: {stderr}"
    );
    assert!(
        output.status.success(),
        "a closed pipe is a clean exit, got {:?}: {stderr}",
        output.status
    );
}
