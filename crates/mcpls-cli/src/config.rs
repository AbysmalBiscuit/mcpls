//! The configuration mcpls resolved for a directory.
//!
//! Discovery runs four tiers, a trusted project config replaces the global
//! one rather than merging with it, and partial `[[lsp_servers]]` entries
//! fold onto the built-ins. The result of all that is a value no other
//! command prints: the doctor's `config:` line gives a fingerprint, which
//! tells a reader that two builds disagree and nothing about how.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context as _, Result};
use mcpls_core::{ConfigSource, Resolved};

use crate::hook::Examined;

/// The tier a configuration was loaded from, as the report names it.
const fn tier(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::Explicit => "explicit (named with --config or MCPLS_CONFIG)",
        ConfigSource::Project => "project (this checkout's own mcpls.toml, trusted)",
        ConfigSource::Global => "global (the user's config file)",
        ConfigSource::Defaults => "defaults (no configuration file applied)",
    }
}

/// The resolved configuration, rendered for a reader or for a script.
///
/// # Errors
///
/// Returns an error if a setting cannot be represented in the chosen
/// format, or if the source file is unreadable while `origin` is set.
pub fn render(
    resolved: &Resolved,
    directory: &Path,
    examined: Examined,
    origin: bool,
    json: bool,
) -> Result<String> {
    let lines = match (origin, json) {
        (true, false) => header(resolved, directory, examined)
            .into_iter()
            .chain(origin_lines(resolved)?)
            .collect(),
        (true, true) => vec![serde_json::to_string_pretty(&origin_json(
            resolved, directory, examined,
        )?)?],
        // Plain `--json` stays a bare config object: a script deserializes
        // it straight back into a configuration, and a wrapper would break
        // that.
        (false, true) => vec![serde_json::to_string_pretty(&resolved.config)?],
        (false, false) => header(resolved, directory, examined)
            .into_iter()
            .chain(std::iter::once(
                resolved
                    .config
                    .to_toml()
                    .context("rendering the configuration as TOML")?
                    .trim_end()
                    .to_string(),
            ))
            .collect(),
    };
    Ok(lines.join("\n") + "\n")
}

/// Where the configuration came from, as TOML comments, so the output
/// stays a file a user could save while saying what produced it.
fn header(resolved: &Resolved, directory: &Path, examined: Examined) -> Vec<String> {
    let mut out = vec![
        format!(
            "# directory: {} ({})",
            directory.display(),
            examined.origin()
        ),
        format!("# tier: {}", tier(resolved.config.source)),
        format!(
            "# source: {}",
            resolved
                .path
                .as_ref()
                .map_or_else(|| "none".to_string(), |path| path.display().to_string())
        ),
        format!("# fingerprint: {}", resolved.config.fingerprint()),
    ];
    if let Some(ignored) = &resolved.ignored_project_config {
        out.push(format!(
            "# ignored: {} was found and not loaded; pass --trust-project-config \
             (or set MCPLS_TRUST_PROJECT_CONFIG=true) to load it",
            ignored.display()
        ));
    }
    out.push(String::new());
    out
}

/// Flattened `path = value  # from <file>` (or `# (default)`) lines, sorted
/// by path, so a reader can see which settings their file actually decided
/// and which the built-ins did.
fn origin_lines(resolved: &Resolved) -> Result<Vec<String>> {
    let merged =
        toml::Value::try_from(&resolved.config).context("rendering the configuration as TOML")?;
    let mut leaves = Vec::new();
    flatten(&merged, "", &mut leaves);
    leaves.sort_by(|a, b| a.0.cmp(&b.0));

    let from_file = match &resolved.path {
        Some(path) => file_leaves(path)?,
        None => BTreeSet::new(),
    };

    Ok(leaves
        .iter()
        .map(
            |(path, value)| match (&resolved.path, from_file.contains(path)) {
                (Some(file), true) => format!("{path} = {value}  # from {}", file.display()),
                _ => format!("{path} = {value}  # (default)"),
            },
        )
        .collect())
}

