//! Dispatching one Claude Code hook invocation.
//!
//! A hook registration spawns `mcpls hook`, writes one JSON payload to its
//! stdin, and reads one JSON payload back from its stdout. Routing on the
//! payload's own `hook_event_name` here, in one binary, means there is no
//! shell script translating five hook names into five subcommands, and the
//! same registrations work unmodified on Windows.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use mcpls_core::hooks::{
    ChangeEvent, ProbeOutcome, Request, Response, SocketIdentity, probe, send, send_many,
    watch_paths,
};
use serde::Deserialize;

/// How long a hook waits for an answer to `Changed` or `EndSession`, which
/// carry no context back and are never worth stalling an edit for.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(50);

/// How long a hook waits for an answer that can carry `additionalContext`:
/// `UserPromptSubmit`'s `Flush`, and the `Flush` half of a `PostToolBatch`.
/// Tracks the spec's `op_deadline_ms` default of 1500: a client timeout
/// tighter than the server's own deadline for finishing that work is never
/// right, since it only cuts off answers from an owner that is going to
/// reply anyway. A missing owner still fails fast, because the connect
/// itself fails immediately rather than waiting out this bound.
const FLUSH_SOCKET_TIMEOUT: Duration = Duration::from_millis(1500);

/// The hook payload Claude Code writes to stdin, keeping only the fields
/// the dispatch table below reads. Every field is optional or defaulted,
/// since which ones are present depends on `hook_event_name`.
#[derive(Debug, Deserialize)]
struct HookPayload {
    hook_event_name: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    file_path: Option<PathBuf>,
    #[serde(default)]
    event: Option<ChangeEvent>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

/// One entry of `PostToolBatch`'s `tool_calls`, keeping only the field its
/// `changed` request needs.
#[derive(Debug, Deserialize)]
struct ToolCall {
    tool_input: ToolInput,
}

#[derive(Debug, Deserialize)]
struct ToolInput {
    #[serde(default)]
    file_path: Option<PathBuf>,
}

/// Await `body`, and swallow whatever it does wrong.
///
/// An edit must never fail because diagnostics were unavailable, so there
/// is exactly one exit code and it is zero. The cost is that a broken
/// installation is invisible, which is what `mcpls hook doctor` exists to
/// answer. This covers an `Err` from `body`; nothing on this path panics
/// today, so there is no `catch_unwind` here, but a caller relying on this
/// wrapper for panic safety would be relying on something it does not do.
///
/// Takes a future rather than a closure: everything it wraps goes through
/// `hooks::send`, which is `async`, and a synchronous `FnOnce` could not
/// contain the await, so the one-wrapper guarantee would not hold.
async fn silently<T: Default>(body: impl Future<Output = Result<T>>) -> T {
    body.await.unwrap_or_default()
}

/// Read one hook payload from `stdin` and return the hook JSON to print,
/// dispatching on the payload's own `hook_event_name` against the socket
/// `identity` names.
///
/// `identity` is `None` when the caller could not derive one (an
/// unreachable or over-long project directory): `SessionStart` still
/// answers, since it never touches the socket, and every other arm
/// degrades to the empty string exactly as it would against an
/// unreachable socket.
///
/// Every fault, a missing identity, a missing socket, a connect timeout, a
/// malformed response, a payload that will not parse, produces the empty
/// string.
pub async fn dispatch_payload(
    stdin: &str,
    project_dir: &Path,
    identity: Option<&SocketIdentity>,
) -> String {
    silently(run(stdin, project_dir, identity)).await
}

async fn run(stdin: &str, project_dir: &Path, identity: Option<&SocketIdentity>) -> Result<String> {
    let payload: HookPayload = serde_json::from_str(stdin)?;

    match payload.hook_event_name.as_str() {
        // `SessionStart` fires while the host is still spawning the MCP
        // server, so the socket may not be bound yet. Computing the watch
        // set locally, rather than asking over the socket, is what keeps
        // this hook from either leaving the session with no coverage or an
        // unbounded watcher over target/. It never touches `identity`.
        "SessionStart" => Ok(session_start_output(&watch_paths(project_dir))),

        "FileChanged" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            let Some(file_path) = payload.file_path else {
                return Ok(String::new());
            };
            let event = payload.event.unwrap_or(ChangeEvent::Change);
            send(
                identity,
                &Request::Changed {
                    session: payload.session_id,
                    paths: vec![file_path],
                    event,
                },
                SOCKET_TIMEOUT,
            )
            .await?;
            Ok(String::new())
        }

        "PostToolBatch" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            let paths = payload
                .tool_calls
                .into_iter()
                .filter_map(|call| call.tool_input.file_path)
                .collect();
            let requests = [
                Request::Changed {
                    session: payload.session_id.clone(),
                    paths,
                    event: ChangeEvent::Change,
                },
                Request::Flush {
                    session: payload.session_id,
                },
            ];
            let responses = send_many(identity, &requests, FLUSH_SOCKET_TIMEOUT).await?;
            let context = responses.into_iter().nth(1).and_then(flush_context);
            Ok(additional_context_output(context))
        }

        "UserPromptSubmit" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            let response = send(
                identity,
                &Request::Flush {
                    session: payload.session_id,
                },
                FLUSH_SOCKET_TIMEOUT,
            )
            .await?;
            Ok(additional_context_output(flush_context(response)))
        }

        "SessionEnd" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            send(
                identity,
                &Request::EndSession {
                    session: payload.session_id,
                },
                SOCKET_TIMEOUT,
            )
            .await?;
            Ok(String::new())
        }

        // `Stop` is deliberately absent. Its `additionalContext` is
        // documented as non-error feedback after which the conversation
        // continues so the model can act on it, so flushing there would
        // turn every new warning into a keep-working signal. The next
        // `UserPromptSubmit` delivers the same diagnostics anyway.
        _ => Ok(String::new()),
    }
}

/// The context a `Flush` response carries, or `None` for any other answer.
fn flush_context(response: Response) -> Option<String> {
    if let Response::Flush { context } = response {
        context
    } else {
        None
    }
}

/// The `SessionStart` hook's JSON, naming the directories to watch.
fn session_start_output(paths: &[PathBuf]) -> String {
    let watch_paths: Vec<String> = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    serde_json::json!({ "hookSpecificOutput": { "watchPaths": watch_paths } }).to_string()
}

/// The hook JSON carrying diagnostics context, or the empty string when
/// there is nothing to report.
fn additional_context_output(context: Option<String>) -> String {
    context
        .filter(|text| !text.is_empty())
        .map_or_else(String::new, |text| {
            serde_json::json!({ "hookSpecificOutput": { "additionalContext": text } }).to_string()
        })
}

/// How many candidate sockets or pipes the doctor probes when this
/// project's own socket answers nobody, so a runtime directory a stale
/// process left cluttered cannot turn one doctor run into an unbounded
/// wait for a human sitting at a terminal.
const MAX_FOREIGN_CANDIDATES: usize = 16;

/// Probe this project's hook socket and describe what it finds: the
/// socket path, the directory hash each side computes, whether an owner
/// answers and how many hooks it has actually seen, and whether `mcpls`
/// resolves on `PATH`.
///
/// Every other command in this feature fails silently by design, so a
/// broken install looks exactly like a quiet workspace. This is the one
/// command allowed to print on failure, because breaking that silence is
/// its entire purpose.
pub async fn doctor(project_dir: &Path, identity: &SocketIdentity) -> String {
    doctor_scanning(project_dir, identity, foreign_scan_prefix()).await
}

/// `doctor`'s body, parameterized on the prefix its runtime-directory scan
/// filters Windows pipe names by.
///
/// Production always reaches this through `doctor`, with the real prefix.
/// A test on Windows needs a different one: the pipe namespace is
/// machine-global, so a scan filtered on the real prefix would enumerate
/// an actual mcpls running on the developer's own machine, not only the
/// one the test bound itself.
async fn doctor_scanning(project_dir: &Path, identity: &SocketIdentity, prefix: &str) -> String {
    let mut lines = vec![
        format!("socket: {}", identity.socket.display()),
        format!("hook sees: {} -> {}", project_dir.display(), identity.hash),
    ];

    match probe(identity, &Request::Status, SOCKET_TIMEOUT).await {
        ProbeOutcome::Answered(Response::Status {
            hash,
            pid,
            root,
            hooks_seen,
            ..
        }) => {
            lines.push(format!("server sees: {} -> {hash}", root.display()));
            lines.push(format!("owner pid: {pid}"));
            lines.push(hooks_seen_line(hooks_seen));
        }
        // An owner deliberately explained itself; print that rather than
        // discarding it behind a timing guess.
        ProbeOutcome::Answered(Response::Error { message }) => {
            lines.push(format!(
                "server sees: an owner answered with an error: {message}"
            ));
            lines.push(OWNER_PID_UNKNOWN.to_string());
        }
        // Some other, unexpected answer to a Status request. An owner
        // exists, evidenced by the answer itself, so this is not a
        // no-owner state either.
        ProbeOutcome::Answered(other) => {
            lines.push(format!(
                "server sees: an owner answered, but not with its own status: {other:?}"
            ));
            lines.push(OWNER_PID_UNKNOWN.to_string());
        }
        // A connection was accepted and the exchange then failed, on the
        // write, the read, an early hang-up, or the parse. The error
        // carries which; the line says only what all four share, that
        // something holds the socket and this build cannot talk to it.
        ProbeOutcome::Unintelligible(error) => {
            lines.push(format!(
                "server sees: a socket is live but this build could not read its reply: \
                 {error}"
            ));
            lines.push(OWNER_PID_UNKNOWN.to_string());
        }
        // A connection was accepted but nothing came back at all: there
        // is an owner, so naming some other directory as the reason
        // would be a guess. The foreign scan does not run here.
        ProbeOutcome::Busy => {
            lines.push(format!(
                "server sees: a socket answered nothing within {}ms; an owner may be busy",
                SOCKET_TIMEOUT.as_millis()
            ));
            lines.push(OWNER_PID_UNKNOWN.to_string());
        }
        ProbeOutcome::NoOwner => {
            let foreign = find_foreign_owner(identity, project_dir, prefix).await;
            lines.push(no_owner_line(foreign));
            lines.push("owner pid: none".to_string());
        }
    }

    lines.push(on_path_line(mcpls_on_path().as_deref()));

    lines.join("\n")
}

/// The prefix the production scan filters Windows pipe names by: exactly
/// what `identity_for` names its own pipes with, so the scan neither
/// misses a real owner nor matches some unrelated pipe by coincidence.
/// Unused on Unix, where the scan is already confined to the directory
/// `identity`'s own socket lives in.
#[cfg(windows)]
const fn foreign_scan_prefix() -> &'static str {
    mcpls_core::hooks::WINDOWS_PIPE_PREFIX
}

#[cfg(not(windows))]
const fn foreign_scan_prefix() -> &'static str {
    ""
}

