//! End-to-end test proving `workspace/didChangeWatchedFiles` reaches a real
//! pyrefly and changes its analysis of a document it holds open.
//!
//! # Process model
//!
//! A single `#[test] fn pyrefly_e2e_suite()` drives the whole suite. nextest
//! sees exactly one test -> one process -> pyrefly is spawned once.
//!
//! # Skip policy
//!
//! - `MCPLS_SKIP_PYREFLY=1`   -> print skip line, return success
//! - `MCPLS_PYREFLY=<path>`   -> use that binary
//! - `pyrefly` found in PATH  -> use it
//! - not found and no skip flag -> panic (fail closed)

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_docs_in_private_items,
    missing_docs
)]

#[path = "common/assertions.rs"]
mod assertions;
#[path = "e2e/mcp_client.rs"]
mod mcp_client;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mcp_client::McpClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Binary resolution
// ---------------------------------------------------------------------------

enum Resolution {
    Found(PathBuf),
    Skipped(&'static str),
    Missing,
}

/// Resolve the pyrefly binary path.
///
/// Priority:
/// 1. `MCPLS_SKIP_PYREFLY=1` -> `Skipped`
/// 2. `MCPLS_PYREFLY=<path>` -> `Found(<path>)`
/// 3. `pyrefly` in PATH -> `Found`
/// 4. Otherwise -> `Missing`
fn resolve_pyrefly() -> Resolution {
    if std::env::var_os("MCPLS_SKIP_PYREFLY").is_some_and(|v| v == "1") {
        return Resolution::Skipped("MCPLS_SKIP_PYREFLY=1");
    }

    if let Some(p) = std::env::var_os("MCPLS_PYREFLY") {
        return Resolution::Found(PathBuf::from(p));
    }

    if std::process::Command::new("pyrefly")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
        && let Some(p) = find_in_path("pyrefly")
    {
        return Resolution::Found(p);
    }

    Resolution::Missing
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let executable = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(&executable);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Workspace staging
// ---------------------------------------------------------------------------

/// Copy `tests/fixtures/python_workspace/` into a fresh `TempDir`.
fn stage_workspace() -> TempDir {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python_workspace");
    let tmp = TempDir::new().expect("failed to create TempDir");
    copy_dir_recursive(&fixture_dir, tmp.path()).expect("failed to copy fixture workspace");
    tmp
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config generation
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct E2eConfig {
    workspace: WorkspaceConfig,
    lsp_servers: Vec<LspServerConfig>,
    apply: ApplyTable,
    diagnostics: DiagnosticsTable,
}

/// How long nothing may be outstanding before `bridge::settle::ServerSettle`
/// treats a session as quiet, in milliseconds.
///
/// pyrefly never sends `$/progress`, so its own one-time session baseline
/// settles through `ServerSettle`'s fixed no-progress grace instead of
/// through this value; it is left short here only because nothing in this
/// suite depends on a longer one.
#[derive(Serialize, Deserialize)]
struct DiagnosticsTable {
    settle_quiet_ms: u64,
    hooks: HooksTable,
}

/// Hooks off. These suites exercise the MCP and LSP layers, and leaving
/// hooks on would have every spawned server bind a socket and a lock in the
/// user's shared runtime directory, which nothing ever removes.
#[derive(Serialize, Deserialize)]
struct HooksTable {
    enabled: bool,
}

#[derive(Serialize, Deserialize)]
struct ApplyTable {
    rename: bool,
}

#[derive(Serialize, Deserialize)]
struct WorkspaceConfig {
    roots: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct LspServerConfig {
    language_id: String,
    command: String,
    args: Vec<String>,
    file_patterns: Vec<String>,
    heuristics: Heuristics,
}

#[derive(Serialize, Deserialize)]
struct Heuristics {
    project_markers: Vec<String>,
}

/// The bundled `python` entry inherits pyright's project markers
/// (`pyproject.toml`, `setup.py`, ...) unless this entry overrides them, and
/// the fixture carries none of those -- only `pyrefly.toml`.
///
/// `--indexing-mode lazy-blocking` is pyrefly's own recommended setting for
/// deterministic testing: under the default background mode, a rename
/// issued immediately after opening a document can race pyrefly's
/// cross-file symbol index and get rejected as touching a "third-party"
/// symbol before that index catches up.
fn write_config(pyrefly_binary: &Path, workspace_root: &Path, config_path: &Path) {
    let cfg = E2eConfig {
        workspace: WorkspaceConfig {
            roots: vec![workspace_root.to_string_lossy().into_owned()],
        },
        lsp_servers: vec![LspServerConfig {
            language_id: "python".to_owned(),
            command: pyrefly_binary.to_string_lossy().into_owned(),
            args: vec![
                "lsp".to_owned(),
                "--indexing-mode".to_owned(),
                "lazy-blocking".to_owned(),
            ],
            file_patterns: vec!["**/*.py".to_owned()],
            heuristics: Heuristics {
                project_markers: vec!["pyrefly.toml".to_owned()],
            },
        }],
        apply: ApplyTable { rename: true },
        diagnostics: DiagnosticsTable {
            settle_quiet_ms: 200,
            hooks: HooksTable { enabled: false },
        },
    };
    let content = toml::to_string(&cfg).expect("failed to serialize e2e config");
    fs::write(config_path, content).expect("failed to write e2e config");
}

// ---------------------------------------------------------------------------
// Anchor helpers
// ---------------------------------------------------------------------------

/// Find the 1-based line number of the first line in `file` containing `needle`.
fn find_line(file: &Path, needle: &str) -> u32 {
    let content = fs::read_to_string(file).expect("failed to read file for anchor search");
    content
        .lines()
        .enumerate()
        .find_map(|(i, line)| {
            if line.contains(needle) {
                Some(u32::try_from(i + 1).expect("line number fits u32"))
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("anchor '{needle}' not found in {}", file.display()))
}

/// How long the settle flush is given to deliver a real report, in milliseconds.
///
/// `MCPLS_PYREFLY_SETTLE_DEADLINE_MS` overrides the default for a slow runner.
fn settle_deadline_ms() -> u64 {
    std::env::var("MCPLS_PYREFLY_SETTLE_DEADLINE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60_000)
}

/// Poll `get_new_diagnostics` until mcpls has taken its one-time session
/// baseline, so the sub-case's own apply is the first thing that can ever
/// count as a change.
///
/// The baseline is a single snapshot taken once the servers have gone quiet
/// for `settle_quiet_ms` (`bridge::settle::ServerSettle`), independent of
/// any particular tool call. pyrefly answers `initialize`, `didOpen` and
/// `rename` almost immediately and never sends `$/progress`, so a fixed
/// sub-case with no wait of its own can finish opening a document and
/// renaming a symbol before that quiet window closes -- fast enough that
/// the diagnostic the rename causes lands in the cache the baseline snapshot
/// reads, rather than after it, and then never appears as a "change" no
/// matter how long the sub-case polls afterward.
fn wait_for_baseline(client: &mut McpClient) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = client
            .call_tool("get_new_diagnostics", &json!({}))
            .expect("get_new_diagnostics failed while waiting for the baseline");
        let text = assertions::assert_tool_ok(&resp);
        let report: Value = serde_json::from_str(&text).expect("bad JSON from get_new_diagnostics");
        let omitted = report["omitted"].as_u64().unwrap_or(0);
        let starting_up = report.get("note").is_some() && omitted == 0;
        if !starting_up {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "mcpls never took its diagnostics baseline within 10s of spawning"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// Sub-case infrastructure
// ---------------------------------------------------------------------------

struct SubResult {
    name: &'static str,
    outcome: Result<(), String>,
}

type SubCaseFn = fn(&mut McpClient, &Path) -> Result<(), String>;

struct SubCase {
    name: &'static str,
    run: SubCaseFn,
}

macro_rules! sub_case {
    ($name:ident) => {
        SubCase {
            name: stringify!($name),
            run: $name,
        }
    };
}

// ---------------------------------------------------------------------------
// Sub-cases
// ---------------------------------------------------------------------------

fn open_document(
    client: &mut McpClient,
    file: &Path,
    anchor: &str,
    character: u32,
) -> Result<(), String> {
    let open_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let outcome = client.call_tool(
            "get_hover",
            &json!({
                "file_path": file.to_string_lossy(),
                "line": find_line(file, anchor),
                "character": character,
            }),
        );
        match outcome {
            Ok(_) => return Ok(()),
            Err(e)
                if Instant::now() < open_deadline
                    && e.to_string().contains("LSP server error: -32800 -") =>
            {
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => return Err(format!("opening {} failed: {e}", file.display())),
        }
    }
}

/// A watched-files notification reaches pyrefly and changes its analysis of
/// a document it holds open.
///
/// `b.py` is opened through a read tool and never written. `a.py` is
/// written by the apply's fanout and never opened. Afterwards `b.py`'s call
/// is a type error, and it is one only against `a.py`'s new content, so the
/// error can only have arrived because mcpls told pyrefly that `a.py`
/// changed and pyrefly re-analysed the document it holds.
///
/// The rename is anchored at `c.py`, not `b.py`: pyrefly never opens `a.py`,
/// so nothing about `a.py`'s definition is reachable from a symbol pyrefly
/// has indexed unless something else already holds a live reference to it.
/// `c.py`'s `helper` call is that reference. Anchoring the rename at `b.py`
/// instead would target the very call this sub-case checks, collapsing the
/// direction the notification has to travel.
fn sc_watched_files_reaches_pyrefly(
    client: &mut McpClient,
    workspace: &Path,
) -> Result<(), String> {
    let b = workspace.join("b.py");
    let c = workspace.join("c.py");
    for (file, anchor, character) in [(&b, "RESULT = greet", 9), (&c, "USED = helper", 8)] {
        open_document(client, file, anchor, character)?;
    }

    // Drain the baseline and document opens so the assertion
    // below is about what this apply caused.
    let _ = client
        .call_tool("get_new_diagnostics", &json!({}))
        .map_err(|e| format!("priming flush failed: {e}"))?;

    // Character 8 (1-based) lands on the 'h' of `helper`. Pyrefly resolves a
    // rename target more strictly than hover: a cursor one column short,
    // still adjacent to the token, resolves to no local symbol at all and
    // pyrefly answers "third-party symbols cannot be renamed" instead of
    // the intended local one.
    let resp = client
        .call_tool(
            "rename_symbol",
            &json!({
                "file_path": c.to_string_lossy(),
                "line": find_line(&c, "USED = helper"),
                "character": 8,
                "new_name": "greet",
                "apply": true,
            }),
        )
        .map_err(|e| format!("rename call failed: {e}"))?;
    let text = assertions::assert_tool_ok(&resp);
    let inner: Value = serde_json::from_str(&text).map_err(|e| format!("bad JSON: {e}"))?;
    if inner["applied"] != json!(true) {
        return Err(format!("expected applied=true, got {inner}"));
    }
    let written: Vec<String> = inner["files_written"]
        .as_array()
        .ok_or_else(|| format!("expected files_written array, got {inner}"))?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if !written.iter().any(|p| p.ends_with("a.py")) {
        return Err(format!(
            "the apply must have rewritten a.py; wrote {written:?}"
        ));
    }
    if written.iter().any(|p| p.ends_with("b.py")) {
        return Err(format!(
            "the apply must not have rewritten b.py, or a didChange would \
             deliver the error and this sub-case would prove nothing; \
             wrote {written:?}"
        ));
    }

    let deadline = Instant::now() + Duration::from_millis(settle_deadline_ms());
    let mut last = Value::Null;
    loop {
        let raw = client
            .call_tool("get_new_diagnostics", &json!({}))
            .map_err(|e| format!("flush call failed: {e}"))?;
        let body = assertions::assert_tool_ok(&raw);
        let report: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
        let omitted = report["omitted"].as_u64().unwrap_or(0);
        let starting_up = report.get("note").is_some() && omitted == 0;
        if !starting_up {
            let hit = report["changed"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|file| {
                    file["file_path"]
                        .as_str()
                        .is_some_and(|p| p.ends_with("b.py"))
                        && file["diagnostics"]
                            .as_array()
                            .is_some_and(|ds| !ds.is_empty())
                });
            if hit {
                return Ok(());
            }
            last = report;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no diagnostic for b.py arrived within the settle deadline. \
                 b.py was never written and a.py was never opened, so this \
                 failing means the workspace/didChangeWatchedFiles for a.py \
                 either did not go out or pyrefly did not act on it. Check \
                 the mcpls log for 'skipping a relative watcher pattern' \
                 before assuming the former. Last report: {last}"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------------------
// Suite driver
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Requires pyrefly in PATH; set MCPLS_SKIP_PYREFLY=1 to skip or MCPLS_PYREFLY=<path> to override"]
fn pyrefly_e2e_suite() {
    let pyrefly_path = match resolve_pyrefly() {
        Resolution::Found(p) => p,
        Resolution::Skipped(reason) => {
            println!("[pyrefly_e2e] suite skipped: {reason}");
            return;
        }
        Resolution::Missing => {
            panic!(
                "[pyrefly_e2e] pyrefly not found in PATH; install it or set \
                 MCPLS_SKIP_PYREFLY=1 to skip"
            );
        }
    };

    println!("[pyrefly_e2e] using pyrefly: {}", pyrefly_path.display());

    let workspace_tmp = stage_workspace();
    let workspace = workspace_tmp
        .path()
        .canonicalize()
        .unwrap_or_else(|_| workspace_tmp.path().to_owned());

    let config_path = workspace.join("mcpls-e2e.toml");
    write_config(&pyrefly_path, &workspace, &config_path);

    let config_str = config_path.to_string_lossy().into_owned();
    let mut client =
        McpClient::spawn_with_args(&["--config", &config_str]).expect("failed to spawn mcpls");

    client.initialize().expect("MCP initialize failed");
    wait_for_baseline(&mut client);

    let sub_cases: &[SubCase] = &[sub_case!(sc_watched_files_reaches_pyrefly)];

    let filter = std::env::var("MCPLS_PYREFLY_FILTER").ok();

    let mut results: Vec<SubResult> = Vec::new();

    for sc in sub_cases {
        if filter.as_deref().is_some_and(|f| !sc.name.contains(f)) {
            continue;
        }

        print!("[pyrefly_e2e] running {} … ", sc.name);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (sc.run)(&mut client, &workspace)
        }));

        let outcome = match outcome {
            Ok(r) => r,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "sub-case panicked".to_owned());
                Err(msg)
            }
        };

        match &outcome {
            Ok(()) => println!("ok"),
            Err(e) => println!("FAILED: {e}"),
        }

        results.push(SubResult {
            name: sc.name,
            outcome,
        });
    }

    let failures: Vec<_> = results.iter().filter(|r| r.outcome.is_err()).collect();

    if !failures.is_empty() {
        let report: Vec<String> = failures
            .iter()
            .map(|f| format!("  • {} — {}", f.name, f.outcome.as_ref().unwrap_err()))
            .collect();
        panic!(
            "[pyrefly_e2e] {} sub-case(s) failed:\n{}",
            failures.len(),
            report.join("\n")
        );
    }

    println!("[pyrefly_e2e] all {} sub-cases passed", results.len());
}