/// The language servers the loaded configuration file names itself.
///
/// A built-in that happens to match a marker here is not something anyone
/// asked for, so its missing binary is a fact rather than a fault. An
/// entry the user wrote is the opposite: they asked for that server, and
/// it is not installed. Empty when nothing was loaded, or when the file
/// has since become unreadable, so a doubtful case claims no fault.
pub fn servers_the_file_names(resolved: &Resolved) -> BTreeSet<String> {
    let Some(path) = &resolved.path else {
        return BTreeSet::new();
    };
    file_leaves(path)
        .unwrap_or_default()
        .iter()
        .filter_map(|leaf| leaf.strip_prefix("lsp_servers.")?.split('.').next())
        .map(str::to_string)
        .collect()
}

/// The leaf paths a configuration file sets, as `flatten` names them.
fn file_leaves(path: &Path) -> Result<BTreeSet<String>> {
    let body =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    let mut leaves = Vec::new();
    flatten(&value, "", &mut leaves);
    Ok(leaves.into_iter().map(|(path, _)| path).collect())
}

/// Dotted leaf paths for a configuration document.
///
/// `lsp_servers` entries are keyed by the identity each one names rather
/// than by position: a file's single entry folds onto the built-ins, so the
/// index it sat at in the file is never the index it ends up at.
fn flatten(value: &toml::Value, prefix: &str, out: &mut Vec<(String, toml::Value)>) {
    match value {
        toml::Value::Table(table) => {
            for (key, sub) in table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(sub, &path, out);
            }
        }
        toml::Value::Array(entries) if prefix == "lsp_servers" => {
            for entry in entries {
                if let Some(id) = server_key(entry) {
                    flatten(entry, &format!("lsp_servers.{id}"), out);
                }
            }
        }
        _ => out.push((prefix.to_string(), value.clone())),
    }
}

/// The routing identity one `[[lsp_servers]]` entry names: its `name`, or
/// the `language_id` it falls back to.
fn server_key(entry: &toml::Value) -> Option<&str> {
    let table = entry.as_table()?;
    table
        .get("name")
        .or_else(|| table.get("language_id"))?
        .as_str()
}

