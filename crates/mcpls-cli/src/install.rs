//! `mcpls lsp install`: run the configured install command for each
//! language server whose binary is missing.

use std::fmt::{self, Write as _};
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use mcpls_core::ServerConfig;
use mcpls_core::bridge::LspAction;
use mcpls_core::config::InstallCommand;
use mcpls_core::hooks::{Request, probe};

/// As long as `mcpls lsp start` waits for the backend to answer.
const NUDGE_TIMEOUT: Duration = Duration::from_secs(3);

/// One language server `mcpls lsp install` considers.
#[derive(Debug)]
pub struct Target {
    /// The routing id `mcpls lsp status` prints.
    pub id: String,
    /// The server's `command`; it resolving means the server is installed.
    pub program: String,
    /// The install command for this OS.
    pub install: Option<String>,
    /// Named on the command line rather than reached through `--all`.
    pub named: bool,
}

/// What happened to one [`Target`].
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Installed,
    AlreadyInstalled,
    NoCommand,
    WouldRun,
    /// The command exited unsuccessfully, with its exit code when it has
    /// one.
    Failed(Option<i32>),
    /// The shell could not be started.
    CouldNotStart(String),
    /// The command succeeded but this program still does not resolve.
    StillMissing(String),
}

impl Status {
    /// Whether this outcome fails the run. Lacking a command is a failure
    /// only for a server someone asked for by name.
    pub const fn fails(&self, named: bool) -> bool {
        match self {
            Self::Installed | Self::AlreadyInstalled | Self::WouldRun => false,
            Self::NoCommand => named,
            Self::Failed(_) | Self::CouldNotStart(_) | Self::StillMissing(_) => true,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Installed => f.write_str("installed"),
            Self::AlreadyInstalled => f.write_str("already installed"),
            Self::NoCommand => f.write_str("no install command"),
            Self::WouldRun => f.write_str("would run"),
            Self::Failed(Some(code)) => write!(f, "failed (exit {code})"),
            Self::Failed(None) => f.write_str("failed (no exit code)"),
            Self::CouldNotStart(error) => write!(f, "could not start: {error}"),
            Self::StillMissing(program) => write!(
                f,
                "installed, but {program} is still not on PATH; open a new shell"
            ),
        }
    }
}

/// The servers to consider, in configuration order: those in `named`, or
/// every applicable server when `named` is empty.
///
/// # Errors
///
/// Names every id in `named` that no applicable server carries, and lists
/// the ids that do apply.
pub fn plan(config: &ServerConfig, root: &Path, named: &[String]) -> Result<Vec<Target>, String> {
    let max_depth = Some(config.workspace.heuristics_max_depth);
    let mut applicable: Vec<Target> = Vec::new();
    for server in &config.lsp_servers {
        let id = server.id().to_string();
        if applicable.iter().any(|target| target.id == id) || !server.should_spawn(root, max_depth)
        {
            continue;
        }
        applicable.push(Target {
            id,
            program: server.command.clone(),
            install: server
                .install
                .as_ref()
                .and_then(InstallCommand::for_this_os)
                .map(str::to_string),
            named: !named.is_empty(),
        });
    }
    if named.is_empty() {
        return Ok(applicable);
    }

    let unknown: Vec<&str> = named
        .iter()
        .filter(|name| !applicable.iter().any(|target| &target.id == *name))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let known: Vec<&str> = applicable.iter().map(|target| target.id.as_str()).collect();
        let known = if known.is_empty() {
            "none do".to_string()
        } else {
            format!("these do: {}", known.join(", "))
        };
        return Err(format!(
            "no language server named {} applies here; {known}",
            unknown.join(", ")
        ));
    }
    applicable.retain(|target| named.contains(&target.id));
    Ok(applicable)
}

/// Install each target in turn, in `root`, skipping those whose program
/// `installed` already finds. With `dry_run`, name each command and run
/// nothing.
pub async fn install(
    targets: &[Target],
    root: &Path,
    dry_run: bool,
    installed: impl Fn(&str) -> bool + Sync,
) -> Vec<(String, Status)> {
    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        let status = if installed(&target.program) {
            Status::AlreadyInstalled
        } else if let Some(command) = &target.install {
            announce(&target.id, command);
            if dry_run {
                Status::WouldRun
            } else {
                run(command, root, &target.program, &installed).await
            }
        } else {
            Status::NoCommand
        };
        results.push((target.id.clone(), status));
    }
    results
}

/// Ask the backend serving `root`, if one answers, to start the servers
/// just installed. Without a backend there is nothing to start, and the
/// answer changes nothing the report says, so neither is printed.
pub async fn nudge(root: &Path, installed: Vec<String>) {
    if installed.is_empty() {
        return;
    }
    let Ok(identity) = mcpls_core::hooks::identity_for(root) else {
        return;
    };
    let request = Request::Lsp {
        action: LspAction::Start,
        servers: installed,
    };
    let _ = probe(&identity, &request, NUDGE_TIMEOUT).await;
}

