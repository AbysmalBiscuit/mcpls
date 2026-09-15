//! Keeps the published config schema aligned with the TOML loader.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;
use std::path::PathBuf;

use mcpls_core::config::schema::document;
use serde_json::{Value, json};

fn committed_schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schema/mcpls-config.json")
}

fn generated_schema() -> String {
    document().expect("generate config schema")
}

fn schema_value() -> Value {
    serde_json::from_str(&generated_schema()).expect("schema document is valid JSON")
}

fn collect_definition_refs(value: &Value, references: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                && let Some(name) = reference.strip_prefix("#/$defs/")
            {
                references.push(name.to_owned());
            }
            for child in object.values() {
                collect_definition_refs(child, references);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_definition_refs(item, references);
            }
        }
        _ => {}
    }
}

fn assert_no_schema_defaults(value: &Value, path: &str) {
    match value {
        Value::Object(object) => {
            assert!(
                !object.contains_key("default"),
                "{path} must not carry a default"
            );
            for (name, child) in object {
                assert_no_schema_defaults(child, &format!("{path}.{name}"));
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                assert_no_schema_defaults(item, &format!("{path}[{index}]"));
            }
        }
        _ => {}
    }
}

fn assert_lsp_schema_has_no_defaults(schema: &Value) {
    let lsp_servers = &schema["properties"]["lsp_servers"];
    let definitions = schema["$defs"].as_object().expect("schema definitions");
    let mut pending = Vec::new();
    let mut visited = HashSet::new();

    assert_no_schema_defaults(lsp_servers, "properties.lsp_servers");
    collect_definition_refs(lsp_servers, &mut pending);

    while let Some(name) = pending.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let definition = &definitions[&name];
        assert_no_schema_defaults(definition, &format!("$defs.{name}"));
        collect_definition_refs(definition, &mut pending);
    }
}

fn first_differing_line(committed: &str, generated: &str) -> String {
    if let Some((index, (left, right))) = committed
        .lines()
        .zip(generated.lines())
        .enumerate()
        .find(|(_, (left, right))| left != right)
    {
        format!(
            "line {}:\n  committed: {left}\n  generated: {right}",
            index + 1
        )
    } else {
        "the files differ in line count or trailing newline".to_owned()
    }
}

#[test]
fn committed_schema_matches_the_config_types() {
    let generated = generated_schema();
    let path = committed_schema_path();
    if std::env::var("MCPLS_UPDATE_SCHEMA").as_deref() == Ok("1") {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
        }
        std::fs::write(path, generated).expect("write generated schema");
        return;
    }

    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    if committed == generated {
        return;
    }

    panic!(
        "schema/mcpls-config.json is stale; first difference:\n{}\n\
         regenerate with `MCPLS_UPDATE_SCHEMA=1 cargo test -p mcpls-core --test config_schema` \
         or `cargo run --bin mcpls -- schema > schema/mcpls-config.json`",
        first_differing_line(&committed, &generated)
    );
}

#[test]
fn schema_has_file_metadata_and_no_table_defaults() {
    let schema = schema_value();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(
        schema["$id"],
        "https://github.com/AbysmalBiscuit/mcpls/releases/latest/download/mcpls-config.json"
    );
    assert_eq!(schema["title"], "mcpls.toml");
    assert_eq!(schema["additionalProperties"], false);
    assert!(schema.get("required").is_none());

    let root = schema["properties"].as_object().expect("root properties");
    for field in [
        "workspace",
        "lsp_servers",
        "apply",
        "diagnostics",
        "backend",
    ] {
        assert!(root.contains_key(field), "missing config field {field}");
    }
    for (field, property) in root {
        assert!(
            property.get("default").is_none(),
            "root table defaults duplicate their field defaults: {field}"
        );
    }
    assert!(!root.contains_key("source"));
    assert!(!root.contains_key("project_config_ignored"));
}

#[test]
fn schema_exposes_truthful_field_defaults() {
    let schema = schema_value();
    let definitions = schema["$defs"].as_object().expect("schema definitions");
    let field_defaults = [
        (
            "WorkspaceConfig",
            [
                ("roots", json!([])),
                ("position_encodings", json!(["utf-8", "utf-16"])),
                ("heuristics_max_depth", json!(10)),
                ("max_documents", json!(100)),
                ("max_file_size", json!(10_485_760)),
            ]
            .into_iter()
            .collect::<Vec<_>>(),
        ),
        (
            "ApplyConfig",
            [
                ("rename", json!(false)),
                ("format_document", json!(false)),
                ("code_actions", json!(false)),
                ("allow_file_deletion", json!(false)),
            ]
            .into_iter()
            .collect(),
        ),
        (
            "DiagnosticsConfig",
            [
                ("severity", json!("warning")),
                ("max_per_file", json!(10)),
                ("max_total", json!(50)),
                ("settle_quiet_ms", json!(1_000)),
                ("settle_deadline_ms", json!(300_000)),
                ("footer", json!(false)),
                ("footer_grace_ms", json!(250)),
                ("footer_quiet_ms", json!(200)),
                ("footer_wait_ms", json!(15_000)),
            ]
            .into_iter()
            .collect(),
        ),
        (
            "HooksConfig",
            [
                ("enabled", json!(true)),
                ("sweep_quiet_ms", json!(500)),
                ("op_deadline_ms", json!(1_500)),
            ]
            .into_iter()
            .collect(),
        ),
        (
            "BackendConfig",
            [
                ("idle_shutdown_ms", json!(10_000)),
                ("spawn", json!("lazy")),
            ]
            .into_iter()
            .collect(),
        ),
    ];

    for (definition, expected) in field_defaults {
        let properties = definitions[definition]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{definition} properties"));
        for (field, default) in expected {
            assert_eq!(
                properties[field]["default"], default,
                "wrong or missing default for {definition}.{field}"
            );
        }
    }

    let language_extensions =
        &definitions["WorkspaceConfig"]["properties"]["language_extensions"]["default"];
    assert_eq!(
        language_extensions[0],
        json!({"extensions": ["rs"], "language_id": "rust"})
    );

    assert!(
        definitions["DiagnosticsConfig"]["properties"]["hooks"]
            .get("default")
            .is_none(),
        "hooks uses its own field defaults"
    );
}

#[test]
fn schema_describes_server_overlays_and_rejects_unknown_fields() {
    let schema = schema_value();
    let root = schema["properties"].as_object().expect("root properties");
    let definitions = schema["$defs"].as_object().expect("schema definitions");
    let lsp_servers = &root["lsp_servers"];
    let lsp_description = lsp_servers["description"]
        .as_str()
        .expect("lsp_servers description");
    assert!(lsp_description.contains("built-in"));
    assert!(lsp_description.contains("overlay"));
    assert!(lsp_description.contains("name`, or `language_id`"));
    assert!(lsp_description.contains("later entry"));
    assert!(lsp_servers.get("default").is_none());
    assert_lsp_schema_has_no_defaults(&schema);

    for definition in [
        "LanguageExtensionMapping",
        "WorkspaceConfig",
        "ApplyConfig",
        "DiagnosticsConfig",
        "HooksConfig",
        "BackendConfig",
        "PartialLspServerConfig",
        "ServerHeuristics",
    ] {
        assert_eq!(
            definitions[definition]["additionalProperties"], false,
            "{definition} rejects unknown fields"
        );
    }
}
