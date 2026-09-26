//! `docs/user-guide/config-reference.md`, rendered from the JSON Schema so
//! the reference a user reads and the hover text an editor shows come from
//! the same doc comments.

use std::fmt::Write;

use serde_json::{Map, Value};

const HEADER: &str = "# Configuration reference

Every key mcpls reads from `mcpls.toml`, with its type, default, and meaning. \
Generated from the JSON Schema `mcpls schema` prints; regenerate it with `devrun task schema`, \
or `MCPLS_UPDATE_SCHEMA=1 cargo test -p mcpls-core --test config_schema`.

Which file mcpls loads, and when it trusts a checkout's own `mcpls.toml`, is in \
`mcpls help --full` under `--config` and `--trust-project-config`. `mcpls config --origin` \
prints the configuration resolved for a directory and the file each setting came from.
";

/// The reference document, with a trailing newline.
///
/// # Errors
///
/// Returns an error if the schema cannot be generated.
pub fn document() -> Result<String, serde_json::Error> {
    let schema = super::schema::value()?;
    let mut reference = Reference {
        defs: &schema["$defs"],
        out: HEADER.to_owned(),
    };
    for (key, prop) in properties(&schema) {
        reference.section(key, prop);
    }
    Ok(reference.out)
}

struct Reference<'a> {
    defs: &'a Value,
    out: String,
}

impl<'a> Reference<'a> {
    /// A table, or an array of tables, with its keys and then its nested
    /// tables.
    fn section(&mut self, path: &str, prop: &'a Value) {
        let array = self.array_of_tables(prop);
        let schema = self.resolve(array.unwrap_or(prop));
        let (open, close) = if array.is_some() {
            ("[[", "]]")
        } else {
            ("[", "]")
        };
        let level = "#".repeat(2 + path.matches('.').count());
        let _ = writeln!(self.out, "\n{level} `{open}{path}{close}`\n");
        if let Some(text) = self.description(prop) {
            let _ = writeln!(self.out, "{}\n", text.join("\n\n"));
        }
        if let Some(default) = prop.get("default") {
            self.block_default(default);
        }

        let mut nested = Vec::new();
        for (key, child) in properties(schema) {
            if self.is_table(child) || self.array_of_tables(child).is_some() {
                nested.push((format!("{path}.{key}"), child));
            } else {
                self.key(key, child);
            }
        }
        for (path, child) in nested {
            self.section(&path, child);
        }
    }

    fn key(&mut self, key: &str, prop: &'a Value) {
        let mut line = format!("- `{key}` ({}", self.type_name(prop));
        if let Some(default) = prop.get("default") {
            let _ = write!(line, ", default `{default}`");
        }
        line.push(')');
        let paragraphs = self.description(prop).unwrap_or_default();
        if let Some((first, rest)) = paragraphs.split_first() {
            let _ = write!(line, ": {first}");
            for paragraph in rest {
                let _ = write!(line, "\n\n  {paragraph}");
            }
        }
        let _ = writeln!(self.out, "{line}");

        if let Some(form) = self.table_form(prop) {
            for (key, field) in properties(form) {
                let mut line = format!("  - `{key}` ({})", self.type_name(field));
                if let Some(text) = self.description(field) {
                    let _ = write!(line, ": {}", text.join(" "));
                }
                let _ = writeln!(self.out, "{line}");
            }
        }
    }

    /// A default too long for one line, such as the built-in extension
    /// mappings, as one array element per line.
    fn block_default(&mut self, default: &Value) {
        let Some(items) = default.as_array().filter(|items| !items.is_empty()) else {
            let _ = writeln!(self.out, "Default: `{default}`.\n");
            return;
        };
        let _ = writeln!(self.out, "Default:\n\n```json\n[");
        for (index, item) in items.iter().enumerate() {
            let comma = if index + 1 < items.len() { "," } else { "" };
            let _ = writeln!(self.out, "  {item}{comma}");
        }
        let _ = writeln!(self.out, "]\n```\n");
    }

    /// The table branch of a key that also accepts a bare value, such as an
    /// install command written as one string or as a table per OS.
    fn table_form(&self, prop: &'a Value) -> Option<&'a Value> {
        branches(self.resolve(prop))?
            .into_iter()
            .map(|branch| self.resolve(branch))
            .find(|branch| branch.get("properties").is_some())
    }

    fn array_of_tables(&self, prop: &'a Value) -> Option<&'a Value> {
        let items = self.resolve(prop).get("items")?;
        self.is_table(items).then_some(items)
    }

    fn is_table(&self, prop: &'a Value) -> bool {
        self.resolve(prop).get("properties").is_some()
    }

    /// Follows a `$ref`, including through the `anyOf` that pairs an
    /// optional value with `null`.
    fn resolve(&self, prop: &'a Value) -> &'a Value {
        if let Some(name) = def_name(prop) {
            return self.resolve(&self.defs[name]);
        }
        match branches(prop).as_deref() {
            Some([only]) => self.resolve(only),
            _ => prop,
        }
    }

    /// The description, one string per paragraph, with the doc comment's
    /// line wrapping undone.
    fn description(&self, prop: &'a Value) -> Option<Vec<String>> {
        let text = prop
            .get("description")
            .or_else(|| self.resolve(prop).get("description"))?
            .as_str()?;
        Some(
            text.split("\n\n")
                .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect(),
        )
    }

    fn type_name(&self, prop: &'a Value) -> String {
        let schema = self.resolve(prop);
        if let Some(values) = schema["oneOf"].as_array() {
            return values
                .iter()
                .map(|value| value["const"].to_string())
                .collect::<Vec<_>>()
                .join(" | ");
        }
        if let Some(branches) = branches(schema) {
            return branches
                .into_iter()
                .map(|branch| self.type_name(branch))
                .collect::<Vec<_>>()
                .join(" or ");
        }
        let name = match &schema["type"] {
            Value::Array(names) => names.iter().find(|name| *name != "null"),
            name => Some(name),
        };
        match name.and_then(Value::as_str) {
            Some("array") => format!("array of {}", self.type_name(&schema["items"])),
            Some("object") => match schema.get("additionalProperties") {
                Some(values) if values.is_object() => {
                    format!("table of {}", self.type_name(values))
                }
                _ => "table".to_owned(),
            },
            Some(name) => name.to_owned(),
            None => "any".to_owned(),
        }
    }
}

fn properties(schema: &Value) -> impl Iterator<Item = (&str, &Value)> {
    schema["properties"]
        .as_object()
        .into_iter()
        .flat_map(Map::iter)
        .map(|(key, value)| (key.as_str(), value))
}

/// The `anyOf` branches other than `null`.
fn branches(schema: &Value) -> Option<Vec<&Value>> {
    let branches = schema.get("anyOf")?.as_array()?;
    Some(
        branches
            .iter()
            .filter(|branch| branch.get("type") != Some(&"null".into()))
            .collect(),
    )
}

fn def_name(prop: &Value) -> Option<&str> {
    prop.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}