/// The `server sees` line for how many `Changed`, `Flush`, or `EndSession`
/// requests the answering owner has served since it started.
///
/// A count of zero does not, on its own, mean a plugin was never wired up:
/// `SessionStart` never touches the socket by design, so a server that
/// started moments ago, or just took over from a previous owner, reads
/// exactly the same as one nobody ever registered. The doctor cannot tell
/// those apart, so it states the count and hands the reader the one thing
/// that resolves the ambiguity, rather than guessing at a cause.
fn hooks_seen_line(count: u64) -> String {
    if count == 0 {
        "hooks seen: none since this owner started; send a prompt or make an \
         edit in your Claude Code session for this project, then run the \
         doctor again; if it is still none after that, the plugin's hooks \
         are not reaching this server"
            .to_string()
    } else {
        format!("hooks seen: {count} request(s) since this owner started")
    }
}

/// What the runtime-directory scan found when this project's own socket
/// answered nobody.
enum ForeignOwners {
    /// The scan ran and no candidate identified itself as an owner.
    None {
        /// Whether the runtime location held more candidates than
        /// [`MAX_FOREIGN_CANDIDATES`] allowed this scan to examine. When
        /// true, "nothing is listening" covers only the candidates this
        /// scan actually reached.
        truncated: bool,
        /// How many candidates were live but unidentifiable: they
        /// accepted a connection and then either said nothing before the
        /// deadline or said something this build could not read. An mcpls
        /// speaking an older wire shape lands here, and reporting it as
        /// an absence is how the doctor would come to say nothing is
        /// running while something is.
        unidentified: usize,
    },
    /// An owner answered whose root is an ancestor or descendant of the
    /// project directory: the shape a server started one level up, or a
    /// `CLAUDE_PROJECT_DIR` pointing into a subdirectory, actually has.
    /// This is the one case the scan can name with evidence behind it.
    Related {
        root: PathBuf,
        pid: u32,
        /// How many candidates were live but unidentifiable, counted the
        /// same way as [`ForeignOwners::None::unidentified`]. Naming an
        /// owner does not end the scan, so this is the same total the
        /// other variants carry.
        unidentified: usize,
    },
    /// One or more owners answered, but none relate to this project's
    /// directory. Naming one of them would blame whichever happened to
    /// come first out of `read_dir`, an innocent project, for this
    /// project's own silence.
    Unrelated {
        /// How many unrelated owners answered among the candidates this
        /// scan actually examined.
        count: usize,
        /// Whether the runtime location held more candidates than
        /// [`MAX_FOREIGN_CANDIDATES`] allowed this scan to examine. When
        /// true, "none relate to this project" is not something the scan
        /// established for the candidates past its own limit.
        truncated: bool,
        /// How many candidates were live but unidentifiable, counted the
        /// same way as [`ForeignOwners::None::unidentified`]. One of them
        /// may well be this project's owner on an older wire shape.
        unidentified: usize,
    },
    /// The scan itself could not run, so nothing above is known one way
    /// or the other. Distinct from `None`, which is an answer the scan
    /// actually earned; this is the scan reporting that it never got to
    /// look.
    ScanFailed(String),
}

impl ForeignOwners {
    /// How many candidates were live but could not be identified.
    ///
    /// Every variant but `ScanFailed` carries a total over the candidates
    /// the scan set out to probe, because the scan runs to its limit
    /// whatever it finds along the way. `ScanFailed` never probed
    /// anything at all.
    const fn unidentified(&self) -> usize {
        match self {
            Self::None { unidentified, .. }
            | Self::Unrelated { unidentified, .. }
            | Self::Related { unidentified, .. } => *unidentified,
            Self::ScanFailed(_) => 0,
        }
    }
}

/// The `server sees` line to print when nothing answers this project's own
/// socket.
///
/// Candidates that were live but unidentifiable are reported as their own
/// clause rather than folded into the counts above it. They are evidence
/// that something is running, but not evidence of whose it is, and the
/// lines above only ever count owners that named their own root.
fn no_owner_line(foreign: ForeignOwners) -> String {
    let unidentified = foreign.unidentified();
    let line = match foreign {
        ForeignOwners::None {
            truncated: false, ..
        } => "server sees: no owner; nothing is listening on this project's socket".to_string(),
        ForeignOwners::None {
            truncated: true, ..
        } => format!(
            "server sees: no owner; checked {MAX_FOREIGN_CANDIDATES} other candidates and \
             none named an owner, but more may exist beyond the scan's limit"
        ),
        ForeignOwners::Related { root, pid, .. } => format!(
            "server sees: no owner for this directory; an mcpls is running for {} \
             (pid {pid}) instead",
            root.display()
        ),
        ForeignOwners::Unrelated {
            count: 1,
            truncated: false,
            ..
        } => "server sees: no owner for this directory; 1 other mcpls instance is \
             running, none for this directory or a parent of it"
            .to_string(),
        ForeignOwners::Unrelated {
            count,
            truncated: false,
            ..
        } => format!(
            "server sees: no owner for this directory; {count} other mcpls instances are \
             running, none for this directory or a parent of it"
        ),
        ForeignOwners::Unrelated {
            count,
            truncated: true,
            ..
        } => format!(
            "server sees: no owner for this directory; {count} other mcpls instances are \
             running, none for this directory or a parent of it, but more may exist \
             beyond the scan's limit"
        ),
        ForeignOwners::ScanFailed(reason) => format!(
            "server sees: no owner for this directory; could not scan for other mcpls \
             instances: {reason}"
        ),
    };
    match unidentified {
        0 => line,
        n => {
            let subject = if n == 1 {
                "other mcpls socket is"
            } else {
                "other mcpls sockets are"
            };
            format!(
                "{line}; {n} {subject} live but did not answer a status request this \
                 build could read"
            )
        }
    }
}

/// Look for an mcpls answering some other project's socket in the same
/// runtime location as `identity`'s own, so a doctor run against an
/// unreachable socket can tell "nothing is running" apart from
/// "something is running, for a directory that explains this one's
/// silence".
///
/// Candidates are probed in sorted order, and the candidate cap applies
/// to that order rather than to whatever the runtime location happened to
/// list. Two runs against an unchanged machine therefore print the same
/// answer, including which owner gets named when more than one relates.
///
/// Sockets are named by their own directory's hash, which is the entire
/// reason `identity`'s own probe above can never observe a live owner
/// whose directory hashed differently: that owner bound a different
/// socket file, not this one. This is the only way the doctor can learn
/// about it at all. An owner is only ever named when its root is an
/// ancestor or descendant of `project_dir`: anything else is a different
/// project entirely, and naming it would accuse it of a failure it has
/// nothing to do with.
async fn find_foreign_owner(
    identity: &SocketIdentity,
    project_dir: &Path,
    prefix: &str,
) -> ForeignOwners {
    let candidates = match foreign_candidates(identity, prefix) {
        Ok(candidates) => candidates,
        Err(error) => return ForeignOwners::ScanFailed(error.to_string()),
    };
    let truncated = candidates.len() > MAX_FOREIGN_CANDIDATES;
    let mut unrelated = 0usize;
    let mut unidentified = 0usize;
    // Naming an owner does not end the scan. Stopping at the first
    // related answer would leave the candidates after it unprobed, so the
    // unidentified count beside the name would be a floor while every
    // other variant's is a total, and which it was would depend on the
    // order the runtime directory happened to list its entries in. The
    // candidate cap is what bounds the wait.
    let mut related = Option::<(PathBuf, u32)>::None;
    for socket in candidates.into_iter().take(MAX_FOREIGN_CANDIDATES) {
        let candidate = SocketIdentity {
            socket,
            lock: PathBuf::new(),
            hash: String::new(),
        };
        match probe(&candidate, &Request::Status, SOCKET_TIMEOUT).await {
            ProbeOutcome::Answered(Response::Status {
                pid,
                root,
                owner: true,
                ..
            }) => {
                if root.starts_with(project_dir) || project_dir.starts_with(&root) {
                    related.get_or_insert((root, pid));
                } else {
                    unrelated += 1;
                }
            }
            // Accepted the connection and then either said nothing in
            // time or said something this build could not read. That is
            // still a process holding the socket, and an mcpls speaking
            // an older wire shape is the likeliest way to get here, so
            // counting it as an absence would report the one thing the
            // scan has evidence against.
            ProbeOutcome::Busy | ProbeOutcome::Unintelligible(_) => unidentified += 1,
            // A socket file with nothing behind it, and an answer that
            // parsed but claimed no ownership. Neither is evidence that
            // anything is running here.
            ProbeOutcome::NoOwner | ProbeOutcome::Answered(_) => {}
        }
    }
    if let Some((root, pid)) = related {
        ForeignOwners::Related {
            root,
            pid,
            unidentified,
        }
    } else if unrelated == 0 {
        ForeignOwners::None {
            truncated,
            unidentified,
        }
    } else {
        ForeignOwners::Unrelated {
            count: unrelated,
            truncated,
            unidentified,
        }
    }
}

/// The other sockets that might have an owner, alongside `identity`'s own
/// in the same runtime directory. `prefix` is unused: the directory
/// itself is already this scan's whole scope on Unix.
///
/// A missing runtime directory is not a failure: it means no mcpls has
/// ever run on this machine since it was last cleared, since the
/// directory is only created when an owner actually binds. Any other
/// error (permissions, most plausibly) is genuinely opaque and reported
/// as one.
#[cfg(not(windows))]
fn foreign_candidates(identity: &SocketIdentity, _prefix: &str) -> std::io::Result<Vec<PathBuf>> {
    let own = identity.socket.clone();
    let Some(dir) = identity.socket.parent() else {
        return Ok(Vec::new());
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| *path != own && path.extension().is_some_and(|ext| ext == "sock"))
        .collect();
    candidates.sort_unstable();
    Ok(candidates)
}

/// The other named pipes that might have an owner, read from the
/// system-wide pipe namespace and filtered to `prefix`, since Windows has
/// no per-project directory to list instead.
///
/// Unlike the Unix directory, this namespace always exists, so any
/// `read_dir` failure here is genuinely opaque rather than the clean
/// "nothing has ever run" answer a missing directory means on Unix.
#[cfg(windows)]
fn foreign_candidates(identity: &SocketIdentity, prefix: &str) -> std::io::Result<Vec<PathBuf>> {
    let own = identity.socket.clone();
    let entries = std::fs::read_dir(r"\\.\pipe\")?;
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            *path != own
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
        })
        .collect();
    candidates.sort_unstable();
    Ok(candidates)
}

/// The doctor's answer when this project's own socket identity cannot be
/// derived at all: an unreachable project directory, or a runtime
/// directory deep enough that the derived socket path exceeds this
/// platform's length limit. A running mcpls that hit the same failure
/// logs a warning and serves no socket rather than aborting startup, so
/// this state is real, not hypothetical.
///
/// Kept distinct from "server sees: no owner": that line means a socket
/// exists and nothing answers it; this means no socket could ever exist
/// here at all, on either side, which a user needs to be able to tell
/// apart from a server that simply is not running right now.
pub fn doctor_without_identity(project_dir: &Path, error: &mcpls_core::Error) -> String {
    let lines = [
        format!("socket: none; could not derive an identity for this directory: {error}"),
        format!("hook sees: {} -> unknown", project_dir.display()),
        "server sees: nothing can run here; no socket exists to probe".to_string(),
        "owner pid: none".to_string(),
        mcpls_on_path().map_or_else(
            || "mcpls on PATH: not found".to_string(),
            |path| format!("mcpls on PATH: {}", path.display()),
        ),
    ];
    lines.join("\n")
}

