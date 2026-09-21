//! Command-line argument parsing.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::completions::Shell;
use crate::hook::{Harness, HookEvent};

/// Parses a boolean flag or env value, accepting `1`/`0`, `yes`/`no`,
/// `y`/`n` and `on`/`off` besides `true`/`false`, case-insensitively.
///
/// Padded or empty input is rejected, so `MCPLS_LOG_JSON=` with no value
/// fails startup instead of reading as unset.
pub fn parse_bool_flag(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "no" | "n" | "off" => Ok(false),
        other => Err(format!(
            "invalid boolean value '{other}' (expected one of: 1, 0, true, false, yes, no, y, n, on, off)"
        )),
    }
}

/// Universal MCP to LSP Bridge
///
/// Exposes Language Server Protocol capabilities as MCP tools,
/// enabling AI agents to access semantic code intelligence.
#[derive(Debug, Parser)]
#[command(name = "mcpls")]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Args {
    /// Subcommand to run instead of the MCP server
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Path to configuration file
    ///
    /// If not specified, searches for mcpls.toml in:
    /// 1. `$MCPLS_CONFIG` environment variable
    /// 2. The checkout root (only loaded with `--trust-project-config`)
    /// 3. Platform config file: `$XDG_CONFIG_HOME/mcpls/mcpls.toml`, else
    ///    `~/.config/mcpls/mcpls.toml` (Linux); `~/Library/Application
    ///    Support/mcpls/mcpls.toml` (macOS); `%APPDATA%\mcpls\mcpls.toml` (Windows)
    #[arg(short, long, value_name = "FILE", env = "MCPLS_CONFIG")]
    pub config: Option<PathBuf>,

    /// Trust and load a `mcpls.toml` found at the checkout root.
    ///
    /// A project-local config discovered this way (as opposed to one passed
    /// explicitly via `--config`/`MCPLS_CONFIG`) can control the LSP server
    /// `command`/`args` mcpls spawns, so it is ignored by default to avoid
    /// arbitrary code execution when running mcpls against an untrusted
    /// checkout. Pass this flag only for repositories you trust. Via
    /// `MCPLS_TRUST_PROJECT_CONFIG`, accepted values are `1`/`0`, `true`/
    /// `false`, `yes`/`no`, `y`/`n`, and `on`/`off` (case-insensitive); any
    /// other value is a parse error at startup.
    #[arg(long, env = "MCPLS_TRUST_PROJECT_CONFIG", value_parser = parse_bool_flag)]
    pub trust_project_config: bool,

    /// Serve this one session in-process instead of through the project's
    /// shared backend.
    ///
    /// For debugging, and for hosts where a detached backend cannot run.
    /// This process binds the project's endpoint for hooks if it is free.
    #[arg(long, env = "MCPLS_NO_BACKEND", value_parser = parse_bool_flag)]
    pub no_backend: bool,

    /// Logging level
    ///
    /// Valid values: trace, debug, info, warn, error
    #[arg(short, long, default_value = "info", env = "MCPLS_LOG")]
    pub log_level: String,

    /// Output logs as JSON (for structured logging)
    ///
    /// Via `MCPLS_LOG_JSON`, accepted values are `1`/`0`, `true`/`false`,
    /// `yes`/`no`, `y`/`n`, and `on`/`off` (case-insensitive).
    #[arg(long, default_value = "false", env = "MCPLS_LOG_JSON", value_parser = parse_bool_flag)]
    pub log_json: bool,

    /// Listen address for HTTP transport (e.g. 127.0.0.1:3000).
    ///
    /// When set, the MCP server binds this address and serves over Streamable
    /// HTTP instead of stdio. Requires the `transport-http` feature.
    #[cfg(feature = "transport-http")]
    #[arg(long, value_name = "ADDR", env = "MCPLS_LISTEN")]
    pub listen: Option<std::net::SocketAddr>,

    /// URL path the MCP service is mounted at (default `/mcp`).
    ///
    /// Only meaningful when `--listen` is set.
    #[cfg(feature = "transport-http")]
    #[arg(
        long,
        value_name = "PATH",
        default_value = "/mcp",
        env = "MCPLS_HTTP_PATH"
    )]
    pub http_path: String,
}

