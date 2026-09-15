//! Guards configuration defaults against TOML deserialization drift.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fmt::Debug;

use mcpls_core::config::{
    ApplyConfig, BackendConfig, DiagnosticsConfig, HooksConfig, PartialLspServerConfig,
    ServerConfig, ServerHeuristics, WorkspaceConfig,
};
use serde::de::DeserializeOwned;

fn assert_empty_toml_matches_default<T>()
where
    T: Debug + Default + DeserializeOwned,
{
    let parsed = toml::from_str::<T>("").unwrap_or_else(|error| {
        panic!(
            "deserialize empty TOML as {}: {error}",
            std::any::type_name::<T>()
        )
    });
    assert_eq!(
        format!("{parsed:?}"),
        format!("{:?}", T::default()),
        "empty TOML must resolve to Default for {}",
        std::any::type_name::<T>()
    );
}

#[test]
fn empty_tables_deserialize_to_their_declared_defaults() {
    assert_empty_toml_matches_default::<ApplyConfig>();
    assert_empty_toml_matches_default::<DiagnosticsConfig>();
    assert_empty_toml_matches_default::<HooksConfig>();
    assert_empty_toml_matches_default::<BackendConfig>();
    assert_empty_toml_matches_default::<WorkspaceConfig>();
    assert_empty_toml_matches_default::<ServerConfig>();
    assert_empty_toml_matches_default::<ServerHeuristics>();
    assert_empty_toml_matches_default::<PartialLspServerConfig>();
}

#[test]
fn explicit_workspace_table_keeps_builtin_language_extensions_when_omitted() {
    let config =
        toml::from_str::<ServerConfig>("[workspace]\n").expect("empty workspace table parses");

    assert_eq!(
        format!("{:?}", config.workspace.language_extensions),
        format!("{:?}", WorkspaceConfig::default().language_extensions)
    );
}

#[test]
fn toml_11_inline_tables_accept_a_trailing_comma() {
    let config = toml::from_str::<ServerConfig>(
        r#"
[[lsp_servers]]
language_id = "rust"
initialization_options = { feature = "toml-1.1", }
"#,
    )
    .expect("TOML 1.1 inline table with trailing comma parses");

    let rust = config
        .lsp_servers
        .iter()
        .find(|server| server.language_id == "rust")
        .expect("built-in Rust server remains configured");
    assert_eq!(
        rust.initialization_options.as_ref().unwrap()["feature"],
        "toml-1.1"
    );
}

#[test]
fn language_extension_mappings_reject_unknown_fields() {
    let result = toml::from_str::<ServerConfig>(
        r#"
[[workspace.language_extensions]]
extensions = ["custom"]
language_id = "rust"
unexpected = true
"#,
    );

    assert!(result.is_err(), "unknown mapping fields must be rejected");
}
