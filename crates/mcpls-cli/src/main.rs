//! MCPLS - Universal MCP to LSP Bridge
//!
//! This binary provides an MCP server that exposes LSP capabilities as tools,
//! enabling AI agents to access semantic code intelligence.

use anyhow::{Context, Result};
use clap::Parser;
use mcpls_core::ProjectConfigTrust;

mod args;
mod completions;
mod hook;
mod logging;

use args::{Args, Command, HookAction};

#[tokio::main]
async fn main() {
    let args = Args::parse();

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

    // A hook invocation needs neither a loaded config nor a log subscriber,
    // and reading stdin and writing hook JSON is the whole command.
    if let Some(Command::Hook { action }) = &args.command {
        match action {
            None => {
                use std::io::{Read as _, Write as _};
                let mut stdin = String::new();
                let _ = std::io::stdin().read_to_string(&mut stdin);
                let raw_project_dir = std::env::var_os("CLAUDE_PROJECT_DIR")
                    .map_or_else(|| std::path::PathBuf::from("."), std::path::PathBuf::from);
                // A watch path is consumed by the host, which has no
                // obligation to resolve it against this process's own
                // working directory, so it must be absolute. Falling back
                // to the raw path on a canonicalization failure keeps
                // today's relative-path behaviour as the floor rather than
                // turning it into a hard error.
                let project_dir = dunce::canonicalize(&raw_project_dir).unwrap_or(raw_project_dir);
                let root = hook::checkout_root(&project_dir);
                // A failed `identity_for` (an unreachable directory, or an
                // over-long socket path) must not suppress `SessionStart`:
                // that arm never touches the socket, which is the entire
                // reason it exists, so every socket-using arm degrades on
                // its own when `identity` is `None` rather than the whole
                // dispatch short-circuiting here.
                let identity = mcpls_core::hooks::identity_for(&root).ok();
                #[cfg(windows)]
                if let (Some(identity), Ok(exe)) = (identity.as_ref(), std::env::current_exe()) {
                    // The frontend cannot spawn a backend that outlives a
                    // job-contained session, so it asks and a hook starts it.
                    let _ = mcpls_core::backend::start_requested(identity, &exe).await;
                }
                let out = hook::dispatch_payload(&stdin, &root, identity.as_ref()).await;
                // `print!` panics on a write failure (a closed stdout pipe
                // reached past the `LineWriter`'s buffer), which would
                // break the exit-0 guarantee this whole command exists to
                // uphold. `completions::emit` already solves this the same
                // way.
                let mut stdout = std::io::stdout().lock();
                let _ = stdout
                    .write_all(out.as_bytes())
                    .and_then(|()| stdout.flush());
            }
            Some(HookAction::Doctor) => {
                // `mcpls hook doctor` reports on the socket and the install.
                // It reads the same `CLAUDE_PROJECT_DIR` the hook itself
                // does, canonicalized the same way, so it probes exactly
                // the socket a real hook invocation would.
                let raw_project_dir = std::env::var_os("CLAUDE_PROJECT_DIR")
                    .map_or_else(|| std::path::PathBuf::from("."), std::path::PathBuf::from);
                let project_dir = dunce::canonicalize(&raw_project_dir).unwrap_or(raw_project_dir);
                let root = hook::checkout_root(&project_dir);
                let out = match mcpls_core::hooks::identity_for(&root) {
                    Ok(identity) => hook::doctor(&project_dir, &root, &identity).await,
                    Err(error) => hook::doctor_without_identity(&project_dir, &root, &error),
                };
                println!("{out}");
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

    // `#[tokio::main]`'s generated wrapper blocks in `Runtime::drop` ->
    // `BlockingPool::shutdown` after this function returns, waiting for
    // every outstanding spawn_blocking thread -- including the one
    // `rmcp::transport::stdio()` (== `tokio::io::stdin()`) parks in a raw,
    // uncancellable `read()` on the real stdin fd. That read only returns on
    // more input or EOF, so if the MCP client's write end of stdin is still
    // open, the wait never completes even though `run()` above (which
    // includes LSP server shutdown and all shutdown logging) has already
    // finished. `process::exit` terminates immediately, bypassing that wait
    // -- safe here because everything that matters has already completed
    // above. See #308.
    std::process::exit(exit_code);
}

async fn run(args: Args) -> Result<()> {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting mcpls");

    if let Some(Command::Backend { root }) = &args.command {
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
        let launch = mcpls_core::backend::BackendLaunch {
            root,
            config: args
                .config
                .as_ref()
                .map(|path| dunce::canonicalize(path).unwrap_or_else(|_| path.clone())),
            trust_project_config: args.trust_project_config,
            log_level: args.log_level.clone(),
            log_json: args.log_json,
        };
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

/// Load the configuration a session in `root` runs with.
fn load_config(args: &Args, root: &std::path::Path) -> Result<mcpls_core::ServerConfig> {
    if let Some(config_path) = &args.config {
        return mcpls_core::ServerConfig::load_from(config_path)
            .with_context(|| format!("failed to load config from {}", config_path.display()));
    }
    let trust = if args.trust_project_config {
        ProjectConfigTrust::Trusted
    } else {
        ProjectConfigTrust::Untrusted
    };
    mcpls_core::ServerConfig::load_at(trust, root).context("failed to load configuration")
}
