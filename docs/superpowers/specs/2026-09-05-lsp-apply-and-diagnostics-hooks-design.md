# LSP edit application and diagnostics injection

Status: approved design, not yet implemented
Target: `AbysmalBiscuit/mcpls` fork, not upstream

## Problem

mcpls exposes LSP capabilities as MCP tools but never writes anything. `rename_symbol`,
`format_document`, and `get_code_actions` return descriptions of edits, and the agent
reproduces those edits by hand with `Edit`. That discards the reason to use LSP at all:
rust-analyzer's rename is scope-aware, while an agent working from `rg` output hits
shadowed bindings, string literals, comments, and unrelated same-named symbols in other
modules. The deterministic answer exists and is being thrown away.

Separately, mcpls holds a warm language server with a full workspace index and a live
`publishDiagnostics` cache, and none of it reaches the agent unless the agent thinks to
ask. Claude Code injects diagnostics natively after edits, but only for language servers
it spawns itself, which means running a second copy of rust-analyzer alongside the one
mcpls already owns.

## Goals

1. Let configured tools apply their edits to the working tree, defaulting to read-only.
2. Push diagnostics into the agent's context after any file change, from the warm server
   mcpls already owns, without a second language server and without stalling edits.

## Non-goals

- Upstreaming. This is fork-only; defaults are tuned for one user.
- A standalone mcpls daemon surviving Claude Code restarts. The MCP process keeps owning
  the language servers. The `mcpls hook` CLI surface is designed so this stays possible
  later without changing the plugin.
- Runtime workspace root changes (`/add-dir`, `DirectoryAdded`). Roots resolve from config
  at startup, as today.

## Part 1: applying edits

### Configuration

New `[apply]` table, absent by default, so existing configs behave exactly as before.

```toml
[apply]
rename              = true    # default false
format_document     = true    # default false
code_actions        = false   # default false
allow_file_deletion = false   # gates delete operations for all of the above
```

Named fields rather than `HashMap<ToolKind, Permission>`: only three of the fifteen
`ToolKind` variants can mutate anything, so a map would need validation purely to reject
meaningless keys like `hover = "apply"`, and `deny_unknown_fields` (already set on
`ServerConfig`) gives better typo errors on a struct than on a map. Booleans rather than a
`"read_only" | "apply"` enum because no third state exists.

### Tool surface

Each of the three tools gains an optional `apply: bool` parameter, default `false`. With
`apply: true` and the corresponding config key `false`, the call fails with an error naming
the config key rather than silently previewing. Preview-then-apply remains possible by
calling twice.

Applying code actions additionally requires `codeAction/resolve` for the common case where
a server returns an assist with `edit: null` and a `command` or `data` payload, followed by
`workspace/executeCommand`. Servers deliver the resulting edits back through an inbound
`workspace/applyEdit` request.

### The applier

New module `bridge/apply.rs`. One entry point takes an `lsp_types::WorkspaceEdit` and
returns a summary of what it wrote.

**Normalization.** A `WorkspaceEdit` arrives as `changes` (a plain URI map), as
`document_changes: Edits`, or as `document_changes: Operations` with resource operations
interleaved among text edits. All three collapse into one ordered operation list. Resource
operations run in array order, as the spec requires. Text edits within a single file apply
bottom-up, because every range refers to the original document. Two edits with overlapping
ranges in one file are a hard error, never a merge.

**Encoding.** Edit ranges use the server's negotiated position encoding, usually UTF-16.
`bridge/translator/encoding_ctx.rs` converts LSP positions outward for display; applying
needs the inverse, LSP position to byte offset, against the same content the server saw.
This is the likeliest place for silent corruption and gets direct tests against a
non-ASCII fixture.

**Staleness.** `TextDocumentEdit` carries an optional version. Before writing, each target
is re-read from disk and checked against what `DocumentTracker` believes the server last
saw. Any drift refuses the entire edit rather than writing a rebased guess.

**Confinement.** Every target path canonicalizes through `dunce` and must resolve under a
configured workspace root, with symlinks resolved before the prefix check. Delete
operations additionally require `allow_file_deletion`.

**Atomicity.** Two phases. Phase one computes every resulting file content in memory and
validates the whole operation list. Phase two writes, each file through a temp file plus
rename, holding original contents in memory for rollback. If a write fails, earlier writes
roll back. If rollback itself fails, the error names which files are in which state.

**Resync.** After a successful write, `DocumentTracker` updates and every server holding
the document receives `didChange` followed by `didSave`. `didSave` is what triggers
rust-analyzer's flycheck, so applying an edit feeds the diagnostics queue in part 2.

**Return value.** Per-file edit counts and the resource operations performed. No unified
diff: the agent re-reads what it needs, and diffs are expensive in tokens.

### Inbound `workspace/applyEdit`

