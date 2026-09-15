//! JSON Schema document for the TOML configuration file.

use std::collections::HashSet;

use serde::ser::Error as _;
use serde_json::{Map, Value};

/// Stable URL used by Taplo and release assets to locate the config schema.
pub const SCHEMA_ID: &str =
    "https://github.com/AbysmalBiscuit/mcpls/releases/latest/download/mcpls-config.json";

/// Generate the JSON Schema used by editors and published release assets.
///
/// # Errors
///
/// Returns an error if Schemars generates an unexpected schema shape or JSON
/// serialization fails.
pub fn document() -> Result<String, serde_json::Error> {
    let mut schema = serde_json::to_value(schemars::schema_for!(super::ServerConfig))?;
    let object = schema
        .as_object_mut()
        .ok_or_else(|| serde_json::Error::custom("ServerConfig schema is not an object"))?;
    object.insert("$id".into(), SCHEMA_ID.into());
    object.insert("title".into(), "mcpls.toml".into());
    object.insert(
        "description".into(),
        "Configuration for mcpls, written in TOML 1.1.".into(),
    );

    remove_composite_defaults(&mut schema)?;

    Ok(format!("{}\n", serde_json::to_string_pretty(&schema)?))
}

fn remove_composite_defaults(schema: &mut Value) -> Result<(), serde_json::Error> {
    let (root_table_fields, root_table_definitions) = {
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| serde_json::Error::custom("ServerConfig schema has no properties"))?;
        let mut fields = HashSet::new();
        let mut definitions = Vec::new();
        for (field, property) in properties {
            if let Some(definition) = object_definition_name(schema, property) {
                fields.insert(field.clone());
                definitions.push(definition);
            }
        }
        (fields, definitions)
    };

    {
        let properties = schema
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| serde_json::Error::custom("ServerConfig schema has no properties"))?;
        for (field, property) in properties {
            if field == "lsp_servers" {
                remove_defaults_recursively(property);
            } else if root_table_fields.contains(field) {
                property_object_mut_field(property, "ServerConfig", field)?.remove("default");
            }
        }
    }

    let mut visited_tables = HashSet::new();
    for definition in root_table_definitions {
        remove_nested_table_defaults(schema, &definition, &mut visited_tables)?;
    }

    remove_lsp_defaults(schema)?;
    Ok(())
}

fn remove_nested_table_defaults(
    schema: &mut Value,
    definition: &str,
    visited: &mut HashSet<String>,
) -> Result<(), serde_json::Error> {
    if !visited.insert(definition.to_owned()) {
        return Ok(());
    }

    let nested_tables = {
        let properties = definition_properties(schema, definition)?;
        properties
            .iter()
            .filter_map(|(field, property)| {
                object_definition_name(schema, property).map(|nested| (field.clone(), nested))
            })
            .collect::<Vec<_>>()
    };

    for (field, nested) in nested_tables {
        {
            let properties = definition_properties_mut(schema, definition)?;
            let property = properties.get_mut(&field).ok_or_else(|| {
                serde_json::Error::custom(format!("{definition} schema has no {field} property"))
            })?;
            property_object_mut_field(property, definition, &field)?.remove("default");
        }
        remove_nested_table_defaults(schema, &nested, visited)?;
    }

    Ok(())
}

fn remove_lsp_defaults(schema: &mut Value) -> Result<(), serde_json::Error> {
    let mut pending = {
        let property = schema
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get("lsp_servers"))
            .ok_or_else(|| serde_json::Error::custom("ServerConfig schema has no lsp_servers"))?;
        let mut references = Vec::new();
        collect_definition_refs(property, &mut references);
        references
    };

    let property = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut("lsp_servers"))
        .ok_or_else(|| serde_json::Error::custom("ServerConfig schema has no lsp_servers"))?;
    remove_defaults_recursively(property);

    let mut visited = HashSet::new();
    while let Some(name) = pending.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }

        let nested_references = {
            let definition = schema
                .get("$defs")
                .and_then(Value::as_object)
                .and_then(|definitions| definitions.get(&name))
                .ok_or_else(|| {
                    serde_json::Error::custom(format!(
                        "schema references missing definition {name}"
                    ))
                })?;
            let mut references = Vec::new();
            collect_definition_refs(definition, &mut references);
            references
        };
        pending.extend(nested_references);

        let definition = schema
            .get_mut("$defs")
            .and_then(Value::as_object_mut)
            .and_then(|definitions| definitions.get_mut(&name))
            .ok_or_else(|| {
                serde_json::Error::custom(format!("schema references missing definition {name}"))
            })?;
        remove_defaults_recursively(definition);
    }

    Ok(())
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

fn remove_defaults_recursively(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("default");
            for child in object.values_mut() {
                remove_defaults_recursively(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                remove_defaults_recursively(item);
            }
        }
        _ => {}
    }
}

fn object_definition_name(schema: &Value, property: &Value) -> Option<String> {
    let reference = property.get("$ref")?.as_str()?;
    let name = reference.strip_prefix("#/$defs/")?;
    let definition = schema.get("$defs")?.get(name)?;
    (definition.get("type").and_then(Value::as_str) == Some("object")).then(|| name.to_owned())
}

fn definition_properties<'a>(
    schema: &'a Value,
    definition: &str,
) -> Result<&'a Map<String, Value>, serde_json::Error> {
    schema
        .get("$defs")
        .and_then(Value::as_object)
        .and_then(|definitions| definitions.get(definition))
        .and_then(|definition| definition.get("properties"))
        .and_then(Value::as_object)
        .ok_or_else(|| serde_json::Error::custom(format!("{definition} schema has no properties")))
}

fn definition_properties_mut<'a>(
    schema: &'a mut Value,
    definition: &str,
) -> Result<&'a mut Map<String, Value>, serde_json::Error> {
    schema
        .get_mut("$defs")
        .and_then(Value::as_object_mut)
        .and_then(|definitions| definitions.get_mut(definition))
        .and_then(|definition| definition.get_mut("properties"))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| serde_json::Error::custom(format!("{definition} schema has no properties")))
}

fn property_object_mut_field<'a>(
    property: &'a mut Value,
    definition: &str,
    field: &str,
) -> Result<&'a mut Map<String, Value>, serde_json::Error> {
    property.as_object_mut().ok_or_else(|| {
        serde_json::Error::custom(format!("{definition}.{field} schema is not an object"))
    })
}