/// Subcommands that replace the default action.
///
/// `mcpls` with no subcommand runs the MCP server, which is what an MCP host
/// launches; everything here is a one-shot utility that prints and exits.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start, stop, or inspect this checkout's shared backend
    ///
    /// Every session in a checkout attaches to one backend, which owns the
    /// language servers. A session starts it on demand, and it exits once
    /// no session has been attached for `backend.idle_shutdown_ms`.
    #[command(
        args_conflicts_with_subcommands = true,
        arg_required_else_help = true,
        override_usage = "mcpls backend <COMMAND>"
    )]
    Backend {
        /// Serve the backend for this canonical checkout root, as a
        /// frontend launches it
        #[arg(long, value_name = "DIR", hide = true)]
        root: Option<PathBuf>,

        /// What to do with the backend
        #[command(subcommand)]
        action: Option<BackendAction>,
    },

    /// Print the note a session start hook hands an agent
    ///
    /// Names the installed language servers that serve this checkout and
    /// points the agent at the mcpls tools. Prints nothing when `[brief]`
    /// is switched off, or when no installed server applies here, and
    /// always exits 0.
    ///
    /// Examines `$CLAUDE_PROJECT_DIR`, then the working directory.
    Brief {
        /// Wrap the brief in the JSON a Codex `SessionStart` hook returns
        #[arg(long)]
        additional_context: bool,
    },

    /// Print a shell completion script to stdout
    ///
    /// Redirect it to wherever the shell reads completions from, for example:
    ///
    ///   mcpls completions bash > ~/.local/share/bash-completion/completions/mcpls
    ///
    ///   mcpls completions fish > ~/.config/fish/completions/mcpls.fish
    ///
    /// A nushell script defines a module, so save it to a file and source that
    /// file from config.nu rather than piping it in. The installation guide
    /// lists the path each shell expects.
    Completions {
        /// Shell dialect to emit
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Report on the backend, the language servers, and the install
    ///
    /// Names the socket, the directory both sides resolved, whether a
    /// backend answers, which configured language servers apply here and
    /// which of their binaries are installed, and whether `mcpls` resolves
    /// on `PATH`. Exits non-zero when it finds a fault.
    Doctor {
        /// Directory to examine
        ///
        /// Defaults to `$CLAUDE_PROJECT_DIR`, then the working directory.
        #[arg(value_name = "DIR")]
        path: Option<PathBuf>,
    },

    /// Print the configuration resolved for a directory
    ///
    /// Names the tier that won, the file it came from, the fingerprint, and
    /// the merged settings including the built-in servers folded in.
    /// Honors `--config` and `--trust-project-config`, which change the
    /// answer.
    Config {
        /// Directory to resolve the configuration for
        ///
        /// Defaults to `$CLAUDE_PROJECT_DIR`, then the working directory.
        #[arg(value_name = "DIR")]
        path: Option<PathBuf>,

        /// Annotate each setting with the file it came from
        #[arg(long)]
        origin: bool,

        /// Print JSON instead of TOML
        #[arg(long)]
        json: bool,
    },

    /// Print the JSON Schema for `mcpls.toml` or initialize a config file
    Schema {
        /// An action to perform instead of printing the schema
        #[command(subcommand)]
        action: Option<SchemaAction>,
    },

    /// Serve one agent hook invocation
    ///
    /// Reads the hook payload for EVENT from stdin and writes hook JSON to
    /// stdout. With no EVENT, the payload's own `hook_event_name` names it,
    /// which is how a manifest from before the verbs reads.
    #[command(subcommand_value_name = "EVENT", subcommand_help_heading = "Events")]
    Hook {
        /// The harness that spawned this hook, which decides where the
        /// project directory comes from
        #[arg(
            long,
            alias = "host",
            value_enum,
            default_value_t = Harness::ClaudeCode,
            global = true
        )]
        harness: Harness,

        /// The event to answer
        #[command(subcommand)]
        action: Option<HookAction>,
    },
}

