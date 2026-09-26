//! `mcpls lsp install` run from a plain shell against a fresh checkout,
//! with no backend anywhere.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt as _;
use tempfile::TempDir;

/// Creates the fake server under the working directory. Both variants
/// quote their paths with double quotes, which Windows argument quoting
/// would mangle on the way into `powershell -Command`.
const CREATES_THE_SERVER: &str = r##"[lsp_servers.install]
unix = 'mkdir -p "bin" && printf "#!/bin/sh\n" > "bin/fake-ls" && chmod +x "bin/fake-ls"'
windows = 'New-Item -ItemType Directory -Force "bin" | Out-Null; Set-Content -Path "bin/fake-ls.cmd" -Value "@echo off"'
"##;

/// A checkout with its own runtime directory and user name, so a run here
/// can never meet a real backend or another test's.
fn checkout() -> (TempDir, TempDir) {
    let dir = TempDir::new().unwrap();
    let root = dunce::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(root.join("marker.fake"), "").unwrap();
    (dir, short_temp_dir())
}

/// macOS's `$TMPDIR` is deep enough that a runtime directory inside it
/// pushes the socket path past the `sun_path` limit.
fn short_temp_dir() -> TempDir {
    #[cfg(unix)]
    let dir = tempfile::Builder::new().tempdir_in("/tmp");
    #[cfg(not(unix))]
    let dir = TempDir::new();
    dir.unwrap()
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
        .env("USER", format!("mcpls-install-test-{}", std::process::id()))
        .env(
            "USERNAME",
            format!("mcpls-install-test-{}", std::process::id()),
        )
        .current_dir(cwd)
        .args(args);
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Where the install command puts the fake server.
fn fake_program(root: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "fake-ls.cmd"
    } else {
        "fake-ls"
    };
    root.join("bin").join(name)
}

/// A config with one server, `fake`, that applies to the checkout and
/// runs `fake_program(root)`. `install` is TOML placed inside the entry.
fn write_config(root: &Path, install: Option<&str>) -> PathBuf {
    let config = root.join("test-mcpls.toml");
    std::fs::write(
        &config,
        format!(
            "[[lsp_servers]]\nlanguage_id = \"fake\"\ncommand = {:?}\n\
             file_patterns = [\"**/*.fake\"]\n{}\n\
             [lsp_servers.heuristics]\nproject_markers = [\"marker.fake\"]\n",
            fake_program(root).display().to_string(),
            install.unwrap_or_default(),
        ),
    )
    .unwrap();
    config
}

fn install(cwd: &Path, runtime: &Path, config: &Path, args: &[&str]) -> Output {
    let config = config.display().to_string();
    let mut all = vec!["--config", &config, "lsp", "install"];
    all.extend_from_slice(args);
    run(cwd, runtime, &all)
}

#[test]
fn install_runs_the_command_and_reports_the_server_installed() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = write_config(&root, Some(CREATES_THE_SERVER));

    let output = install(&root, runtime.path(), &config, &["fake"]);
    let report = stdout(&output);

    assert!(output.status.success(), "{report}");
    assert!(report.contains("==> fake: "), "{report}");
    assert!(report.ends_with("fake  installed\n"), "{report}");
    assert!(fake_program(&root).is_file());
}

#[test]
fn install_from_a_subdirectory_runs_in_the_checkout_root() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let sub = root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let config = write_config(&root, Some(CREATES_THE_SERVER));

    let output = install(&sub, runtime.path(), &config, &["fake"]);

    assert!(output.status.success(), "{}", stdout(&output));
    assert!(fake_program(&root).is_file());
    assert!(!sub.join("bin").exists());
}

#[test]
fn a_failing_install_command_fails_the_run() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = write_config(&root, Some("install = \"exit 3\""));

    let output = install(&root, runtime.path(), &config, &["fake"]);
    let report = stdout(&output);

    assert!(!output.status.success(), "{report}");
    assert!(report.ends_with("fake  failed (exit 3)\n"), "{report}");
}

#[test]
fn an_install_that_leaves_the_binary_missing_says_to_open_a_new_shell() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = write_config(&root, Some("install = \"echo done\""));

    let output = install(&root, runtime.path(), &config, &["fake"]);
    let report = stdout(&output);

    assert!(!output.status.success(), "{report}");
    assert!(
        report.ends_with(&format!(
            "fake  installed, but {} is still not on PATH; open a new shell\n",
            fake_program(&root).display()
        )),
        "{report}"
    );
}

#[test]
fn dry_run_prints_the_command_and_runs_nothing() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = write_config(&root, Some(CREATES_THE_SERVER));

    let output = install(&root, runtime.path(), &config, &["--all", "--dry-run"]);
    let report = stdout(&output);

    assert!(output.status.success(), "{report}");
    assert!(report.contains("==> fake: "), "{report}");
    assert!(report.ends_with("fake  would run\n"), "{report}");
    assert!(!fake_program(&root).exists());
}

#[test]
fn an_installed_server_is_skipped() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let program = fake_program(&root);
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    std::fs::write(&program, "").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = write_config(&root, Some("install = \"exit 3\""));

    let output = install(&root, runtime.path(), &config, &["fake"]);

    assert!(output.status.success(), "{}", stdout(&output));
    assert_eq!(stdout(&output), "fake  already installed\n");
}

#[test]
fn a_named_server_without_an_install_command_fails_but_all_does_not() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = write_config(&root, None);

    let named = install(&root, runtime.path(), &config, &["fake"]);
    assert!(!named.status.success(), "{}", stdout(&named));
    assert_eq!(stdout(&named), "fake  no install command\n");

    let all = install(&root, runtime.path(), &config, &["--all"]);
    assert!(all.status.success(), "{}", stdout(&all));
    assert_eq!(stdout(&all), "fake  no install command\n");
}

/// A fresh checkout's own `mcpls.toml` is the config most likely to carry
/// install commands, and it is ignored until trusted.
#[test]
fn install_names_a_project_config_it_ignored() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let planted = root.join("mcpls.toml");
    std::fs::rename(write_config(&root, Some(CREATES_THE_SERVER)), &planted).unwrap();

    let output = run(
        &root,
        runtime.path(),
        &["lsp", "install", "--all", "--dry-run"],
    );
    let report = stdout(&output);

    assert!(
        report.contains(&format!(
            "project config: {} was found and ignored; pass --trust-project-config",
            planted.display()
        )),
        "{report}"
    );
    assert!(!report.contains("fake"), "{report}");
}

#[test]
fn an_unknown_server_names_the_ones_that_apply() {
    let (project, runtime) = checkout();
    let root = dunce::canonicalize(project.path()).unwrap();
    let config = write_config(&root, Some(CREATES_THE_SERVER));

    let output = install(&root, runtime.path(), &config, &["nope"]);

    assert!(!output.status.success());
    assert_eq!(
        stdout(&output),
        "no language server named nope applies here; these do: fake\n"
    );
}