/// One line per target, id then outcome.
pub fn render(results: &[(String, Status)]) -> String {
    if results.is_empty() {
        return "no language servers apply here\n".to_string();
    }
    results
        .iter()
        .fold(String::new(), |mut text, (id, status)| {
            let _ = writeln!(text, "{id}  {status}");
            text
        })
}

/// Print the command about to run before its own output reaches the
/// terminal.
fn announce(id: &str, command: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "==> {id}: {command}").and_then(|()| out.flush());
}

async fn run(
    command: &str,
    root: &Path,
    program: &str,
    installed: impl Fn(&str) -> bool + Sync,
) -> Status {
    match shell(command).current_dir(root).status().await {
        Err(error) => Status::CouldNotStart(error.to_string()),
        Ok(exit) if !exit.success() => Status::Failed(exit.code()),
        Ok(_) if installed(program) => Status::Installed,
        Ok(_) => Status::StillMissing(program.to_string()),
    }
}

#[cfg(not(windows))]
fn shell(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("sh");
    shell.arg("-c").arg(command);
    shell
}

/// Windows `PowerShell`, handed the command encoded: Windows argument
/// quoting mangles double quotes on their way into `-Command`. The
/// execution policy is bypassed because a package manager's own shim is
/// often a `.ps1` script, which the default policy refuses to run.
#[cfg(windows)]
fn shell(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("powershell");
    shell
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
        ])
        .arg(encode_powershell(command));
    shell
}

/// The base64 of `command`'s UTF-16LE bytes, as `-EncodedCommand` reads it.
#[cfg(windows)]
fn encode_powershell(command: &str) -> String {
    use base64::Engine as _;
    let bytes: Vec<u8> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use mcpls_core::ServerConfig;
    use tempfile::TempDir;

    use super::*;

    /// Servers `a` and `b`, in that order, both applicable in the returned
    /// directory, plus `absent`, whose marker is missing there.
    fn two_servers() -> (ServerConfig, TempDir) {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("marker.here"), "").unwrap();
        let server = |id: &str, marker: &str| {
            format!(
                "[[lsp_servers]]\nlanguage_id = \"{id}\"\ncommand = \"{id}-ls\"\n\
                 [lsp_servers.heuristics]\nproject_markers = [\"{marker}\"]\n"
            )
        };
        let toml = [
            server("a", "marker.here"),
            server("b", "marker.here"),
            server("absent", "marker.elsewhere"),
        ]
        .concat();
        (toml::from_str(&toml).unwrap(), dir)
    }

    fn ids(targets: &[Target]) -> Vec<&str> {
        targets.iter().map(|target| target.id.as_str()).collect()
    }

    #[test]
    fn test_plan_keeps_configuration_order_and_drops_repeated_names() {
        let (config, dir) = two_servers();
        let named = ["b", "a", "b"].map(String::from);
        let targets = plan(&config, dir.path(), &named).unwrap();
        assert_eq!(ids(&targets), ["a", "b"]);
        assert!(targets.iter().all(|target| target.named));
    }

    #[test]
    fn test_plan_all_skips_servers_that_do_not_apply() {
        let (config, dir) = two_servers();
        let targets = plan(&config, dir.path(), &[]).unwrap();
        assert_eq!(ids(&targets), ["a", "b"]);
        assert!(targets.iter().all(|target| !target.named));
    }

    #[test]
    fn test_plan_names_the_applicable_servers_for_an_unknown_one() {
        let (config, dir) = two_servers();
        let error = plan(&config, dir.path(), &["nope".to_string()]).unwrap_err();
        assert_eq!(
            error,
            "no language server named nope applies here; these do: a, b"
        );
    }

    #[test]
    fn test_failed_without_an_exit_code_still_fails() {
        assert_eq!(Status::Failed(None).to_string(), "failed (no exit code)");
        assert_eq!(Status::Failed(Some(3)).to_string(), "failed (exit 3)");
        assert!(Status::Failed(None).fails(false));
    }

    #[test]
    fn test_only_a_named_server_fails_for_lacking_a_command() {
        assert!(Status::NoCommand.fails(true));
        assert!(!Status::NoCommand.fails(false));
        for named in [true, false] {
            assert!(!Status::Installed.fails(named));
            assert!(!Status::AlreadyInstalled.fails(named));
            assert!(!Status::WouldRun.fails(named));
            assert!(Status::StillMissing("x".into()).fails(named));
            assert!(Status::CouldNotStart("x".into()).fails(named));
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_powershell_commands_are_utf16_base64() {
        assert_eq!(encode_powershell("a"), "YQA=");
    }
}