/// The absolute path to an executable named `mcpls` (`mcpls.exe` on
/// Windows) on the first `PATH` entry that has one, or `None`.
///
/// What an owner prints for its pid when it exists but did not say which
/// process it is. Distinct from `none`, which the doctor prints only when
/// nothing holds the socket at all: the two send a reader to different
/// places, and one token for both sends half of them to the wrong one.
const OWNER_PID_UNKNOWN: &str = "owner pid: unknown";

/// The `mcpls on PATH` line for `found`.
///
/// Split out of the doctor so both branches can be driven by a test.
/// The doctor's own call resolves against this process's real `PATH`, so
/// whether a suite run exercises the found branch, the missing one, or
/// only one of them is a property of the host rather than of the tests.
fn on_path_line(found: Option<&Path>) -> String {
    found.map_or_else(
        || "mcpls on PATH: not found".to_string(),
        |path| format!("mcpls on PATH: {}", path.display()),
    )
}

/// Hooks invoke `mcpls` by name off `PATH` rather than by an absolute
/// path, so a hook environment missing the install directory makes every
/// hook do nothing, invisibly. Walking `PATH` by hand rather than
/// shelling out to `which`, which is not installed on every host mcpls
/// runs on. `PATH` entries are not guaranteed absolute themselves (a bare
/// `bin`, `.`, or an empty entry from `::` all parse), so the winning
/// candidate is absolutized before it is returned: a relative path here
/// means nothing to whatever directory a hook later runs in.
fn mcpls_on_path() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) { "mcpls.exe" } else { "mcpls" };
    let path = std::env::var_os("PATH")?;
    resolve_on_path(&path, exe_name)
}