/// The configuration with its provenance beside it, for `--origin --json`.
fn origin_json(
    resolved: &Resolved,
    directory: &Path,
    examined: Examined,
) -> Result<serde_json::Value> {
    let origins: Vec<String> = origin_lines(resolved)?;
    Ok(serde_json::json!({
        "config": resolved.config,
        "directory": directory,
        "directory_from": examined.origin(),
        "tier": tier(resolved.config.source),
        "source": resolved.path,
        "fingerprint": resolved.config.fingerprint(),
        "ignored_project_config": resolved.ignored_project_config,
        "origins": origins,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::PathBuf;

    use mcpls_core::ServerConfig;

    use super::*;

    fn resolved(source: ConfigSource, path: Option<&Path>, ignored: Option<&str>) -> Resolved {
        let config = path.map_or_else(ServerConfig::default, |path| {
            ServerConfig::load_from(path).unwrap()
        });
        Resolved {
            config: ServerConfig { source, ..config },
            path: path.map(Path::to_path_buf),
            ignored_project_config: ignored.map(PathBuf::from),
        }
    }

    fn write(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("mcpls.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn test_the_header_names_the_tier_the_file_and_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "[backend]\nidle_shutdown_ms = 5000\n");
        let resolved = resolved(ConfigSource::Project, Some(&path), None);

        let out = render(
            &resolved,
            Path::new("/work"),
            Examined::Argument,
            false,
            false,
        )
        .unwrap();

        assert!(out.contains("# tier: project"), "{out}");
        assert!(
            out.contains(&format!("# source: {}", path.display())),
            "{out}"
        );
        assert!(
            out.contains(&format!("# fingerprint: {}", resolved.config.fingerprint())),
            "{out}"
        );
        assert!(
            out.contains("# directory: /work (given on the command line)"),
            "{out}"
        );
    }

    /// The body is the file a user would write, so it parses back into the
    /// same settings rather than only looking like TOML.
    #[test]
    fn test_the_body_round_trips_into_the_same_configuration() {
        let resolved = resolved(ConfigSource::Defaults, None, None);
        let out = render(
            &resolved,
            Path::new("/work"),
            Examined::WorkingDirectory,
            false,
            false,
        )
        .unwrap();

        let body: String = out
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed: ServerConfig = toml::from_str(&body).unwrap();

        assert_eq!(parsed.fingerprint(), resolved.config.fingerprint());
    }

    #[test]
    fn test_an_ignored_project_config_is_named() {
        let resolved = resolved(ConfigSource::Defaults, None, Some("/work/mcpls.toml"));
        let out = render(
            &resolved,
            Path::new("/work"),
            Examined::Argument,
            false,
            false,
        )
        .unwrap();

        assert!(
            out.contains("# ignored: /work/mcpls.toml was found and not loaded"),
            "{out}"
        );
        assert!(out.contains("--trust-project-config"), "{out}");
    }

    /// A script deserializes `--json` straight back into a configuration,
    /// so nothing may wrap it.
    #[test]
    fn test_plain_json_is_the_configuration_itself() {
        let resolved = resolved(ConfigSource::Defaults, None, None);
        let out = render(
            &resolved,
            Path::new("/work"),
            Examined::ProjectDir,
            false,
            true,
        )
        .unwrap();

        let parsed: ServerConfig = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed.fingerprint(), resolved.config.fingerprint());
    }

    /// The whole point of `--origin`: a setting the file decided is told
    /// apart from one the built-ins did.
    #[test]
    fn test_origin_names_the_file_for_what_it_set_and_defaults_for_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "[backend]\nidle_shutdown_ms = 5000\n");
        let resolved = resolved(ConfigSource::Project, Some(&path), None);

        let out = render(
            &resolved,
            Path::new("/work"),
            Examined::Argument,
            true,
            false,
        )
        .unwrap();

        let from_file = out
            .lines()
            .find(|line| line.starts_with("backend.idle_shutdown_ms ="))
            .unwrap();
        assert!(from_file.contains("= 5000"), "{from_file}");
        assert!(
            from_file.contains(&format!("# from {}", path.display())),
            "{from_file}"
        );

        let untouched = out
            .lines()
            .find(|line| line.starts_with("backend.spawn ="))
            .unwrap();
        assert!(untouched.contains("# (default)"), "{untouched}");
    }

    /// A `[[lsp_servers]]` entry folds onto a built-in, landing at an index
    /// that has nothing to do with the one it sat at in the file, so the
    /// origin lines key it by its identity instead.
    #[test]
    fn test_origin_attributes_a_server_override_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "[[lsp_servers]]\nlanguage_id = \"rust\"\ncommand = \"my-analyzer\"\n",
        );
        let resolved = resolved(ConfigSource::Project, Some(&path), None);

        let out = render(
            &resolved,
            Path::new("/work"),
            Examined::Argument,
            true,
            false,
        )
        .unwrap();

        let command = out
            .lines()
            .find(|line| line.starts_with("lsp_servers.rust.command ="))
            .unwrap();
        assert!(command.contains("my-analyzer"), "{command}");
        assert!(
            command.contains(&format!("# from {}", path.display())),
            "{command}"
        );

        let inherited = out
            .lines()
            .find(|line| line.starts_with("lsp_servers.python.command ="))
            .unwrap();
        assert!(inherited.contains("# (default)"), "{inherited}");
    }

    #[test]
    fn test_origin_json_carries_the_config_and_its_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "[backend]\nidle_shutdown_ms = 5000\n");
        let resolved = resolved(ConfigSource::Project, Some(&path), None);

        let out = render(
            &resolved,
            Path::new("/work"),
            Examined::ProjectDir,
            true,
            true,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["fingerprint"], resolved.config.fingerprint());
        assert_eq!(parsed["directory_from"], "from CLAUDE_PROJECT_DIR");
        assert!(parsed["config"]["lsp_servers"].is_array(), "{out}");
        assert!(
            parsed["origins"].as_array().unwrap().iter().any(|line| line
                .as_str()
                .unwrap()
                .starts_with("backend.idle_shutdown_ms =")),
            "{out}"
        );
    }
}