/// Actions for `mcpls backend`.
///
/// Each examines `DIR`, else `$CLAUDE_PROJECT_DIR`, else the working
/// directory, and acts on the backend for that directory's checkout.
#[derive(Debug, Subcommand)]
pub enum BackendAction {
    /// Start the backend and keep it running
    ///
    /// A started backend stays up with no session attached, instead of
    /// exiting on its idle timer, until `mcpls backend stop`. A backend
    /// already running is kept rather than replaced. A new backend runs
    /// with this invocation's `--config`, `--trust-project-config`,
    /// `--log-level` and `--log-json`.
    Start {
        /// Directory whose checkout to serve
        #[arg(value_name = "DIR")]
        path: Option<PathBuf>,
    },

    /// Stop the backend
    ///
    /// With sessions attached, the backend is left to them: it is no
    /// longer kept, exits once the last one leaves, and this exits
    /// non-zero. `--force` stops it at once, and those sessions have no
    /// mcpls tools until they restart.
    Stop {
        /// Directory whose checkout's backend to stop
        #[arg(value_name = "DIR")]
        path: Option<PathBuf>,

        /// Stop the backend even with sessions attached
        #[arg(long)]
        force: bool,
    },

    /// Report whether the backend is running
    ///
    /// Exits 0 when a backend answers and 1 when none does.
    Status {
        /// Directory whose checkout's backend to report on
        #[arg(value_name = "DIR")]
        path: Option<PathBuf>,
    },
}

/// Actions for `mcpls schema`.
#[derive(Debug, Subcommand)]
pub enum SchemaAction {
    /// Create a default `mcpls.toml` in the current directory
    Init,
}

/// Actions for `mcpls hook`.
#[derive(Debug, Subcommand)]
pub enum HookAction {
    #[command(flatten)]
    Event(HookEvent),

    /// Alias for `mcpls doctor`, kept because published troubleshooting
    /// text spells it this way. It always exits 0, which the hook path
    /// requires; the top-level command reports a fault in its status.
    #[command(hide = true)]
    Doctor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool_flag_accepts_truthy_spellings() {
        for value in ["1", "true", "TRUE", "yes", "YES", "y", "Y", "on", "On"] {
            assert_eq!(
                parse_bool_flag(value),
                Ok(true),
                "expected {value:?} to parse as true"
            );
        }
    }

    #[test]
    fn test_parse_bool_flag_accepts_falsy_spellings() {
        for value in ["0", "false", "FALSE", "no", "NO", "n", "N", "off", "Off"] {
            assert_eq!(
                parse_bool_flag(value),
                Ok(false),
                "expected {value:?} to parse as false"
            );
        }
    }

    #[test]
    fn test_parse_bool_flag_rejects_invalid_values() {
        for value in ["banana", "2", "", "truee", "yesno"] {
            assert!(
                parse_bool_flag(value).is_err(),
                "expected {value:?} to be rejected"
            );
        }
    }

    #[test]
    fn test_default_args() {
        let args = Args::parse_from(["mcpls"]);
        assert!(args.command.is_none(), "no subcommand runs the MCP server");
        assert!(args.config.is_none());
        assert_eq!(args.log_level, "info");
        assert!(!args.log_json);
    }

    #[test]
    fn test_completions_subcommand_parses_shell() {
        let args = Args::parse_from(["mcpls", "completions", "nushell"]);
        assert!(matches!(
            args.command,
            Some(Command::Completions {
                shell: Shell::Nushell
            })
        ));
    }

    /// A shell mcpls cannot generate for has to be a parse error, not an
    /// empty script the user sources without noticing.
    #[test]
    fn test_completions_rejects_unknown_shell() {
        assert!(Args::try_parse_from(["mcpls", "completions", "tcsh"]).is_err());
    }

    /// The doctor moved out from under `hook`, where a user had to know
    /// it was filed under the thing they were not debugging.
    #[test]
    fn test_doctor_is_a_top_level_command_taking_an_optional_directory() {
        assert!(matches!(
            Args::parse_from(["mcpls", "doctor"]).command,
            Some(Command::Doctor { path: None })
        ));
        assert!(matches!(
            Args::parse_from(["mcpls", "doctor", "/work"]).command,
            Some(Command::Doctor { path: Some(path) }) if path == std::path::Path::new("/work")
        ));
    }

