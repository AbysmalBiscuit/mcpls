//! MCPLS - Universal MCP to LSP Bridge
//!
//! This binary provides an MCP server that exposes LSP capabilities as tools,
//! enabling AI agents to access semantic code intelligence.

use anyhow::{Context, Result};
use clap::Parser;
use mcpls_core::ProjectConfigTrust;

mod args;
mod backend;
mod brief;
mod completions;
mod config;
mod hook;
mod install;
mod logging;
mod lsp;
mod status;

use args::{Args, BackendAction, Command, HookAction, LspCommand, LspTargets, SchemaAction};
use hook::{Examined, Report};
use mcpls_core::bridge::LspAction;

/// Parse, failing a `hook` invocation with exit 1 rather than clap's 2.
///
/// Exit 2 is a verdict to a harness: Claude Code erases a submitted prompt
/// on it and Codex blocks the tool call, so a verb from a manifest newer
/// than this binary must not read as one. `use_stderr` tells a usage error
/// from `--help`, which clap also reports as an error.
fn parse_args() -> Args {
    let hook = std::env::args_os().nth(1).is_some_and(|arg| arg == "hook");
    match Args::try_parse() {
        Ok(args) => args,
        Err(err) if hook && err.use_stderr() => {
            let _ = err.print();
            std::process::exit(1);
        }
        Err(err) => err.exit(),
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args();

    // Completions need neither a loaded config nor a log subscriber, and
    // printing the script is the whole command, so this runs before both.
    if let Some(Command::Completions { shell }) = &args.command {
        let mut out = std::io::stdout().lock();
        if let Err(err) = completions::emit(*shell, &mut out) {
            eprintln!("failed to write completion script: {err}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    if let Some(Command::Schema { action }) = &args.command {
        match action {
            Some(SchemaAction::Init) => {
                let directory = match std::env::current_dir() {
                    Ok(directory) => directory,
                    Err(err) => {
                        eprintln!("failed to read the current directory: {err}");
                        std::process::exit(1);
                    }
                };
                let path = directory.join("mcpls.toml");
                match mcpls_core::config::init_config_file(&path) {
                    Ok(()) => {
                        println!("Created {}", path.display());
                        println!(
                            "For project-local use, run init at the checkout root and opt in with `--trust-project-config` or `MCPLS_TRUST_PROJECT_CONFIG=true`."
                        );
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        eprintln!("{} already exists", path.display());
                        std::process::exit(1);
                    }
                    Err(err) => {
                        eprintln!("failed to create {}: {err}", path.display());
                        std::process::exit(1);
                    }
                }
            }
            None => emit_schema(),
        }
        std::process::exit(0);
    }

    if let Some(Command::Doctor { path }) = &args.command {
        let report = diagnose(&args, path.as_deref()).await;
        write_report(&format!("{}\n", report.text));
        std::process::exit(i32::from(!report.problems.is_empty()));
    }

    if let Some(Command::Backend {
        action: Some(action),
        ..
    }) = &args.command
    {
        let outcome = control_backend(&args, action).await;
        write_report(&outcome.text);
        std::process::exit(i32::from(!outcome.success));
    }

    if let Some(Command::Lsp { action }) = &args.command {
        let outcome = run_lsp(&args, action).await;
        write_report(&outcome.text);
        std::process::exit(i32::from(!outcome.success));
    }

    if let Some(Command::Config { path, origin, json }) = &args.command {
        let (directory, examined) = examined_directory(path.as_deref());
        let root = hook::checkout_root(&directory);
        let rendered = resolve_config(&args, &root)
            .and_then(|resolved| config::render(&resolved, &directory, examined, *origin, *json));
        match rendered {
            Ok(text) => write_report(&text),
            Err(err) => {
                eprintln!("{err:?}");
                std::process::exit(1);
            }
        }
        std::process::exit(0);
    }

    match &args.command {
        Some(Command::Brief { additional_context }) => emit_brief(&args, *additional_context),
        Some(Command::Status { all }) => emit_status(*all).await,
        _ => {}
    }

    // A hook invocation needs neither a loaded config nor a log subscriber,
    // and reading stdin and writing hook JSON is the whole command.
    if let Some(Command::Hook { harness, action }) = &args.command {
        match action {
            Some(HookAction::Event(event)) => serve_hook(*harness, Some(*event)).await,
            None => serve_hook(*harness, None).await,
            // The alias exits 0 whatever it found, which every hook
            // registration depends on.
            Some(HookAction::Doctor) => {
                write_report(&format!("{}\n", diagnose(&args, None).await.text));
            }
        }
        std::process::exit(0);
    }

    // Initialize logging. No subscriber is installed yet, so failures here
    // must go straight to stderr.
    if let Err(err) = logging::init(&args.log_level, args.log_json) {
        eprintln!("failed to initialize logging: {err:?}");
        std::process::exit(1);
    }

    // Route fatal errors through the tracing subscriber (rather than the
    // default `Result` `Termination` printer) so they honor --log-json too.
    let exit_code = if let Err(err) = run(args).await {
        tracing::error!(error = ?err, "mcpls exited with an error");
        1
    } else {
        0
    };

    // Dropping the runtime waits on the thread parked in an uncancellable
    // stdin `read()`, which never returns while the client holds stdin open.
    std::process::exit(exit_code);
}

/// Answer one hook invocation for `event`, or for the event the payload
/// names when the manifest named none.
async fn serve_hook(harness: hook::Harness, event: Option<hook::HookEvent>) {
    use std::io::{Read as _, Write as _};
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    let Some(event) = event.or_else(|| hook::HookEvent::named_in(&stdin)) else {
        return;
    };
    let raw_project_dir = hook::project_dir(harness, &stdin);
    // The host need not resolve a watch path against this process's working
    // directory, so it must be absolute; a failed canonicalize keeps the raw path.
    let project_dir = dunce::canonicalize(&raw_project_dir).unwrap_or(raw_project_dir);
    let root = hook::checkout_root(&project_dir);
    // A failed `identity_for` (an unreachable directory, or an over-long
    // socket path) leaves each arm to degrade on its own.
    let identity = mcpls_core::hooks::identity_for(&root).ok();
    #[cfg(windows)]
    if let (Some(identity), Ok(exe)) = (identity.as_ref(), std::env::current_exe()) {
        // The frontend cannot spawn a backend that outlives a job-contained
        // session, so it asks and a hook starts it.
        let _ = mcpls_core::backend::start_requested(identity, &exe).await;
    }
    let out = hook::dispatch_payload(harness, event, &stdin, &root, identity.as_ref()).await;
    // `print!` panics on a write failure (a closed stdout pipe reached past
    // the `LineWriter`'s buffer), which would break the exit-0 guarantee
    // every hook registration depends on.
    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(out.as_bytes())
        .and_then(|()| stdout.flush());
}

fn emit_schema() {
    use std::io::Write as _;
    let document = match mcpls_core::config::schema::document() {
        Ok(document) => document,
        Err(err) => {
            eprintln!("failed to generate config schema: {err}");
            std::process::exit(1);
        }
    };
    let mut out = std::io::stdout().lock();
    if let Err(err) = out
        .write_all(document.as_bytes())
        .and_then(|()| out.flush())
    {
        eprintln!("failed to write config schema: {err}");
        std::process::exit(1);
    }
}

/// Print the session brief for the project a hook names, then exit 0.
///
/// A brief is context, never a gate: a config that fails to load gets no
/// brief rather than a failed session start.
fn emit_brief(args: &Args, additional_context: bool) -> ! {
    let (directory, _) = examined_directory(None);
    let root = hook::checkout_root(&directory);
    let text = resolve_config(args, &root)
        .ok()
        .and_then(|resolved| brief::render(&resolved.config, &root));
    match text {
        Some(text) if additional_context => {
            write_report(&format!("{}\n", brief::session_start_output(&text)));
        }
        Some(text) => write_report(&text),
        None => {}
    }
    std::process::exit(0);
}

/// Print this checkout's backend, or with `all` every backend this user
/// runs, then exit.
async fn emit_status(all: bool) -> ! {
    let report = if all {
        status::all().await
    } else {
        let (directory, _) = examined_directory(None);
        status::checkout(&hook::checkout_root(&directory)).await
    };
    match report {
        Ok(text) => write_report(&text),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}

async fn run(args: Args) -> Result<()> {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting mcpls");

    if let Some(Command::Backend {
        root: Some(root), ..
    }) = &args.command
    {
        let config = load_config(&args, root)?;
        mcpls_core::backend::serve_backend(config, root.clone())
            .await
            .context("backend error")?;
        return Ok(());
    }

    let cwd = std::env::current_dir().context("failed to read the working directory")?;
    let root = mcpls_core::hooks::project_root(&cwd).unwrap_or(cwd);
    let config = load_config(&args, &root)?;

    tracing::debug!(
        lsp_servers = config.lsp_servers.len(),
        "configuration loaded"
    );

    #[cfg(feature = "transport-http")]
    let in_process = args.no_backend || args.listen.is_some();
    #[cfg(not(feature = "transport-http"))]
    let in_process = args.no_backend;

    if !in_process {
        let stamp = mcpls_core::backend::ConfigStamp::of(&config);
        let launch = backend_launch(&args, root);
        mcpls_core::backend::run_frontend(mcpls_core::backend::FrontendOptions { launch, stamp })
            .await;
        return Ok(());
    }

    let transport = {
        #[cfg(feature = "transport-http")]
        {
            match args.listen {
                Some(bind) => mcpls_core::Transport::Http(mcpls_core::HttpConfig::new(
                    bind,
                    args.http_path.clone(),
                )),
                None => mcpls_core::Transport::Stdio,
            }
        }
        #[cfg(not(feature = "transport-http"))]
        {
            mcpls_core::Transport::Stdio
        }
    };

    mcpls_core::serve_with(config, transport)
        .await
        .context("server error")?;

    tracing::info!("mcpls shutdown complete");
    Ok(())
}

/// What a backend for `root` is started with, from this invocation's flags.
fn backend_launch(args: &Args, root: std::path::PathBuf) -> mcpls_core::backend::BackendLaunch {
    mcpls_core::backend::BackendLaunch {
        root,
        config: args
            .config
            .as_ref()
            .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone())),
        trust_project_config: args.trust_project_config,
        log_level: args.log_level.clone(),
        log_json: args.log_json,
    }
}

/// Start, stop, or report on the backend for the checkout `action` names.
async fn control_backend(args: &Args, action: &BackendAction) -> backend::Outcome {
    use mcpls_core::backend::control;

    let path = match action {
        BackendAction::Start { path }
        | BackendAction::Stop { path, .. }
        | BackendAction::Auto { path }
        | BackendAction::Status { path } => path.as_deref(),
    };
    let (directory, _) = examined_directory(path);
    let root = hook::checkout_root(&directory);
    let identity = match mcpls_core::hooks::identity_for(&root) {
        Ok(identity) => identity,
        Err(error) => {
            return backend::Outcome {
                text: format!("no backend endpoint for {}: {error}\n", root.display()),
                success: false,
            };
        }
    };
    match action {
        BackendAction::Status { .. } => backend::status(
            control::status(&identity).await,
            &root,
            &identity.log_file(),
        ),
        BackendAction::Stop { force, .. } => {
            backend::stop(control::stop(&identity, *force).await, &root)
        }
        BackendAction::Auto { .. } => backend::auto(control::release(&identity).await, &root),
        BackendAction::Start { .. } => {
            // The backend would fail on the same configuration, with the
            // reason only in its log.
            if let Err(error) = resolve_config(args, &root) {
                return backend::Outcome {
                    text: format!("{error:?}\n"),
                    success: false,
                };
            }
            let exe = match std::env::current_exe() {
                Ok(exe) => exe,
                Err(error) => {
                    return backend::Outcome {
                        text: format!("failed to locate the mcpls executable: {error}\n"),
                        success: false,
                    };
                }
            };
            let launch = backend_launch(args, root.clone());
            backend::start(
                control::start(&identity, &exe, &launch).await,
                &root,
                &identity.log_file(),
            )
        }
    }
}

/// Write a report to stdout, treating a reader that closed early
/// (`mcpls config | head`) as done rather than a crash: `print!` panics on
/// a broken pipe, and both reports are long enough that paging them is the
/// norm.
fn write_report(text: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
}

/// The directory a doctor or config run examines, canonicalized, and where
/// its name came from.
fn examined_directory(argument: Option<&std::path::Path>) -> (std::path::PathBuf, Examined) {
    let (named, examined) = hook::examined_directory(argument);
    (dunce::canonicalize(&named).unwrap_or(named), examined)
}

/// Probe the backend for a directory and report what answers.
///
/// Reads the same `CLAUDE_PROJECT_DIR` a hook does, canonicalized the same
/// way, so with no argument it probes exactly the socket a real hook
/// invocation would.
async fn diagnose(args: &Args, path: Option<&std::path::Path>) -> Report {
    let (directory, examined) = examined_directory(path);
    let root = hook::checkout_root(&directory);
    let local = resolve_config(args, &root).ok();
    match mcpls_core::hooks::identity_for(&root) {
        Ok(identity) => hook::doctor(&directory, examined, &root, &identity, local.as_ref()).await,
        Err(error) => hook::doctor_without_identity(&directory, examined, &error),
    }
}

/// Run one `mcpls lsp` action against the backend for its directory.
async fn run_lsp(args: &Args, action: &LspCommand) -> lsp::Outcome {
    let (dir, change) = match action {
        LspCommand::Install { targets, dry_run } => {
            return run_install(args, targets, *dry_run).await;
        }
        LspCommand::Status { dir } => (dir.as_deref(), None),
        LspCommand::Start { targets, no_wait } => (
            targets.dir.as_deref(),
            Some((targets, LspAction::Start, !no_wait)),
        ),
        LspCommand::Restart { targets, no_wait } => (
            targets.dir.as_deref(),
            Some((targets, LspAction::Restart, !no_wait)),
        ),
        LspCommand::Stop { targets } => (
            targets.dir.as_deref(),
            Some((targets, LspAction::Stop, false)),
        ),
    };
    let (directory, _) = examined_directory(dir);
    let root = hook::checkout_root(&directory);
    let identity = match mcpls_core::hooks::identity_for(&root) {
        Ok(identity) => identity,
        Err(error) => {
            return lsp::Outcome {
                text: format!(
                    "cannot locate the backend for {}: {error}\n",
                    root.display()
                ),
                success: false,
            };
        }
    };
    let Some((targets, lsp_action, wait)) = change else {
        return lsp::status(&identity, &root).await;
    };
    let servers = if targets.all {
        Vec::new()
    } else {
        targets.servers.clone()
    };
    let ceiling = wait.then(|| wait_ceiling(resolve_config(args, &root).ok().as_ref()));
    lsp::control(&identity, &root, lsp_action, servers, ceiling).await
}

/// Install the missing language servers `targets` names, from the
/// configuration for its directory.
async fn run_install(args: &Args, targets: &LspTargets, dry_run: bool) -> lsp::Outcome {
    let (directory, _) = examined_directory(targets.dir.as_deref());
    let root = hook::checkout_root(&directory);
    let failed = |text: String| lsp::Outcome {
        text: format!("{text}\n"),
        success: false,
    };
    let resolved = match resolve_config(args, &root) {
        Ok(resolved) => resolved,
        Err(error) => return failed(format!("{error:#}")),
    };
    let named: &[String] = if targets.all { &[] } else { &targets.servers };
    let planned = match install::plan(&resolved.config, &root, named) {
        Ok(planned) => planned,
        Err(message) => return failed(message),
    };
    let results = install::install(&planned, &root, dry_run, |program| {
        hook::resolve_program(program, &root).is_some()
    })
    .await;
    let success = planned
        .iter()
        .zip(&results)
        .all(|(target, (_, status))| !status.fails(target.named));
    lsp::Outcome {
        text: install::render(&results),
        success,
    }
}

/// The longest any configured server may take to finish `initialize`.
fn wait_ceiling(resolved: Option<&mcpls_core::Resolved>) -> std::time::Duration {
    let seconds = resolved
        .and_then(|resolved| {
            resolved
                .config
                .lsp_servers
                .iter()
                .map(|server| server.timeout_seconds)
                .max()
        })
        .unwrap_or(30);
    std::time::Duration::from_secs(seconds)
}

/// Load the configuration a session in `root` runs with.
fn load_config(args: &Args, root: &std::path::Path) -> Result<mcpls_core::ServerConfig> {
    Ok(resolve_config(args, root)?.config)
}

/// The configuration a session in `root` runs with, and where it came from.
fn resolve_config(args: &Args, root: &std::path::Path) -> Result<mcpls_core::Resolved> {
    if let Some(config_path) = &args.config {
        let config = mcpls_core::ServerConfig::load_from(config_path)
            .with_context(|| format!("failed to load config from {}", config_path.display()))?;
        return Ok(mcpls_core::Resolved {
            config,
            path: Some(config_path.clone()),
            ignored_project_config: None,
        });
    }
    let trust = if args.trust_project_config {
        ProjectConfigTrust::Trusted
    } else {
        ProjectConfigTrust::Untrusted
    };
    mcpls_core::ServerConfig::resolve_at(trust, root).context("failed to load configuration")
}
