//! Tests the schema subcommand's stdout contract.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use mcpls_core::config::schema::document;

#[test]
fn schema_subcommand_prints_the_library_document_without_logs() {
    let output = Command::cargo_bin("mcpls")
        .expect("locate mcpls binary")
        .arg("schema")
        .output()
        .expect("run `mcpls schema`");

    assert!(
        output.status.success(),
        "`mcpls schema` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "schema command writes no logs");
    assert_eq!(
        String::from_utf8(output.stdout).expect("schema output is UTF-8"),
        document().expect("generate config schema")
    );
}

#[test]
fn schema_init_creates_a_default_config_with_the_latest_schema_link() {
    let directory = tempfile::tempdir().expect("create temporary working directory");
    let output = Command::cargo_bin("mcpls")
        .expect("locate mcpls binary")
        .args(["schema", "init"])
        .current_dir(directory.path())
        .output()
        .expect("run `mcpls schema init`");

    assert!(
        output.status.success(),
        "`mcpls schema init` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "schema init writes no logs");
    let stdout = String::from_utf8(output.stdout).expect("init output is UTF-8");
    assert!(stdout.contains("checkout root"));
    assert!(stdout.contains("--trust-project-config"));
    assert!(stdout.contains("MCPLS_TRUST_PROJECT_CONFIG"));

    let config_path = directory.path().join("mcpls.toml");
    let contents = std::fs::read_to_string(config_path).expect("read generated config");
    assert!(contents.starts_with(
        "#:schema https://github.com/AbysmalBiscuit/mcpls/releases/latest/download/mcpls-config.json\n"
    ));
    assert!(contents.contains("# mcpls configuration"));
    toml::from_str::<toml::Value>(
        contents
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
            .as_str(),
    )
    .expect("generated default config is valid TOML");
}

#[test]
fn schema_init_leaves_an_existing_config_untouched() {
    let directory = tempfile::tempdir().expect("create temporary working directory");
    let config_path = directory.path().join("mcpls.toml");
    let original = "[workspace]\nroots = [\"keep me\"]\n";
    std::fs::write(&config_path, original).expect("create existing config");

    let output = Command::cargo_bin("mcpls")
        .expect("locate mcpls binary")
        .args(["schema", "init"])
        .current_dir(directory.path())
        .output()
        .expect("run `mcpls schema init` against existing config");

    assert!(
        !output.status.success(),
        "existing config must not be replaced"
    );
    assert_eq!(
        std::fs::read_to_string(config_path).expect("read existing config"),
        original
    );
}