    /// Published troubleshooting text spells it `mcpls hook doctor`, so
    /// the old form keeps parsing.
    #[test]
    fn test_hook_doctor_still_parses() {
        assert!(matches!(
            Args::parse_from(["mcpls", "hook", "doctor"]).command,
            Some(Command::Hook {
                action: Some(HookAction::Doctor),
                ..
            })
        ));
    }

    #[test]
    fn test_config_takes_an_optional_directory_and_its_two_flags() {
        assert!(matches!(
            Args::parse_from(["mcpls", "config"]).command,
            Some(Command::Config {
                path: None,
                origin: false,
                json: false
            })
        ));
        assert!(matches!(
            Args::parse_from(["mcpls", "config", "/work", "--origin", "--json"]).command,
            Some(Command::Config {
                path: Some(path),
                origin: true,
                json: true
            }) if path == std::path::Path::new("/work")
        ));
    }

    #[test]
    fn test_trust_project_config_default_false() {
        let args = Args::parse_from(["mcpls"]);
        assert!(!args.trust_project_config);
    }

    #[test]
    fn test_trust_project_config_flag() {
        let args = Args::parse_from(["mcpls", "--trust-project-config"]);
        assert!(args.trust_project_config);
    }

    #[test]
    fn test_no_backend_flag() {
        assert!(!Args::parse_from(["mcpls"]).no_backend);
        assert!(Args::parse_from(["mcpls", "--no-backend"]).no_backend);
    }