/// The absolute path to `exe_name` on the first entry of `path_var` (a
/// `PATH`-shaped value) that has an executable file by that name, or
/// `None`.
///
/// Split out of `mcpls_on_path` so a test can supply a `PATH` of its own
/// choosing rather than mutating this process's real environment, which a
/// multi-threaded test binary sharing one process cannot safely do.
fn resolve_on_path(path_var: &std::ffi::OsStr, exe_name: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(exe_name))
        .find(|candidate| is_executable_file(candidate))
        .map(|candidate| std::path::absolute(&candidate).unwrap_or(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    use super::*;

    /// The canned text a default `RecordingOwner` answers `Flush` with:
    /// multi-line, with a blank-free but indented second line, so a
    /// dispatcher that trims or re-wraps the owner's text before handing it
    /// to `additionalContext` fails the exact-value assertions below rather
    /// than passing on a fixture too flat to notice.
    const DEFAULT_FLUSH_TEXT: &str = "2 problems in a.rs\n  1 warning in b.rs\n";

    /// Run the dispatcher over one payload, against the socket identity
    /// `project_dir` derives, and return what it printed.
    ///
    /// Nothing is listening on that identity unless the test bound one,
    /// which is the point of most of these: the silent-failure rule says an
    /// unreachable socket prints nothing and exits zero.
    async fn dispatch(payload: &serde_json::Value, project_dir: &Path) -> String {
        dispatch_raw(&payload.to_string(), project_dir).await
    }

    /// The same, from raw stdin bytes, so a payload that is not JSON at all
    /// goes through the same path.
    async fn dispatch_raw(stdin: &str, project_dir: &Path) -> String {
        let identity = mcpls_core::hooks::identity_for(project_dir).expect("identity");
        super::dispatch_payload(stdin, project_dir, Some(&identity)).await
    }

    /// Run the dispatcher against a listener that records the requests it
    /// gets.
    async fn dispatch_against(payload: &serde_json::Value, recorder: &RecordingOwner) -> String {
        super::dispatch_payload(
            &payload.to_string(),
            recorder.project_dir(),
            Some(&recorder.identity),
        )
        .await
    }

    /// A `SocketIdentity` whose socket and lock live inside `dir`, built
    /// field by field rather than through `identity_for`, which would put
    /// the socket in the real runtime directory. `hash` and `lock` are
    /// never used by a `RecordingOwner`, which owns its socket directly
    /// rather than through `HookListener`'s lock file.
    fn temp_identity(dir: &Path) -> SocketIdentity {
        let suffix = format!("owner-{}", rand_suffix());
        #[cfg(windows)]
        let socket = PathBuf::from(format!(r"\\.\pipe\mcpls-recording-{suffix}"));
        #[cfg(not(windows))]
        let socket = dir.join(format!("{suffix}.sock"));
        SocketIdentity {
            lock: dir.join(format!("{suffix}.lock")),
            socket,
            hash: suffix,
        }
    }

    /// A per-call suffix, so two tests running in parallel never collide on
    /// a Windows pipe name, which is process-global rather than
    /// directory-scoped.
    fn rand_suffix() -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::thread::current().id().hash(&mut hasher);
        std::time::SystemTime::now().hash(&mut hasher);
        hasher.finish()
    }

    /// The op name for a `Request`, e.g. "changed" or "flush", matching the
    /// wire's own `op` tag.
    fn op_name(request: &Request) -> String {
        match request {
            Request::Changed { .. } => "changed",
            Request::Flush { .. } => "flush",
            Request::EndSession { .. } => "end_session",
            Request::Status => "status",
        }
        .to_string()
    }

    /// How `RecordingOwner` answers requests: the `Flush` context text, how
    /// long to wait before answering one, and whether `Changed` answers an
    /// error instead of queuing. The delay defaults to zero and the error
    /// defaults to off; a test exercising either sets it explicitly, so a
    /// regression that narrows a client's tolerance has something in the
    /// suite that would notice.
    #[derive(Clone)]
    struct OwnerBehavior {
        flush_text: Option<String>,
        flush_delay: Duration,
        changed_errors: bool,
        /// What `Status` reports as its directory hash, startup root, and
        /// hook-request count. Empty/zero by default, which is fine for
        /// every test that never sends `Status` against this behavior.
        status_hash: String,
        status_root: PathBuf,
        status_hooks_seen: u64,
        /// Whether `Status` answers as the socket's owner. `true` by
        /// default, since only an owner ever answers in every other test;
        /// one test sets this to `false` to prove the foreign-owner scan
        /// ignores an answer that says it is not the owner, the way a
        /// future forwarding proxy would.
        status_owner: bool,
        /// Whether a connection is accepted and then held open without
        /// ever being read or answered, standing in for a real owner that
        /// is busy past the client's deadline. `false` by default; one
        /// test sets it to prove the doctor tells this apart from nobody
        /// being there at all.
        silent: bool,
        /// When set, `Status` answers with `Response::Error { message }`
        /// instead of its usual `Response::Status`, the shape an owner
        /// uses to explain a request it could not satisfy. `None` by
        /// default.
        status_error: Option<String>,
        /// When set, `Status` answers with this exact JSON line instead
        /// of going through `Response`'s own serialization at all,
        /// standing in for a wire shape this build's `Response` cannot
        /// represent (a previous version missing a field this build now
        /// requires). `None` by default.
        status_raw_line: Option<String>,
    }

    impl Default for OwnerBehavior {
        fn default() -> Self {
            Self {
                flush_text: None,
                flush_delay: Duration::ZERO,
                changed_errors: false,
                status_hash: String::new(),
                status_root: PathBuf::new(),
                status_hooks_seen: 0,
                status_owner: true,
                silent: false,
                status_error: None,
                status_raw_line: None,
            }
        }
    }

    /// A plausible answer to `request` under `behavior`.
    ///
    /// A real owner answers `Changed` with `Response::Error` once its
    /// sweeper has stopped (ordinarily: shutdown) rather than silently
    /// dropping the path, which `behavior.changed_errors` reproduces here.
    fn answer(request: &Request, behavior: &OwnerBehavior) -> Response {
        match request {
            Request::Changed { paths, .. } => {
                if behavior.changed_errors {
                    Response::Error {
                        message: "mcpls is shutting down; these paths were not queued".to_string(),
                    }
                } else {
                    Response::Changed {
                        queued: paths.len(),
                    }
                }
            }
            Request::Flush { .. } => Response::Flush {
                context: behavior.flush_text.clone(),
            },
            Request::EndSession { .. } => Response::EndSession,
            Request::Status => behavior.status_error.as_ref().map_or_else(
                || Response::Status {
                    hash: behavior.status_hash.clone(),
                    socket: PathBuf::new(),
                    pid: std::process::id(),
                    owner: behavior.status_owner,
                    root: behavior.status_root.clone(),
                    hooks_seen: behavior.status_hooks_seen,
                },
                |message| Response::Error {
                    message: message.clone(),
                },
            ),
        }
    }

    /// A listener on a temporary socket that records every request it is
    /// sent, in order, and answers each plausibly.
    ///
    /// A hand-rolled newline-delimited JSON accept loop, not
    /// `HookListener`: the protocol's framing is pinned by
    /// `mcpls_core::hooks::protocol`'s own tests, so nothing here depends
    /// on `HookListener`'s specific accept/serve machinery, and speaking
    /// the wire format directly means one socket yields the full `Request`
    /// (paths, session ids, event kinds) and a true connection count from
    /// its own accept counter, rather than needing a second socket in
    /// front of a `HookListener` whose handler cannot see connection
    /// boundaries at all.
    struct RecordingOwner {
        dir: tempfile::TempDir,
        identity: SocketIdentity,
        requests: Arc<Mutex<Vec<Request>>>,
        connections: Arc<AtomicUsize>,
        _cancel: tokio::sync::watch::Sender<bool>,
    }

    impl RecordingOwner {
        /// Bind and start serving, answering every `Flush` with
        /// [`DEFAULT_FLUSH_TEXT`] immediately and every `Changed` with
        /// `Response::Changed`.
        fn start() -> Self {
            Self::start_with_flush(Some(DEFAULT_FLUSH_TEXT.to_string()))
        }

        /// The same, answering every `Flush` with `flush_text` instead.
        fn start_with_flush(flush_text: Option<String>) -> Self {
            Self::start_with(OwnerBehavior {
                flush_text,
                ..OwnerBehavior::default()
            })
        }

        /// The same, waiting `delay` before answering each `Flush`. Used to
        /// prove a client actually waits out an owner slower than an
        /// instant in-process reply, rather than merely using whichever
        /// timeout constant happens to be in scope at its call site.
        fn start_with_flush_delay(flush_text: Option<String>, delay: Duration) -> Self {
            Self::start_with(OwnerBehavior {
                flush_text,
                flush_delay: delay,
                ..OwnerBehavior::default()
            })
        }

        /// The same, answering every `Changed` with `Response::Error`
        /// instead of `Response::Changed`, the shape a real owner sends
        /// once its sweeper has stopped taking new paths (ordinarily:
        /// shutdown). `Flush` still answers `flush_text` normally, so a
        /// `PostToolBatch`'s `changed` erroring must not be allowed to
        /// swallow its own `flush`'s context.
        fn start_with_changed_error(flush_text: Option<String>) -> Self {
            Self::start_with(OwnerBehavior {
                flush_text,
                changed_errors: true,
                ..OwnerBehavior::default()
            })
        }

        /// Not `async`: everything here is a synchronous bind or a plain
        /// `tokio::spawn` call, which needs a runtime running (true of
        /// every `#[tokio::test]` caller) but not an `async` caller.
        fn start_with(behavior: OwnerBehavior) -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            let identity = temp_identity(dir.path());
            Self::start_on(dir, identity, behavior)
        }

        /// The same, bound on `identity` rather than one generated fresh.
        ///
        /// Lets a caller start an owner on the exact socket some other,
        /// independently derived identity names, which `mcpls hook
        /// doctor`'s tests need: the socket the doctor connects to has to
        /// be the one it computed for a project directory, not one this
        /// harness invented.
        fn start_on(
            dir: tempfile::TempDir,
            identity: SocketIdentity,
            behavior: OwnerBehavior,
        ) -> Self {
            let requests: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
            let connections = Arc::new(AtomicUsize::new(0));
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

            // Bound synchronously, before this function returns: a spawned
            // task's first poll is not guaranteed to happen before the
            // caller's next `send`/`send_many`, and a socket that does not
            // exist yet would refuse that connection outright.
            let listener = bind_owner(&identity.socket);
            tokio::spawn(accept_loop(
                listener,
                identity.socket.clone(),
                Arc::clone(&requests),
                Arc::clone(&connections),
                behavior,
                cancel_rx,
            ));

            Self {
                dir,
                identity,
                requests,
                connections,
                _cancel: cancel_tx,
            }
        }

        /// An owner bound on `identity`, answering `Status` as though its
        /// own startup directory were `root` and it had already answered
        /// `hooks_seen` hook requests, rather than wherever this process
        /// actually runs and however many requests this test harness has
        /// actually served.
        ///
        /// Uses `identity_hash` rather than `identity_for`, which also
        /// builds and length-checks a real socket path this call never
        /// uses: a caller on a host with a long runtime directory would
        /// otherwise panic here before the test bound anything.
        fn start_reporting_status(identity: SocketIdentity, root: &Path, hooks_seen: u64) -> Self {
            let hash = mcpls_core::hooks::identity_hash(root)
                .expect("identity hash for the reported root");
            let dir = tempfile::tempdir().expect("a temp dir");
            Self::start_on(
                dir,
                identity,
                OwnerBehavior {
                    status_hash: hash,
                    status_root: root.to_path_buf(),
                    status_hooks_seen: hooks_seen,
                    ..OwnerBehavior::default()
                },
            )
        }

        /// An owner bound on `identity`, answering `Status` with `owner:
        /// false`, the shape a forwarding proxy would answer with rather
        /// than an actual owner.
        fn start_reporting_non_owner(identity: SocketIdentity, root: &Path) -> Self {
            let hash = mcpls_core::hooks::identity_hash(root)
                .expect("identity hash for the reported root");
            let dir = tempfile::tempdir().expect("a temp dir");
            Self::start_on(
                dir,
                identity,
                OwnerBehavior {
                    status_hash: hash,
                    status_root: root.to_path_buf(),
                    status_owner: false,
                    ..OwnerBehavior::default()
                },
            )
        }

        /// An owner bound on `identity` that accepts a connection and then
        /// never reads or answers it, standing in for a real owner too
        /// busy to get back to the client within its deadline.
        fn start_silent(identity: SocketIdentity) -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            Self::start_on(
                dir,
                identity,
                OwnerBehavior {
                    silent: true,
                    ..OwnerBehavior::default()
                },
            )
        }

        /// An owner bound on `identity` that answers `Status` with a
        /// well-formed `Response::Error` instead of its usual
        /// `Response::Status`, the shape an owner uses to explain a
        /// request it deliberately could not satisfy.
        fn start_answering_status_with_error(identity: SocketIdentity, message: &str) -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            Self::start_on(
                dir,
                identity,
                OwnerBehavior {
                    status_error: Some(message.to_string()),
                    ..OwnerBehavior::default()
                },
            )
        }

        /// An owner bound on `identity` that answers `Status` with a raw
        /// JSON line this build's own `Response` cannot represent,
        /// standing in for a previous version's wire shape missing a
        /// field this build now requires.
        fn start_answering_status_with_raw_line(identity: SocketIdentity, raw_line: &str) -> Self {
            let dir = tempfile::tempdir().expect("a temp dir");
            Self::start_on(
                dir,
                identity,
                OwnerBehavior {
                    status_raw_line: Some(raw_line.to_string()),
                    ..OwnerBehavior::default()
                },
            )
        }

        /// The directory the dispatcher treats as `CLAUDE_PROJECT_DIR`.
        fn project_dir(&self) -> &Path {
            self.dir.path()
        }

        /// The requests received, in order, in full.
        fn requests(&self) -> Vec<Request> {
            self.requests.lock().expect("requests lock").clone()
        }

        /// The op names received, in order: "changed", "flush", and so on.
        fn ops(&self) -> Vec<String> {
            self.requests().iter().map(op_name).collect()
        }

        /// How many client connections the socket has accepted.
        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    /// Read newline-delimited requests off `stream` until it closes,
    /// recording each and answering it before reading the next. Generic
    /// over the stream type so the same loop body serves both a Unix
    /// socket and a Windows named pipe.
    async fn serve_connection<S>(
        stream: S,
        requests: Arc<Mutex<Vec<Request>>>,
        behavior: OwnerBehavior,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        if behavior.silent {
            // Accepted, and then never read from or written to: a real
            // connection with a real owner on the other end of it, who
            // simply never gets back to the client. `stream` stays open
            // for as long as this future is polled, which is exactly as
            // long as the test that spawned it keeps its runtime alive.
            std::future::pending::<()>().await;
        }

        let (reader, mut writer) = tokio::io::split(stream);
        let mut lines = tokio::io::BufReader::new(reader).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(request) = serde_json::from_str::<Request>(&line) else {
                return;
            };
            if matches!(request, Request::Flush { .. }) && !behavior.flush_delay.is_zero() {
                tokio::time::sleep(behavior.flush_delay).await;
            }

            // A raw line bypasses `Response`'s own serialization entirely,
            // for a test standing in for a wire shape this build's
            // `Response` cannot represent at all (a previous version
            // missing a field this build now requires).
            let mut out = if matches!(request, Request::Status)
                && let Some(raw) = &behavior.status_raw_line
            {
                raw.clone()
            } else {
                let response = answer(&request, &behavior);
                let Ok(serialized) = serde_json::to_string(&response) else {
                    return;
                };
                serialized
            };
            requests.lock().expect("requests lock").push(request);

            out.push('\n');
            if writer.write_all(out.as_bytes()).await.is_err() {
                return;
            }
            if writer.flush().await.is_err() {
                return;
            }
        }
    }

    /// Bind the owner's socket synchronously.
    #[cfg(not(windows))]
    fn bind_owner(socket: &Path) -> tokio::net::UnixListener {
        tokio::net::UnixListener::bind(socket).expect("bind the recording owner")
    }

    /// Accept every client on `listener`, counting each one and serving it.
    /// `_socket` is unused here: a `UnixListener` never needs to recreate
    /// itself between clients the way a named pipe does, but the parameter
    /// is kept so both platforms share one call site in
    /// `RecordingOwner::start_with`.
    #[cfg(not(windows))]
    async fn accept_loop(
        listener: tokio::net::UnixListener,
        _socket: PathBuf,
        requests: Arc<Mutex<Vec<Request>>>,
        connections: Arc<AtomicUsize>,
        behavior: OwnerBehavior,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                result = cancel.changed() => {
                    if result.is_err() || *cancel.borrow() {
                        return;
                    }
                }
                accepted = listener.accept() => {
                    let Ok((stream, _addr)) = accepted else {
                        return;
                    };
                    connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve_connection(
                        stream,
                        Arc::clone(&requests),
                        behavior.clone(),
                    ));
                }
            }
        }
    }

    /// Create the owner's first pipe instance synchronously.
    #[cfg(windows)]
    fn bind_owner(socket: &Path) -> tokio::net::windows::named_pipe::NamedPipeServer {
        tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(socket)
            .expect("create the recording owner pipe")
    }

    /// Accept every client on `server`'s pipe, counting each one and
    /// serving it. `socket` is the pipe path `server` was created on, kept
    /// alongside it so each accepted client's replacement instance can be
    /// created on the same name.
    #[cfg(windows)]
    async fn accept_loop(
        mut server: tokio::net::windows::named_pipe::NamedPipeServer,
        socket: PathBuf,
        requests: Arc<Mutex<Vec<Request>>>,
        connections: Arc<AtomicUsize>,
        behavior: OwnerBehavior,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) {
        use tokio::net::windows::named_pipe::ServerOptions;
        loop {
            tokio::select! {
                result = cancel.changed() => {
                    if result.is_err() || *cancel.borrow() {
                        return;
                    }
                }
                connected = server.connect() => {
                    if connected.is_err() {
                        return;
                    }
                    // The replacement instance is created *before* the
                    // connected one is moved out and handed off, mirroring
                    // `crates/mcpls-core/src/hooks/listener.rs`'s
                    // `PipeTransport::accept`: a client arriving between
                    // this instance no longer listening and a replacement
                    // existing would get `ERROR_FILE_NOT_FOUND` rather than
                    // the `ERROR_PIPE_BUSY` a real client's connect loop
                    // retries on.
                    let Ok(next) = ServerOptions::new().create(&socket) else {
                        return;
                    };
                    let current = std::mem::replace(&mut server, next);
                    connections.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve_connection(
                        current,
                        Arc::clone(&requests),
                        behavior.clone(),
                    ));
                }
            }
        }
    }

    #[test]
    fn test_the_flush_timeout_tracks_the_op_deadline_default() {
        assert_eq!(
            FLUSH_SOCKET_TIMEOUT,
            Duration::from_millis(1500),
            "a client timeout tighter than the server's own op_deadline_ms \
             default is never right; if the spec's default changes, this \
             constant must change deliberately alongside it"
        );
    }

    /// Well past the old, uniform 50ms bound and well inside
    /// `FLUSH_SOCKET_TIMEOUT`'s 1500ms: a ~15x margin under the timeout
    /// budget and a 2x margin over the bound a flush-bearing op must no
    /// longer be held to, so this is not a flake risk the way a
    /// tight-margin timing test would be.
    const SLOW_FLUSH_DELAY: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn test_a_missing_identity_still_lets_session_start_answer() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");

        let out = super::dispatch_payload(
            &json!({ "hook_event_name": "SessionStart" }).to_string(),
            dir.path(),
            None,
        )
        .await;

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert!(
            !parsed["hookSpecificOutput"]["watchPaths"]
                .as_array()
                .expect("watchPaths")
                .is_empty(),
            "SessionStart never touches the socket, so a missing identity \
             (an unreachable or over-long project directory) must not \
             suppress it: {out}"
        );
    }

    #[tokio::test]
    async fn test_a_missing_identity_produces_no_output_for_socket_using_arms() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let out = super::dispatch_payload(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }).to_string(),
            dir.path(),
            None,
        )
        .await;
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn test_user_prompt_submit_waits_out_a_slow_owner_within_the_flush_timeout() {
        let recorder = RecordingOwner::start_with_flush_delay(
            Some(DEFAULT_FLUSH_TEXT.to_string()),
            SLOW_FLUSH_DELAY,
        );
        let out = dispatch_against(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            &recorder,
        )
        .await;

        assert_eq!(
            out,
            additional_context_output(Some(DEFAULT_FLUSH_TEXT.to_string())),
            "an owner slower than the old 50ms bound but inside \
             FLUSH_SOCKET_TIMEOUT must still be waited out, or raising the \
             timeout had no effect at this call site"
        );
    }

    #[tokio::test]
    async fn test_post_tool_batch_waits_out_a_slow_owner_within_the_flush_timeout() {
        let recorder = RecordingOwner::start_with_flush_delay(
            Some(DEFAULT_FLUSH_TEXT.to_string()),
            SLOW_FLUSH_DELAY,
        );
        let file = recorder.project_dir().join("a.rs");
        std::fs::write(&file, "fn a() {}").expect("write");
        let out = dispatch_against(
            &json!({
                "hook_event_name": "PostToolBatch",
                "session_id": "s1",
                "tool_calls": [{ "tool_input": { "file_path": file.display().to_string() } }]
            }),
            &recorder,
        )
        .await;

        assert_eq!(
            out,
            additional_context_output(Some(DEFAULT_FLUSH_TEXT.to_string())),
            "an owner slower than the old 50ms bound but inside \
             FLUSH_SOCKET_TIMEOUT must still be waited out, or raising the \
             timeout had no effect at this call site"
        );
    }

    #[tokio::test]
    async fn test_session_start_returns_watch_paths_without_a_socket() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");

        let out = dispatch(&json!({ "hook_event_name": "SessionStart" }), dir.path()).await;

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        let paths = parsed["hookSpecificOutput"]["watchPaths"]
            .as_array()
            .expect("watchPaths");
        assert!(!paths.is_empty());
        for path in paths {
            let path = path.as_str().expect("a string path");
            assert!(
                Path::new(path).starts_with(dir.path()),
                "SessionStart must watch the project directory it was given, \
                 not some other directory: {path}"
            );
        }
        // No listener is bound in this test at all.
        assert!(
            !out.contains("error"),
            "SessionStart fires while the host is still spawning the MCP server, \
             so asking a socket that is not bound yet would leave the session \
             with either no coverage or an unbounded watcher over target/"
        );
    }

    #[tokio::test]
    async fn test_an_unreachable_socket_produces_no_output_and_exit_zero() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let out = dispatch(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            dir.path(),
        )
        .await;
        assert_eq!(
            out, "",
            "an edit must never fail because diagnostics were unavailable"
        );
    }

    #[tokio::test]
    async fn test_an_unknown_event_produces_no_output() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let out = dispatch(&json!({ "hook_event_name": "Whatever" }), dir.path()).await;
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn test_a_malformed_payload_produces_no_output() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let out = dispatch_raw("not json at all", dir.path()).await;
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn test_file_changed_sends_a_changed_request_with_the_path_and_event() {
        let recorder = RecordingOwner::start();
        let file = recorder.project_dir().join("a.rs");
        let out = dispatch_against(
            &json!({
                "hook_event_name": "FileChanged",
                "session_id": "s1",
                "file_path": file.display().to_string(),
                "event": "add"
            }),
            &recorder,
        )
        .await;

        assert_eq!(out, "", "FileChanged never injects context");
        let requests = recorder.requests();
        assert_eq!(requests.len(), 1);
        let Request::Changed {
            session,
            paths,
            event,
        } = &requests[0]
        else {
            panic!("expected a changed request: {:?}", requests[0]);
        };
        assert_eq!(session.as_str(), "s1");
        assert_eq!(*paths, vec![file]);
        assert_eq!(*event, ChangeEvent::Add);
    }

    #[tokio::test]
    async fn test_file_changed_without_an_event_defaults_to_change() {
        let recorder = RecordingOwner::start();
        let file = recorder.project_dir().join("a.rs");
        dispatch_against(
            &json!({
                "hook_event_name": "FileChanged",
                "session_id": "s1",
                "file_path": file.display().to_string()
            }),
            &recorder,
        )
        .await;

        let requests = recorder.requests();
        assert_eq!(requests.len(), 1);
        let Request::Changed { event, .. } = &requests[0] else {
            panic!("expected a changed request: {:?}", requests[0]);
        };
        assert_eq!(
            *event,
            ChangeEvent::Change,
            "an event kind the host omits must default to Change, not some \
             other kind that would mislead the sweep's hint"
        );
    }

    #[tokio::test]
    async fn test_file_changed_without_a_file_path_sends_nothing() {
        let recorder = RecordingOwner::start();
        let out = dispatch_against(
            &json!({ "hook_event_name": "FileChanged", "session_id": "s1" }),
            &recorder,
        )
        .await;
        assert_eq!(out, "");
        assert_eq!(
            recorder.ops(),
            Vec::<String>::new(),
            "a FileChanged payload with no file_path must not open a connection at all"
        );
    }

    #[tokio::test]
    async fn test_session_end_sends_an_end_session_request() {
        let recorder = RecordingOwner::start();
        let out = dispatch_against(
            &json!({ "hook_event_name": "SessionEnd", "session_id": "s1" }),
            &recorder,
        )
        .await;
        assert_eq!(out, "");
        let requests = recorder.requests();
        assert_eq!(requests.len(), 1);
        let Request::EndSession { session } = &requests[0] else {
            panic!("expected an end_session request: {:?}", requests[0]);
        };
        assert_eq!(session.as_str(), "s1");
    }

    #[tokio::test]
    async fn test_user_prompt_submit_flushes_and_injects_context() {
        let recorder = RecordingOwner::start();
        let out = dispatch_against(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            &recorder,
        )
        .await;

        let requests = recorder.requests();
        assert_eq!(requests.len(), 1);
        let Request::Flush { session } = &requests[0] else {
            panic!("expected a flush request: {:?}", requests[0]);
        };
        assert_eq!(session.as_str(), "s1");

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            json!(DEFAULT_FLUSH_TEXT)
        );
    }

    #[tokio::test]
    async fn test_an_empty_flush_context_injects_nothing() {
        let recorder = RecordingOwner::start_with_flush(Some(String::new()));
        let out = dispatch_against(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            &recorder,
        )
        .await;
        assert_eq!(
            out, "",
            "an owner reporting an empty flush must not produce an \
             additionalContext key at all"
        );
    }

    #[tokio::test]
    async fn test_stop_sends_nothing() {
        let recorder = RecordingOwner::start();
        let out = dispatch_against(
            &json!({ "hook_event_name": "Stop", "session_id": "s1" }),
            &recorder,
        )
        .await;
        assert_eq!(out, "");
        assert_eq!(
            recorder.ops(),
            Vec::<String>::new(),
            "Stop's additionalContext continues the conversation, so flushing \
             there would turn every new warning into a keep-working signal"
        );
    }

    #[tokio::test]
    async fn test_post_tool_batch_sends_changed_then_flush() {
        let recorder = RecordingOwner::start();
        let file = recorder.project_dir().join("a.rs");
        std::fs::write(&file, "fn a() {}").expect("write");
        let out = dispatch_against(
            &json!({
                "hook_event_name": "PostToolBatch",
                "session_id": "s1",
                "tool_calls": [{ "tool_input": { "file_path": file.display().to_string() } }]
            }),
            &recorder,
        )
        .await;

        let requests = recorder.requests();
        assert_eq!(
            requests.len(),
            2,
            "expected a changed request followed by a flush request: {requests:?}"
        );
        let Request::Changed {
            session: changed_session,
            paths,
            event,
        } = &requests[0]
        else {
            panic!(
                "expected the first request to be changed: {:?}",
                requests[0]
            );
        };
        let Request::Flush {
            session: flush_session,
        } = &requests[1]
        else {
            panic!("expected the second request to be flush: {:?}", requests[1]);
        };
        assert_eq!(changed_session.as_str(), "s1");
        assert_eq!(
            *paths,
            vec![file],
            "the batch's own path must reach the changed request"
        );
        assert_eq!(*event, ChangeEvent::Change);
        assert_eq!(flush_session.as_str(), "s1");

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            json!(DEFAULT_FLUSH_TEXT),
            "the owner's flush text must reach additionalContext unmodified, \
             not re-rendered, wrapped, or trimmed"
        );
        assert_eq!(
            recorder.connections(),
            1,
            "the spec's protocol says PostToolBatch sends changed then flush on \
             one connection, which is what send_many is for"
        );
    }

    /// A real owner answers `Changed` with `Response::Error` once its
    /// sweeper has stopped taking new paths (ordinarily: shutdown), rather
    /// than silently dropping the path. `PostToolBatch` reads `send_many`'s
    /// responses by position, not by matching the `changed` half's
    /// variant, so an error there must not prevent the `flush` half's
    /// context, arriving over the same connection, from reaching
    /// `additionalContext`.
    #[tokio::test]
    async fn test_a_changed_error_does_not_swallow_the_batch_s_flush() {
        let recorder =
            RecordingOwner::start_with_changed_error(Some(DEFAULT_FLUSH_TEXT.to_string()));
        let file = recorder.project_dir().join("a.rs");
        std::fs::write(&file, "fn a() {}").expect("write");
        let out = dispatch_against(
            &json!({
                "hook_event_name": "PostToolBatch",
                "session_id": "s1",
                "tool_calls": [{ "tool_input": { "file_path": file.display().to_string() } }]
            }),
            &recorder,
        )
        .await;

        assert_eq!(
            out,
            additional_context_output(Some(DEFAULT_FLUSH_TEXT.to_string())),
            "an error answering changed must not swallow a flush answer \
             that arrived on the same connection: {out}"
        );
    }

    /// The prefix these tests' own pipes carry on Windows, distinct from
    /// `mcpls_core::hooks::WINDOWS_PIPE_PREFIX`: the pipe namespace is
    /// machine-global, so a scan filtered on the real prefix would
    /// enumerate an actual mcpls running on the developer's own machine,
    /// not only the one a test bound itself. Every doctor call in this
    /// module goes through `doctor_scanning` with this prefix rather than
    /// the public `doctor`, so the scan can never see past this suite's
    /// own pipes on Windows.
    #[cfg(windows)]
    const TEST_PIPE_PREFIX: &str = "mcpls-doctor-test-";
    #[cfg(not(windows))]
    const TEST_PIPE_PREFIX: &str = "";

    /// The identity `identity_for(project)` would derive, with its socket
    /// and lock moved into `dir` in place of the real runtime directory
    /// `identity_for` would otherwise choose.
    ///
    /// Uses `identity_hash` rather than `identity_for`, which also builds
    /// and length-checks a real socket path this function never uses: on
    /// a host with a long runtime directory (the macOS/CI condition
    /// `doctor_without_identity` exists to degrade gracefully for),
    /// `identity_for` fails outright, and every one of these tests would
    /// panic before binding anything.
    fn local_identity_for(project: &Path, dir: &Path) -> SocketIdentity {
        let hash = mcpls_core::hooks::identity_hash(project).expect("identity hash");
        #[cfg(windows)]
        let socket = PathBuf::from(format!(r"\\.\pipe\{TEST_PIPE_PREFIX}{hash}"));
        #[cfg(not(windows))]
        let socket = dir.join(format!("{hash}.sock"));
        SocketIdentity {
            lock: dir.join(format!("{hash}.lock")),
            socket,
            hash,
        }
    }

    /// Run the doctor for `project` against a real owner bound on
    /// `project`'s own socket, answering as though it had itself started
    /// in `project` and already served `hooks_seen` hook requests.
    ///
    /// Returns the identity too, so a test can assert the `socket:` line
    /// against the exact path this call bound, not merely that the label
    /// showed up.
    async fn doctor_with_own_owner(project: &Path, hooks_seen: u64) -> (String, SocketIdentity) {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let _owner = RecordingOwner::start_reporting_status(identity.clone(), project, hooks_seen);
        let out = super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await;
        (out, identity)
    }

    /// Run the doctor for `project` with nothing bound anywhere it looks.
    async fn doctor_with_nothing_running(project: &Path) -> (String, SocketIdentity) {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let out = super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await;
        (out, identity)
    }

    /// Run the doctor for `project` where nothing owns `project`'s own
    /// socket, but a real owner is bound for `foreign` in the same
    /// runtime directory the doctor scans.
    ///
    /// This is how a server started in the wrong working directory
    /// actually shows up: a different canonical directory hashes to a
    /// different socket *file*, never to a different hash answering the
    /// same one, since the socket's name is that hash.
    async fn doctor_with_foreign_owner(project: &Path, foreign: &Path) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let foreign_identity = local_identity_for(foreign, socket_dir.path());
        let _owner = RecordingOwner::start_reporting_status(foreign_identity, foreign, 0);
        super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await
    }

    /// Run the doctor for `project` against several real, unrelated
    /// owners at once, none of which relate to `project`'s own
    /// directory.
    async fn doctor_with_unrelated_owners(project: &Path, others: &[&Path]) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let _owners: Vec<_> = others
            .iter()
            .map(|other| {
                let other_identity = local_identity_for(other, socket_dir.path());
                RecordingOwner::start_reporting_status(other_identity, other, 0)
            })
            .collect();
        super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await
    }

    /// Run the doctor for `project` where a real owner is running for
    /// `related` (an ancestor or descendant of `project`) alongside
    /// `unreadable` other owners answering a wire shape this build cannot
    /// read.
    ///
    /// The socket names are chosen so the related owner sorts first among
    /// the candidates. The scan probes in sorted order, so a scan that
    /// stopped at the first related answer would reach none of the
    /// unreadable ones, and the count beside the name would be zero.
    async fn doctor_with_related_owner_among_unreadable_ones(
        project: &Path,
        related: &Path,
        unreadable: usize,
    ) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let related_identity = SocketIdentity {
            socket: socket_dir.path().join("aaa-related.sock"),
            ..local_identity_for(related, socket_dir.path())
        };
        let _related = RecordingOwner::start_reporting_status(related_identity, related, 0);
        let others: Vec<_> = (0..unreadable)
            .map(|_| tempfile::tempdir().expect("a temp dir"))
            .collect();
        let _owners: Vec<_> = others
            .iter()
            .enumerate()
            .map(|(i, other)| {
                let other_identity = SocketIdentity {
                    socket: socket_dir.path().join(format!("zzz-unreadable-{i}.sock")),
                    ..local_identity_for(other.path(), socket_dir.path())
                };
                RecordingOwner::start_answering_status_with_raw_line(
                    other_identity,
                    r#"{"op":"status","hash":"abc","socket":"x","pid":1,"owner":true}"#,
                )
            })
            .collect();
        super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await
    }

    /// Run the doctor for `project` where the runtime directory holds
    /// `count` other sockets, each with a live owner answering `Status`
    /// with a wire shape this build cannot read. Stands in for other
    /// mcpls processes running a previous protocol version.
    async fn doctor_with_unreadable_foreign_owners(project: &Path, count: usize) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let others: Vec<_> = (0..count)
            .map(|_| tempfile::tempdir().expect("a temp dir"))
            .collect();
        let _owners: Vec<_> = others
            .iter()
            .map(|other| {
                let other_identity = local_identity_for(other.path(), socket_dir.path());
                RecordingOwner::start_answering_status_with_raw_line(
                    other_identity,
                    r#"{"op":"status","hash":"abc","socket":"x","pid":1,"owner":true}"#,
                )
            })
            .collect();
        super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await
    }

    /// Run the doctor for `project` where the only reachable socket
    /// answers `Status` with `owner: false`, as a forwarding proxy would.
    async fn doctor_with_non_owner(project: &Path, elsewhere: &Path) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let elsewhere_identity = local_identity_for(elsewhere, socket_dir.path());
        let _owner = RecordingOwner::start_reporting_non_owner(elsewhere_identity, elsewhere);
        super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await
    }

    /// Run the doctor for `project` against a real owner that accepts the
    /// connection and then never answers, standing in for one busy past
    /// the deadline.
    async fn doctor_with_busy_owner(project: &Path) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let _owner = RecordingOwner::start_silent(identity.clone());
        super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await
    }

    /// The same, with a second, real, related owner also present in the
    /// same runtime directory: proves the foreign scan does not run at
    /// all when this project's own socket is merely busy, rather than
    /// running it and happening not to name the related owner.
    async fn doctor_with_busy_owner_and_related_owner(project: &Path, related: &Path) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let _busy = RecordingOwner::start_silent(identity.clone());
        let related_identity = local_identity_for(related, socket_dir.path());
        let _related = RecordingOwner::start_reporting_status(related_identity, related, 0);
        super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await
    }

    /// Run the doctor for `project` against a real owner that answers
    /// `Status` with a well-formed `Response::Error`.
    async fn doctor_with_owner_answering_error(project: &Path, message: &str) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let _owner = RecordingOwner::start_answering_status_with_error(identity.clone(), message);
        super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await
    }

    /// Run the doctor for `project` against a real owner that answers
    /// `Status` with a raw line this build's `Response` cannot parse.
    async fn doctor_with_owner_answering_raw_status(project: &Path, raw_line: &str) -> String {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project, socket_dir.path());
        let _owner =
            RecordingOwner::start_answering_status_with_raw_line(identity.clone(), raw_line);
        super::doctor_scanning(project, &identity, TEST_PIPE_PREFIX).await
    }

    /// Every line asserted by its exact text and position, not merely by
    /// label, and the line count pinned too: a deleted line, a bare label
    /// with its payload dropped, or a value swapped for a look-alike (the
    /// requesting hash for the owner's own, `project_dir` for the
    /// owner's `root`) must all fail this, which `out.contains("server
    /// sees")`-style checks would not have caught.
    #[tokio::test]
    async fn test_doctor_reports_the_live_owners_root_pid_and_hook_activity() {
        let project = tempfile::tempdir().expect("a temp dir");

        let (out, identity) = doctor_with_own_owner(project.path(), 3).await;
        let hash = mcpls_core::hooks::identity_hash(project.path()).expect("hash");

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 6, "expected exactly six lines: {out}");
        assert_eq!(lines[0], format!("socket: {}", identity.socket.display()));
        assert_eq!(
            lines[1],
            format!("hook sees: {} -> {hash}", project.path().display())
        );
        assert_eq!(
            lines[2],
            format!("server sees: {} -> {hash}", project.path().display())
        );
        assert_eq!(lines[3], format!("owner pid: {}", std::process::id()));
        assert_eq!(
            lines[4],
            "hooks seen: 3 request(s) since this owner started"
        );
        assert!(lines[5].starts_with("mcpls on PATH: "));
    }

    /// `SessionStart` never touches the socket by design, so a server
    /// that started moments ago, or just took over from a previous
    /// owner, reads exactly like one nobody ever registered. The old
    /// wording asserted "may not be registered" on this state, which is a
    /// false alarm against a perfectly healthy, freshly started owner;
    /// the doctor cannot tell the two apart and must not guess which one
    /// it is looking at.
    #[tokio::test]
    async fn test_doctor_states_zero_hooks_seen_without_claiming_the_plugin_is_unregistered() {
        let project = tempfile::tempdir().expect("a temp dir");

        let (out, _identity) = doctor_with_own_owner(project.path(), 0).await;

        assert!(
            !out.contains("may not be registered") && !out.contains("may be unregistered"),
            "a live, reachable owner that has never been sent a hook is \
             indistinguishable from one that started a moment ago; \
             asserting non-registration here is a guess dressed as a \
             finding: {out}"
        );
        assert!(
            out.contains(
                "hooks seen: none since this owner started; send a prompt or \
                 make an edit in your Claude Code session for this project, \
                 then run the doctor again; if it is still none after that, \
                 the plugin's hooks are not reaching this server"
            ),
            "the doctor must still state the count as a fact, hand the \
             reader the action that resolves the ambiguity, and say what a \
             still-zero count after that action means: {out}"
        );
    }

    #[tokio::test]
    async fn test_doctor_reports_no_owner_when_nothing_is_reachable_anywhere() {
        let project = tempfile::tempdir().expect("a temp dir");

        let (out, identity) = doctor_with_nothing_running(project.path()).await;
        let hash = mcpls_core::hooks::identity_hash(project.path()).expect("hash");

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5, "expected exactly five lines: {out}");
        assert_eq!(lines[0], format!("socket: {}", identity.socket.display()));
        assert_eq!(
            lines[1],
            format!("hook sees: {} -> {hash}", project.path().display())
        );
        assert_eq!(
            lines[2],
            "server sees: no owner; nothing is listening on this project's socket"
        );
        assert_eq!(lines[3], "owner pid: none");
        assert!(lines[4].starts_with("mcpls on PATH: "));
    }

    /// The runtime location only exists once an owner has bound there,
    /// so its absence is the ordinary shape of a machine mcpls has never
    /// run on, not a scan failure. This is the first thing a new
    /// install's first `mcpls hook doctor` run would see.
    #[tokio::test]
    async fn test_doctor_reports_a_clean_no_owner_when_the_runtime_location_was_never_created() {
        let project = tempfile::tempdir().expect("a temp dir");
        let never_created = project.path().join("does-not-exist-mcpls-dir");
        let identity = local_identity_for(project.path(), &never_created);

        let out = super::doctor_scanning(project.path(), &identity, TEST_PIPE_PREFIX).await;

        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            "server sees: no owner; nothing is listening on this project's socket",
            "a machine where mcpls has never bound must not be reported as \
             a scan failure: {out}"
        );
    }

    /// A server started one level up or down from `CLAUDE_PROJECT_DIR` is
    /// exactly the shape a directory disagreement takes in reality, so the
    /// foreign owner here lives in a subdirectory of `project`, not an
    /// unrelated tempdir: only that relationship earns the specific,
    /// named answer.
    #[tokio::test]
    async fn test_doctor_finds_a_related_foreign_owner_running_for_a_different_directory() {
        let project = tempfile::tempdir().expect("a temp dir");
        let nested = project.path().join("nested");
        std::fs::create_dir(&nested).expect("mkdir");

        let out = doctor_with_foreign_owner(project.path(), &nested).await;

        let server_sees = out
            .lines()
            .find(|line| line.starts_with("server sees: "))
            .expect("a server-sees line is always printed");
        assert_eq!(
            server_sees,
            format!(
                "server sees: no owner for this directory; an mcpls is running \
                 for {} (pid {}) instead",
                nested.display(),
                std::process::id()
            ),
            "a server started in a different directory than CLAUDE_PROJECT_DIR \
             is the failure this command exists to diagnose, and printing \
             plain 'no owner' here would make it indistinguishable from \
             nothing running at all: {out}"
        );
        assert!(
            out.contains("owner pid: none"),
            "the foreign pid belongs in the server-sees line; owner pid \
             reports whether THIS project's own socket has an owner, which \
             it does not: {out}"
        );
    }

    /// The old behaviour named whichever owner `read_dir` happened to
    /// list first, so a developer with several unrelated projects open
    /// could be told a healthy, unrelated server was the reason their own
    /// hooks were dead. This must not happen: an owner sharing no
    /// ancestor/descendant relationship with `project_dir` is reported as
    /// a count, never as a name.
    #[tokio::test]
    async fn test_doctor_does_not_accuse_an_unrelated_project_of_being_the_reason() {
        let project = tempfile::tempdir().expect("a temp dir");
        let innocent = tempfile::tempdir().expect("a temp dir");

        let out = doctor_with_foreign_owner(project.path(), innocent.path()).await;

        assert!(
            !out.contains(&innocent.path().display().to_string()),
            "an unrelated project's server has nothing to do with this \
             project's own silence; naming it sends the reader to debug a \
             server that is not the problem, which is worse than the plain \
             ambiguity it would replace: {out}"
        );
        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            "server sees: no owner for this directory; 1 other mcpls \
             instance is running, none for this directory or a parent of it",
            "{out}"
        );
    }

    /// The scan's whole job is telling "nothing is running" apart from
    /// "something is running that explains this". A candidate that
    /// accepts a connection and then answers in a shape this build cannot
    /// read is running, and an mcpls on a previous protocol version is
    /// the likeliest way to be one. Counting it as nothing turns the
    /// scan's most confident sentence into its most wrong one: several
    /// live processes reported as an empty runtime directory.
    #[tokio::test]
    async fn test_doctor_counts_a_live_but_unreadable_socket_as_something_running() {
        let project = tempfile::tempdir().expect("a temp dir");

        let out = doctor_with_unreadable_foreign_owners(project.path(), 4).await;

        let server_sees = out
            .lines()
            .find(|line| line.starts_with("server sees: "))
            .expect("a server-sees line is always printed");
        assert!(
            server_sees.contains(
                "4 other mcpls sockets are live but did not answer a \
                 status request this build could read"
            ),
            "four processes were listening; the scan probed all four and \
             could not read any of them, which is a fact it must report \
             rather than discard: {out}"
        );
    }

    /// Naming an owner is the scan's most specific claim, and it is the
    /// one variant that could have been built without ever finishing the
    /// scan. If it were, the live-but-unreadable candidates listed after
    /// the named one would vanish, and whether they did would depend on
    /// the order the runtime directory happened to return its entries in:
    /// the same answer, from the same machine, differing run to run.
    #[tokio::test]
    async fn test_doctor_counts_unreadable_sockets_even_when_it_names_an_owner() {
        let parent = tempfile::tempdir().expect("a temp dir");
        let project = parent.path().join("child");
        std::fs::create_dir(&project).expect("create the child directory");

        let out = doctor_with_related_owner_among_unreadable_ones(&project, parent.path(), 3).await;

        let server_sees = out
            .lines()
            .find(|line| line.starts_with("server sees: "))
            .expect("a server-sees line is always printed");
        assert!(
            server_sees.contains(&format!(
                "an mcpls is running for {}",
                parent.path().display()
            )),
            "the related owner is still the headline: {out}"
        );
        assert!(
            server_sees.contains(
                "3 other mcpls sockets are live but did not answer a status request \
                 this build could read"
            ),
            "three other sockets were live and unreadable, and the scan \
             probed all of them before answering: {out}"
        );
    }

    /// `none` is the doctor's token for nothing holding the socket. Every
    /// arm reached by an accepted connection has an owner that simply did
    /// not name its process, which sends a reader somewhere else entirely.
    #[tokio::test]
    async fn test_doctor_says_the_pid_is_unknown_when_an_owner_exists() {
        let project = tempfile::tempdir().expect("a temp dir");

        let unreadable = doctor_with_owner_answering_raw_status(
            project.path(),
            r#"{"op":"status","hash":"abc","socket":"x","pid":1,"owner":true}"#,
        )
        .await;
        let wrong_shape = doctor_with_owner_answering_raw_status(
            project.path(),
            r#"{"op":"changed","queued":0}"#,
        )
        .await;
        let errored = doctor_with_owner_answering_error(project.path(), "not right now").await;

        for out in [&unreadable, &wrong_shape, &errored] {
            assert_eq!(
                out.lines()
                    .find(|line| line.starts_with("owner pid: "))
                    .expect("an owner pid line is always printed"),
                "owner pid: unknown",
                "something answered on this socket, so there is an owner; \
                 saying `none` here means the same word carries both \
                 'nobody is there' and 'somebody is there but did not say \
                 who': {out}"
            );
        }
    }

    /// The scan's cap means it may not reach every candidate, so which
    /// ones it reaches decides the answer. Left in `read_dir` order that
    /// is the filesystem's choice, and two runs against an unchanged
    /// machine could print different things. Deleting the sort cannot be
    /// caught by a doctor-level test: it makes the answer arbitrary
    /// rather than wrong, so a test asserting one answer merely becomes
    /// flaky. This asserts the order itself.
    #[cfg(unix)]
    #[test]
    fn test_the_candidate_scan_returns_a_stable_order() {
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        for name in ["m.sock", "a.sock", "z.sock", "b.sock"] {
            std::fs::write(socket_dir.path().join(name), b"").expect("write");
        }
        let identity = SocketIdentity {
            socket: socket_dir.path().join("own.sock"),
            lock: socket_dir.path().join("own.lock"),
            hash: "own".to_string(),
        };

        let candidates = super::foreign_candidates(&identity, TEST_PIPE_PREFIX).expect("the scan");

        let names: Vec<_> = candidates
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect();
        assert_eq!(
            names,
            ["a.sock", "b.sock", "m.sock", "z.sock"],
            "{candidates:?}"
        );
    }

    /// Whether a suite run exercises the found branch, the missing one,
    /// or only one of them is otherwise a property of the host's `PATH`.
    #[test]
    fn test_the_path_line_reports_both_outcomes() {
        assert_eq!(
            super::on_path_line(Some(Path::new("/usr/local/bin/mcpls"))),
            "mcpls on PATH: /usr/local/bin/mcpls"
        );
        assert_eq!(
            super::on_path_line(None),
            "mcpls on PATH: not found",
            "hooks invoke mcpls by name, so this line is the whole warning \
             that every hook is silently doing nothing"
        );
    }

    /// The singular form of the same clause, which the plural one does
    /// not cover: a count formatted with the wrong noun reads as a bug in
    /// the tool rather than the state of the machine.
    #[tokio::test]
    async fn test_doctor_reports_a_single_unreadable_socket_in_the_singular() {
        let project = tempfile::tempdir().expect("a temp dir");

        let out = doctor_with_unreadable_foreign_owners(project.path(), 1).await;

        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            "server sees: no owner; nothing is listening on this project's socket; \
             1 other mcpls socket is live but did not answer a status request this \
             build could read",
            "{out}"
        );
    }

    /// An owner that answers this project's own socket with a valid
    /// response of the wrong kind is neither unreachable nor unreadable,
    /// and the line saying so was the one user-visible string the
    /// deletion sweep could remove without failing a test.
    #[tokio::test]
    async fn test_doctor_reports_an_answer_that_is_not_a_status() {
        let project = tempfile::tempdir().expect("a temp dir");

        let out = doctor_with_owner_answering_raw_status(
            project.path(),
            r#"{"op":"changed","queued":0}"#,
        )
        .await;

        let server_sees = out
            .lines()
            .find(|line| line.starts_with("server sees: "))
            .expect("a server-sees line is always printed");
        assert!(
            server_sees.contains("answered, but not with its own status"),
            "an owner that answered the wrong response is a different \
             fact from one that could not be read and one that said \
             nothing: {out}"
        );
    }

    /// The singular count had a test; the plural sentence did not, and
    /// replacing it entirely leaves every other test green.
    #[tokio::test]
    async fn test_doctor_reports_a_plural_count_of_unrelated_owners() {
        let project = tempfile::tempdir().expect("a temp dir");
        let other_a = tempfile::tempdir().expect("a temp dir");
        let other_b = tempfile::tempdir().expect("a temp dir");

        let out =
            doctor_with_unrelated_owners(project.path(), &[other_a.path(), other_b.path()]).await;

        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            "server sees: no owner for this directory; 2 other mcpls \
             instances are running, none for this directory or a parent of it",
            "{out}"
        );
    }

    /// Past `MAX_FOREIGN_CANDIDATES`, the scan has not actually examined
    /// every socket in the runtime location, so it must not claim none of
    /// them relate to this project: that is a positive claim the
    /// truncated scan never earned.
    #[tokio::test]
    async fn test_doctor_admits_the_scan_was_truncated_past_its_candidate_limit() {
        let project = tempfile::tempdir().expect("a temp dir");
        let others: Vec<_> = (0..=MAX_FOREIGN_CANDIDATES)
            .map(|_| tempfile::tempdir().expect("a temp dir"))
            .collect();
        let other_paths: Vec<&Path> = others.iter().map(tempfile::TempDir::path).collect();

        let out = doctor_with_unrelated_owners(project.path(), &other_paths).await;

        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            format!(
                "server sees: no owner for this directory; {MAX_FOREIGN_CANDIDATES} \
                 other mcpls instances are running, none for this directory or a parent \
                 of it, but more may exist beyond the scan's limit"
            ),
            "with more candidates than the scan examines, it must not assert \
             an absence it never established; and the count is how many \
             answered, never how many were looked at: {out}"
        );
    }

    /// The same overreach, but for the case where nothing among the
    /// examined candidates answered at all rather than answering
    /// unrelated: `ForeignOwners::None` must carry `truncated` too, not
    /// only `Unrelated`.
    #[tokio::test]
    async fn test_doctor_admits_a_truncated_scan_found_nothing_conclusively() {
        let project = tempfile::tempdir().expect("a temp dir");
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project.path(), socket_dir.path());
        // Stale candidate files, none of them a real listener: nothing
        // answers, but there are more of them than the scan's own cap,
        // so it cannot have examined all of them.
        for i in 0..MAX_FOREIGN_CANDIDATES + 4 {
            std::fs::write(socket_dir.path().join(format!("stale-{i}.sock")), b"").expect("write");
        }

        let out = super::doctor_scanning(project.path(), &identity, TEST_PIPE_PREFIX).await;

        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            format!(
                "server sees: no owner; checked {MAX_FOREIGN_CANDIDATES} other candidates \
                 and none named an owner, but more may exist beyond the scan's limit"
            ),
            "with more stale candidates than the scan examines, it must not \
             claim the clean 'nothing is listening' it never established: {out}"
        );
    }

    /// A future forwarding proxy answers `Status` with `owner: false`; the
    /// scan must treat that exactly like no answer at all, not like an
    /// owner it can name.
    #[tokio::test]
    async fn test_doctor_ignores_a_reachable_socket_that_answers_it_is_not_the_owner() {
        let project = tempfile::tempdir().expect("a temp dir");
        let elsewhere = tempfile::tempdir().expect("a temp dir");

        let out = doctor_with_non_owner(project.path(), elsewhere.path()).await;

        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            "server sees: no owner; nothing is listening on this project's socket",
            "{out}"
        );
    }

    /// A connection that was accepted and never answered means an owner
    /// exists and is merely busy, which must read differently from
    /// nobody being there at all: the reader's next step is "wait", not
    /// "start the host" or "go debug some other directory". Drives a
    /// real listener that really accepts the connection, not a mocked
    /// error, since that is the exact distinction at stake.
    #[tokio::test]
    async fn test_doctor_reports_a_busy_owner_rather_than_no_owner() {
        let project = tempfile::tempdir().expect("a temp dir");

        let out = doctor_with_busy_owner(project.path()).await;

        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            format!(
                "server sees: a socket answered nothing within {}ms; an owner may be busy",
                SOCKET_TIMEOUT.as_millis()
            ),
            "{out}"
        );
        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("owner pid: "))
                .expect("an owner pid line is always printed"),
            "owner pid: unknown",
            "a busy owner is an owner: it accepted the connection and then \
             did not say which process it is, which is not the same fact \
             as nothing holding the socket at all: {out}"
        );
    }

    /// A busy owner on this project's own socket must not trigger the
    /// foreign-owner scan at all, even when a real, related owner exists
    /// elsewhere: there is already an owner here, so there is nothing to
    /// look for. If the scan ran anyway, this would show the related
    /// owner's line instead of the busy line.
    #[tokio::test]
    async fn test_doctor_does_not_scan_for_a_foreign_owner_when_its_own_owner_is_busy() {
        let project = tempfile::tempdir().expect("a temp dir");
        let nested = project.path().join("nested");
        std::fs::create_dir(&nested).expect("mkdir");

        let out = doctor_with_busy_owner_and_related_owner(project.path(), &nested).await;

        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            format!(
                "server sees: a socket answered nothing within {}ms; an owner may be busy",
                SOCKET_TIMEOUT.as_millis()
            ),
            "a busy owner on this project's own socket must not be reported \
             as no owner with some unrelated directory named as the cause: {out}"
        );
    }

    /// An owner that deliberately answers with an error has an
    /// explanation to give, and folding that into the busy line (as an
    /// earlier draft of this feature did) throws it away and replaces it
    /// with a guess about timing that the owner's prompt answer already
    /// disproves.
    #[tokio::test]
    async fn test_doctor_prints_an_owners_error_rather_than_calling_it_busy() {
        let project = tempfile::tempdir().expect("a temp dir");

        let out = doctor_with_owner_answering_error(project.path(), "boom").await;

        assert_eq!(
            out.lines()
                .find(|line| line.starts_with("server sees: "))
                .expect("a server-sees line is always printed"),
            "server sees: an owner answered with an error: boom",
            "{out}"
        );
    }

    /// A previous version's `Status` answer, missing a field this build
    /// now requires, is not the same fact as a busy owner: something
    /// answered at once, it just cannot be read. This is what a machine
    /// upgraded mid-session looks like from the doctor's side, and it is
    /// exactly when someone is likely to reach for this command.
    #[tokio::test]
    async fn test_doctor_reports_an_unreadable_reply_rather_than_calling_it_busy() {
        let project = tempfile::tempdir().expect("a temp dir");
        let raw = r#"{"op":"status","hash":"abc","socket":"x","pid":1,"owner":true,"root":"/x"}"#;

        let out = doctor_with_owner_answering_raw_status(project.path(), raw).await;

        let server_sees = out
            .lines()
            .find(|line| line.starts_with("server sees: "))
            .expect("a server-sees line is always printed");
        assert!(
            server_sees.contains("could not read its reply"),
            "an owner that answered promptly with something this build \
             cannot parse is not a busy owner: {out}"
        );
        assert!(
            !server_sees.contains("may be busy"),
            "calling a version mismatch 'busy' sends the reader looking \
             for load that does not exist: {out}"
        );
        assert!(
            !server_sees.contains("version"),
            "the four ways this exchange fails share only that something \
             holds the socket; naming one of them as the cause states \
             what the probe did not establish: {out}"
        );
    }

    /// A directory the scan cannot read (permissions, most plausibly) is
    /// not the same fact as an empty one: the scan never got to look, so
    /// it must say so rather than claiming the clean "nothing is
    /// listening" it has no evidence for. Unix-only: the permission trick
    /// this drives has no Windows equivalent, and the Windows pipe
    /// namespace's failure modes are not reproducible from a test.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_doctor_reports_a_scan_failure_rather_than_a_clean_no_owner() {
        use std::os::unix::fs::PermissionsExt;

        let project = tempfile::tempdir().expect("a temp dir");
        let socket_dir = tempfile::tempdir().expect("a temp dir");
        let identity = local_identity_for(project.path(), socket_dir.path());

        let original_mode = std::fs::metadata(socket_dir.path())
            .expect("stat the directory")
            .permissions()
            .mode();
        std::fs::set_permissions(socket_dir.path(), std::fs::Permissions::from_mode(0o000))
            .expect("lock the directory down");

        let out = super::doctor_scanning(project.path(), &identity, TEST_PIPE_PREFIX).await;

        // Restore access before any assertion can panic and skip this,
        // leaving this test's own TempDir unable to clean itself up.
        std::fs::set_permissions(
            socket_dir.path(),
            std::fs::Permissions::from_mode(original_mode),
        )
        .expect("restore the directory's permissions");

        let server_sees = out
            .lines()
            .find(|line| line.starts_with("server sees: "))
            .expect("a server-sees line is always printed");
        assert!(
            server_sees.contains("could not scan"),
            "the scan never ran, so the doctor must say so rather than \
             report a clean negative it has no evidence for: {out}"
        );
        assert!(
            !server_sees.contains("nothing is listening"),
            "'I could not look' and 'I looked and found nothing' are \
             different facts: {out}"
        );
    }

    #[tokio::test]
    async fn test_doctor_reports_whether_mcpls_is_on_path() {
        let project = tempfile::tempdir().expect("a temp dir");
        let (out, _identity) = doctor_with_nothing_running(project.path()).await;

        let line = out
            .lines()
            .find(|line| line.starts_with("mcpls on PATH: "))
            .expect(
                "hooks invoke mcpls from PATH, and a hook environment missing \
                 the install directory makes every hook do nothing, invisibly, \
                 so the doctor must carry one line that answers it",
            );
        let value = line
            .strip_prefix("mcpls on PATH: ")
            .expect("the line was found by this exact prefix above");
        assert!(
            value == "not found" || Path::new(value).is_absolute(),
            "the line has to carry an absolute path or a plain 'not found'; a \
             relative PATH entry (a bare 'bin', '.', or an empty ':: ' entry) \
             means nothing to whatever directory a hook later runs in: {line}"
        );
    }

    /// A path from `base` to `target`, built from `..` components and
    /// `target`'s own path past their common ancestor. Just enough to
    /// build a relative `PATH` entry for a test without mutating this
    /// process's actual current directory, which a multi-threaded test
    /// binary sharing one process cannot safely do.
    fn relative_from(base: &Path, target: &Path) -> PathBuf {
        let base: Vec<_> = base.components().collect();
        let target: Vec<_> = target.components().collect();
        let common = base
            .iter()
            .zip(target.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let mut relative = PathBuf::new();
        for _ in common..base.len() {
            relative.push("..");
        }
        for component in &target[common..] {
            relative.push(component.as_os_str());
        }
        relative
    }

    /// A `PATH` entry may be relative, and a relative path means nothing
    /// to whatever directory a hook later runs in. Deleting the
    /// absolutizing `.map` from `resolve_on_path`
    /// must fail this without depending on the ambient `PATH`, which is
    /// why the entry here is built from the real current directory
    /// instead of assumed to already be relative.
    #[test]
    fn test_resolve_on_path_absolutizes_a_relative_path_entry() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let exe_name = if cfg!(windows) { "mcpls.exe" } else { "mcpls" };
        let exe_path = dir.path().join(exe_name);
        std::fs::write(&exe_path, "").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let cwd = std::env::current_dir().expect("cwd");
        let relative = relative_from(&cwd, dir.path());
        assert!(
            relative.is_relative(),
            "the point of this test is a PATH entry that is not already \
             absolute: {relative:?}"
        );
        let path_var = std::env::join_paths([relative]).expect("join paths");

        let found =
            super::resolve_on_path(&path_var, exe_name).expect("the executable is right there");
        assert!(
            found.is_absolute(),
            "a relative PATH entry must never reach the doctor's output \
             verbatim: {found:?}"
        );
    }

    #[test]
    fn test_doctor_without_identity_says_no_socket_could_exist_rather_than_no_owner() {
        let project = tempfile::tempdir().expect("a temp dir");
        let missing = project.path().join("does-not-exist");
        let error = mcpls_core::hooks::identity_for(&missing).expect_err("an unreachable dir");

        let out = super::doctor_without_identity(project.path(), &error);

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5, "expected exactly five lines: {out}");
        assert_eq!(
            lines[0],
            format!("socket: none; could not derive an identity for this directory: {error}")
        );
        assert_eq!(
            lines[1],
            format!("hook sees: {} -> unknown", project.path().display())
        );
        assert_eq!(
            lines[2], "server sees: nothing can run here; no socket exists to probe",
            "'unknown' on this line would be the second, contradictory sense \
             of the word one line above, where the hash genuinely cannot be \
             computed: {out}"
        );
        assert_eq!(
            lines[3], "owner pid: none",
            "every doctor state prints an owner pid line, even this one, so \
             the two commands' output has one consistent shape: {out}"
        );
        assert!(lines[4].starts_with("mcpls on PATH: "));
    }
}