`lsp/client.rs` answers this request with `{"applied": false}` from
`server_request_response`, a synchronous function with no access to translator state. It
gains an optional sink, installed only when apply is enabled for some tool:

```rust
type ApplySink = mpsc::Sender<(WorkspaceEdit, oneshot::Sender<ApplyOutcome>)>;
```

The message loop forwards the edit, awaits the oneshot with a timeout, and answers
`{"applied": bool, "failureReason": ...}`. With no sink installed the behavior is unchanged.

An inbound `applyEdit` is honored only while an apply-enabled tool call is in flight.
Outside that window the answer stays `{"applied": false}`. Without this rule a language
server could write to the working tree at a moment of its own choosing, with no tool call
and no user action behind it.

## Part 2: diagnostics injection

### Socket ownership

The socket path derives from a hash of the canonicalized primary workspace root:
`$XDG_RUNTIME_DIR/mcpls/<hash>.sock` on Linux, `$TMPDIR` on macOS,
`\\.\pipe\mcpls-<hash>` on Windows. `tokio` is already on `features = ["full"]`, so both
transports are available without a new dependency.

The first mcpls instance in a workspace binds. A later instance gets `EADDRINUSE`, pings
the existing socket, and either stays a passive non-owner (owner alive) or unlinks the
stale file and binds (owner dead). Two Claude Code sessions in one repository can share an
owner safely: diagnostics are workspace-scoped and identical regardless of which instance
answers, and per-session state is keyed by the session id the hook payload carries.

### Protocol

Newline-delimited JSON, one request per connection, connection closes after the response.

```
{"op":"changed","session":"<id>","paths":["/abs/src/x.rs"],"event":"change"}
{"op":"flush","session":"<id>"}
{"op":"ping"}
```

`changed` resyncs without flushing. `flush` drains without resyncing. `PostToolBatch` sends
`changed` followed by `flush` on one connection.

### Resync

For each path: re-read from disk, bump the `DocumentTracker` version, send `didChange` then
`didSave` to every server holding the document. A path mcpls has never opened is opened
first, subject to the existing `max_documents` and `max_file_size` limits. Hitting a limit
yields no diagnostics for that file and is not surfaced as an error. An `unlink` event maps
to `didClose`, an `add` event to `didOpen`. The call returns without waiting for any
publish to come back.

### Delivery and deduplication

`NotificationCache` already holds the latest diagnostics per server and URI. It gains a
per-session record mapping URI to a hash of that file's diagnostic set. A flush returns
every file whose hash differs from the record, then updates the record.

This is coarser than Claude Code's native per-diagnostic deduplication. The tradeoff is
deliberate: per-file needs far less state, and when a file breaks, its full current error
list is more useful than a delta against a list no longer in context. The cost is
re-sending an unchanged error in a file that gained a new one.

Session records drop on `SessionEnd`. A 4 hour idle timeout and a cap on record count exist
only as backstops for a session that ends without the hook firing.

### Caps

Per-file and total diagnostic counts, plus a severity floor, all configurable. Truncation
is stated in the output rather than applied silently.

```toml
[hooks.diagnostics]
enabled      = true    # default true: installing the plugin is the opt-in
languages    = []        # empty means every configured server
severity     = "warning" # error | warning | information | hint
max_per_file = 10
max_total    = 50
socket       = "auto"    # auto | off | <explicit path>
```

Unlike `[apply]`, diagnostics injection defaults on. Installing the plugin is the opt-in,
and a plugin whose hooks do nothing until a config file is written would look broken.
With `enabled = false` or `socket = "off"` the listener never binds and every hook exits 0
without output.

### Hook set

| Event | mcpls does | Injects context |
|---|---|---|
| `SessionStart` | returns `watchPaths` (the workspace roots) | no |
| `FileChanged` | `changed` for one path | no |
| `PostToolBatch` | `changed` for the batch's paths, then `flush` | yes |
| `UserPromptSubmit` | `flush` | yes |
| `Stop` | `flush` | yes |
| `SessionEnd` | drops the session record | no |

`FileChanged` is what covers writers that are not the `Edit` tool: `sed -i`, `cargo fmt`,
ad-hoc python scripts, `git checkout`, an editor open in another window. `PostToolBatch`
also carries the batch's file paths, so `FileChanged` is an optimization and not a
load-bearing dependency; if the watcher is unavailable or too noisy, resync still happens
per batch.

`PostToolBatch` is used in preference to `PostToolUse` because it fires exactly once per
batch, where `PostToolUse` runs concurrently for parallel tool calls and would put several
hook processes on the socket at once.

### Working directory

The hook resolves its socket from `CLAUDE_PROJECT_DIR`, falling back to walking up from the
changed file's absolute path to a project marker, and only then to the process working
directory. Deriving it from the working directory alone would break as soon as the agent
`cd`s into a subdirectory.