    #[test]
    fn test_backend_subcommand_takes_a_root() {
        let args = Args::parse_from([
            "mcpls",
            "--log-level",
            "debug",
            "backend",
            "--root",
            "/work",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::Backend { root: Some(root), action: None }) if root == std::path::Path::new("/work")
        ));
        assert_eq!(args.log_level, "debug");
    }

    #[test]
    fn test_backend_actions_take_an_optional_directory() {
        assert!(matches!(
            Args::parse_from(["mcpls", "backend", "stop", "--force", "/work"]).command,
            Some(Command::Backend {
                root: None,
                action: Some(BackendAction::Stop { path: Some(path), force: true }),
            }) if path == std::path::Path::new("/work")
        ));
        assert!(matches!(
            Args::parse_from(["mcpls", "backend", "start"]).command,
            Some(Command::Backend {
                action: Some(BackendAction::Start { path: None }),
                ..
            })
        ));
        assert!(Args::try_parse_from(["mcpls", "backend", "--root", "/work", "status"]).is_err());
    }

    /// The arguments a frontend launches a backend with parse back into the
    /// settings it launched it with.
    #[test]
    fn test_launch_arguments_round_trip_through_the_parser() {
        let launch = mcpls_core::backend::BackendLaunch {
            root: std::path::PathBuf::from("/work"),
            config: Some(std::path::PathBuf::from("/etc/mcpls.toml")),
            trust_project_config: true,
            log_level: "trace".to_string(),
            log_json: true,
        };
        let mut argv = vec![std::ffi::OsString::from("mcpls")];
        argv.extend(launch.args());
        let parsed = Args::parse_from(argv);
        assert_eq!(
            parsed.config.as_deref(),
            Some(std::path::Path::new("/etc/mcpls.toml"))
        );
        assert!(parsed.trust_project_config);
        assert_eq!(parsed.log_level, "trace");
        assert!(parsed.log_json);
        assert!(matches!(parsed.command, Some(Command::Backend { .. })));
    }

    #[test]
    fn test_config_arg() {
        let args = Args::parse_from(["mcpls", "--config", "/path/to/config.toml"]);
        assert_eq!(args.config, Some(PathBuf::from("/path/to/config.toml")));
    }

    #[test]
    fn test_config_short_flag() {
        let args = Args::parse_from(["mcpls", "-c", "/path/to/config.toml"]);
        assert_eq!(
            args.config,
            Some(PathBuf::from("/path/to/config.toml")),
            "Short flag -c should work for config"
        );
    }

    #[test]
    fn test_log_level_arg() {
        let args = Args::parse_from(["mcpls", "--log-level", "debug"]);
        assert_eq!(args.log_level, "debug");
    }

    #[test]
    fn test_log_level_short_flag() {
        let args = Args::parse_from(["mcpls", "-l", "trace"]);
        assert_eq!(
            args.log_level, "trace",
            "Short flag -l should work for log-level"
        );
    }

    #[test]
    fn test_log_level_all_valid_values() {
        let valid_levels = ["trace", "debug", "info", "warn", "error"];

        for level in &valid_levels {
            let args = Args::parse_from(["mcpls", "--log-level", level]);
            assert_eq!(
                args.log_level, *level,
                "Log level {level} should be accepted"
            );
        }
    }

    #[test]
    fn test_log_json_flag() {
        let args = Args::parse_from(["mcpls", "--log-json"]);
        assert!(args.log_json, "Flag --log-json should enable JSON logging");
        assert_eq!(
            args.log_level, "info",
            "Default log level should still be info"
        );
    }

    #[test]
    fn test_log_json_default_false() {
        let args = Args::parse_from(["mcpls"]);
        assert!(!args.log_json, "JSON logging should be disabled by default");
    }

    #[test]
    fn test_all_args_combined() {
        let args = Args::parse_from([
            "mcpls",
            "--config",
            "/custom/config.toml",
            "--log-level",
            "debug",
            "--log-json",
        ]);

        assert_eq!(args.config, Some(PathBuf::from("/custom/config.toml")));
        assert_eq!(args.log_level, "debug");
        assert!(args.log_json);
    }

    #[test]
    fn test_config_with_relative_path() {
        let args = Args::parse_from(["mcpls", "--config", "./mcpls.toml"]);
        assert_eq!(args.config, Some(PathBuf::from("./mcpls.toml")));
    }

    #[test]
    fn test_config_with_home_path() {
        let args = Args::parse_from(["mcpls", "--config", "~/.config/mcpls/mcpls.toml"]);
        assert_eq!(
            args.config,
            Some(PathBuf::from("~/.config/mcpls/mcpls.toml"))
        );
    }

    #[test]
    fn test_log_level_case_sensitive() {
        let args = Args::parse_from(["mcpls", "--log-level", "DEBUG"]);
        assert_eq!(
            args.log_level, "DEBUG",
            "Log level should preserve case (validation happens later)"
        );
    }

    #[test]
    fn test_args_with_mixed_short_long_flags() {
        let args = Args::parse_from([
            "mcpls",
            "-c",
            "/path/to/config.toml",
            "-l",
            "warn",
            "--log-json",
        ]);

        assert_eq!(args.config, Some(PathBuf::from("/path/to/config.toml")));
        assert_eq!(args.log_level, "warn");
        assert!(args.log_json);
    }

    #[cfg(feature = "transport-http")]
    #[allow(clippy::unwrap_used)]
    mod http_transport_tests {
        use std::net::SocketAddr;

        use super::*;

        #[test]
        fn test_listen_flag_parses_addr() {
            let args = Args::parse_from(["mcpls", "--listen", "127.0.0.1:3000"]);
            let expected: SocketAddr = "127.0.0.1:3000".parse().unwrap();
            assert_eq!(args.listen, Some(expected));
        }

        #[test]
        fn test_listen_default_is_none() {
            let args = Args::parse_from(["mcpls"]);
            assert!(args.listen.is_none());
        }

        #[test]
        fn test_http_path_default() {
            let args = Args::parse_from(["mcpls"]);
            assert_eq!(args.http_path, "/mcp");
        }

        #[test]
        fn test_http_path_custom() {
            let args = Args::parse_from(["mcpls", "--http-path", "/api/mcp"]);
            assert_eq!(args.http_path, "/api/mcp");
        }

        #[test]
        fn test_listen_ipv6() {
            let args = Args::parse_from(["mcpls", "--listen", "[::1]:4000"]);
            let expected: SocketAddr = "[::1]:4000".parse().unwrap();
            assert_eq!(args.listen, Some(expected));
        }
    }
}