mcpls's own working directory is set at spawn and never changes; a `cd` in the Bash tool
happens in a different process. Workspace roots resolve and canonicalize at startup and are
fixed for the process lifetime, so agent navigation cannot move them. A file outside every
configured root gets no route, no diagnostics, and is refused by apply confinement.

### Failure is silent

No socket, connect timeout (50ms), malformed response, or any other fault: the hook exits 0
having printed nothing. An edit must never fail because diagnostics were unavailable.

## Part 3: packaging

### CLI

All subcommands read the hook payload from stdin and write hook JSON to stdout, so no shell
script is needed and the same definitions work on Windows.

```
mcpls hook session-start
mcpls hook file-changed
mcpls hook post-tool-batch
mcpls hook flush --event <name>
mcpls hook session-end
mcpls hook doctor            # socket path, owner pid, liveness, PATH check
mcpls format --file <path> --apply
```

`mcpls format` exists so a custom hook can format on write without that behavior being
built in.

### Plugin layout

```
plugin/
  .claude-plugin/plugin.json
  .mcp.json                 registers mcpls as the MCP server
  hooks/hooks.json          the six registrations above
  skills/mcpls/SKILL.md     moved from the repository's top-level skills/
```

The existing skill moves in so that installing the plugin brings it along.

Hooks invoke `mcpls` from `PATH`. If the hook process environment lacks the install
directory, every hook silently does nothing, which the silent-failure design makes
invisible. `mcpls hook doctor` exists to answer that, and the plugin README leads with it.

## Testing

The applier's normalization is pure and gets unit tests: three `WorkspaceEdit` shapes
collapsing to one operation list, bottom-up ordering within a file, overlapping ranges
rejected, UTF-16 offsets against a non-ASCII fixture, a symlink escaping a workspace root
rejected, version drift refused, rollback after a mid-write failure.

The socket gets integration tests over a temporary directory: bind, a second instance
deferring to a live owner, a second instance taking over a stale socket file, and a
delivery record returning a changed file once and not twice on a second flush.

End to end, `tests/ra_e2e.rs` already drives a real rust-analyzer against
`tests/fixtures/rust_workspace`. It extends to: rename a symbol with `apply: true` and
assert the on-disk files changed, then break a caller and assert the fanout file appears in
a later flush.

Windows named pipes will have no CI coverage. The path abstraction goes behind a trait so
the logic is testable, and the Windows transport itself is verified by hand or not at all.

## Risks

- Claude Code's file watcher may not exclude `target/`, in which case one `cargo check`
  could fire thousands of `FileChanged` hooks on a Rust repository. Measure before relying
  on it; `PostToolBatch` resync is the fallback.
- Position encoding conversion is the highest-consequence code in the applier. A wrong
  offset silently corrupts a file rather than failing loudly.
- Applying edits makes the agent's cached view of a file stale. The agent must re-read
  after an apply, and the tool result says so.
- Two rust-analyzer processes still exist if Claude Code separately spawns one through a
  plugin `.lsp.json`. Setting `forwardDiagnostics: false` on such a server suppresses
  duplicate injection but not the duplicate process; removing the `.lsp.json` is the
  actual fix.

## Verification of host behavior

The hook events, payload shapes, and native diagnostics behavior described here were read
out of the Claude Code binary rather than documentation, and are version-specific. To
re-verify against a different version:

```fish
strings -n 4 ~/.local/share/claude/versions/<version> > /tmp/cc.strings
rg -o 'hook_event_name:C\("FileChanged"\).{0,300}' /tmp/cc.strings
rg -o 'hookEventName:C\("PostToolBatch"\).{0,200}' /tmp/cc.strings
rg -o 'Whether to push publishDiagnostics.{0,200}' /tmp/cc.strings
```

Verified against 2.1.261:

- `Edit` and `Write` call `changeFile` then `saveFile`, both fire-and-forget with a
  `.catch` that only logs. Nothing is awaited and the tool result does not wait.
- `saveFile` sends `textDocument/didSave`, which is what makes rust-analyzer run flycheck.
- `publishDiagnostics` handlers drop stale publishes by document version, accumulate the
  rest, and flush them into the next model request as a `<new-diagnostics>` attachment,
  deduplicated and volume-capped.
- Plugins spawn language servers through `.lsp.json`, whose schema includes a per-server
  `forwardDiagnostics` toggle documented as suppressing automatic diagnostic injection
  while keeping navigation.
- `PostToolBatch` input carries `tool_calls: [{tool_name, tool_input, tool_use_id,
  tool_response?}]` and fires once per batch; `PostToolUse` may run concurrently across
  parallel calls.
- `FileChanged` input carries `{file_path, event}` where event is `change`, `add`, or
  `unlink`; hooks declare coverage by returning `watchPaths`.
- `additionalContext` is accepted from `PostToolBatch`, `UserPromptSubmit`, `Stop`,
  `SessionStart`, and others, but not from `FileChanged`, whose only output is
  `watchPaths`.
