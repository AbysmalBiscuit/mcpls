# Agent diagnostic attribution implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking. The user selects the execution method after reviewing this plan.

**Goal:** Deliver diagnostics to the agents that wrote each file, retain independent delivery history, and expire records without losing history during a reconnect to a surviving backend.

**Architecture:** Keep file ownership separate from per-recipient delivery state. Normalize hook and MCP identities at their entry points, retain report snapshots independently of the live cache, and track record lifetime through a shared connection registry. Extend the existing Codex adapter and shared backend.

**Tech stack:** Rust, Tokio, serde, rmcp, existing hook socket/named-pipe protocol, cargo-nextest through devkit.

**Spec:** [Shared backend design, Sessions, agents and records](../specs/2026-09-12-shared-backend-design.md#sessions-agents-and-records). Also read the acknowledgment, baseline, severity-floor, and cap rules in [the diagnostics design](../specs/2026-09-06-diagnostics-injection-design.md).

**Issue:** https://github.com/AbysmalBiscuit/mcpls/issues/26

## Global constraints

- Target this fork. Work in the existing issue worktree; publish or push only when requested.
- Writer sets are scoped to a file within a root session. Root fallback applies only to files with no known writer in that session.
- The first committed delivery closes an ownership cycle to new writers. The next edit replaces the writer set; additional edits before delivery add writers. Delivery itself retains ownership for later publications.
- Each frozen report retains its recipients and diagnostic state. One recipient's acknowledgment cannot consume another's report or erase a newer ownership cycle.
- Hook delivery commits on acknowledgment. MCP tool and footer delivery commit during flush. Preserve baseline guards, severity floors, muted-file behavior, and output caps.
- Claude agent identity depends on `agent_id` alone, never on `agent_type`. MCP calls with no agent marker use the session record.
- Codex hook `agent_id` and MCP `threadId` select the same record. Root hooks fall back to `session_id`. Resolve patch paths against payload `cwd` and ignore inherited `CLAUDE_PROJECT_DIR` for Codex.
- `SubagentStop` is not teardown. Root `SessionEnd` drops unprotected records immediately and marks protected records for removal after their last protecting connection closes.
- Record grace defaults to 60 seconds and is configurable. Backend idle shutdown remains independent; reconnect history survives only while the same backend lives.
- Records remain in memory. Use existing dependencies and retain Unix and Windows transports.
- Run project tasks through devkit. All implementation edits go through the edit tool. Commit selectively through `devrun task commit`, using the implementing model's actual co-author trailer.

## Checkout findings

This is a planning snapshot, not a claim that tests have passed. Recheck these locations before execution if HEAD changes.

- HEAD at inspection: `deb1282`, `fix(mcp): key codex records by thread (#54)`. Stage 2 is present despite the older design status note.
- `mcp/server.rs::call_tool` already resolves Codex metadata and adopts anonymous history once. `mcp/session_identity_tests.rs` exercises it through serialized MCP requests.
- `mcpls-cli/src/hook/codex.rs` already parses patch headers and uses payload `cwd`. It currently builds `session/agent` keys, unlike MCP's thread key, and sends `EndSession` for `SubagentStop`.
- `hooks/service.rs` ignores identity on `Changed`; it only enqueues paths. `hooks/protocol.rs` carries a session string without agent identity.
- `bridge/delivery.rs` keeps per-session hashes and one staged token per session. It does not retain a diagnostic snapshot for another recipient.
- `backend/endpoint.rs` removes anonymous records on disconnect. Named records have no connection-counted grace policy. HTTP cleanup lives in `mcp/server.rs`.
- `Sweeper::enqueue` filters paths and returns a count, not the admitted paths. Attribution must share admission and URI normalization with diagnostics.

## Review focus

1. A delayed acknowledgment after another edit must preserve both the older recipient's pending report and the newer ownership cycle. Task 2 owns this test.
2. A rename/delete or aliased path must use the same file identity as cached diagnostics, including paths that no longer exist. Tasks 2 and 3 own these tests.
3. MCP identity may arrive before hooks or contain only `threadId`; discovering the root later must preserve history and correct fallback routing. Tasks 1 and 4 own these tests.
4. A successful disk write can outlive cancellation or precede a partial failure. Attribution must record the actual written paths even if the tool never returns successfully. Task 3 owns this test.
5. An agent that only fires hooks must expire without an MCP close event, while an active root or agent connection protects it. Task 4 owns this test.

## File responsibilities

Paths below are relative to the checkout.

| File | Responsibility |
| --- | --- |
| `crates/mcpls-core/src/bridge/identity.rs` (new) | Typed record identity and root association, shared by both entry points |
| `crates/mcpls-core/src/bridge/delivery.rs` | Ownership cycles, retained reports, acknowledgment, existing deduplication and budgets |
| `crates/mcpls-core/src/bridge/delivery_tests.rs` (new) | Attribution state-machine tests, included as a child test module |
| `crates/mcpls-core/src/bridge/record_lifetime.rs` (new) | Connection membership, end markers, grace deadlines |
| `crates/mcpls-core/src/bridge/mod.rs` | Export the shared types |
| `crates/mcpls-core/src/hooks/{protocol,listener,service,sweep}.rs` | Carry caller identity, acknowledge the same recipient, admit and attribute hook writes |
| `crates/mcpls-core/src/mcp/{server,handlers,session_identity_tests}.rs` | Per-request caller, shared state, MCP write attribution, both delivery entry points |
| `crates/mcpls-core/src/bridge/translator/{mod,edits}.rs`, `bridge/apply/mod.rs` | Associate actual writes with their request, including partial and cancelled applies |
| `crates/mcpls-core/src/{lib,transport}.rs`, `backend/endpoint.rs` | Shared runtime lifecycle, expiry worker, connection registration and cleanup |
| `crates/mcpls-cli/src/hook.rs`, `hook/codex.rs` | Normalize host payloads and extend existing parser/dispatcher tests |
| `plugin/hooks/hooks-codex.json` | Remove the unnecessary `SubagentStop` registration |
| `crates/mcpls-core/src/config/mod.rs`, `schema/mcpls-config.json` | Configurable record grace, default, template, schema |
| `crates/mcpls-core/tests/hooks_socket.rs` | Real protocol round-trip coverage |
| `README.md` | Routing and reconnect behavior, grace configuration |

The schema task regenerates the file owned by `crates/mcpls-core/tests/config_schema.rs`. Avoid moving unrelated existing tests or refactoring the server wholesale.

## Execution commands

Use this checkout for every command:

```fish
devrun -C /home/lev/Git/lev/mcpls_worktrees/26-deliver-each-diagnostic-only-to-the-agents task
devrun -C /home/lev/Git/lev/mcpls_worktrees/26-deliver-each-diagnostic-only-to-the-agents task test
```

The configured `test` task runs workspace tests with all features. For each RED/GREEN cycle below, run it and check the named new tests. A missing method, compile error, or fixture setup failure is not behavioral RED: add only the necessary type/plumbing skeleton, then confirm the assertions fail because routing or lifetime is wrong. Add a filtered devkit task only if suite cost warrants it; do not bypass the harness with handwritten cargo commands.

Before each logical implementation commit, inspect the diff, stage only that task's files, and inspect `git diff --staged`. Discover the current commit task arguments with `devrun task`; if it still reports `commit` as invalid, resolve that task configuration before committing rather than bypassing it with raw `git commit`. No commit or push is needed to review this plan.

## Task 1: Unify hook and MCP caller identity

**Files:** Create `bridge/identity.rs`; modify `bridge/mod.rs`, `hooks/protocol.rs`, `hooks/listener.rs`, `hooks/service.rs`, `mcp/server.rs`, `mcp/session_identity_tests.rs`, CLI `hook.rs` and `hook/codex.rs`, and `plugin/hooks/hooks-codex.json`. Migrate delivery map keys to `RecordId` here without changing routing; Task 2 adds routing behavior.

**Interfaces:** Define these shared core types. `SessionId` remains the existing nonempty host/session identity wrapper. Root association is optional until it is known; it is not part of a Codex thread's record key.

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RecordId {
    Session(SessionId),
    ClaudeAgent { root: SessionId, agent: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    pub record: RecordId,
    pub root: Option<SessionId>,
}
```

Use `RecordId::Session(thread_id)` for Codex roots and children, and `RecordId::Session(session_id)` for Claude roots. Claude child keys are structured pairs rather than concatenated strings. Keep hook wire fields `session`, optional `agent_id`, and a defaulted `host` discriminator with `claude` and `codex` values. Apply those fields consistently to `Changed`, `Flush`, and `Ack`; `EndSession` names the root. Old payloads without the new fields retain Claude root behavior. A `FileChanged` notification needs a separate attributed/unattributed distinction in Task 3.

- [ ] Add dispatcher tests using `RecordingOwner`, `dispatch_as`, and the real JSON payload. Assert the changed request, flush request, and acknowledgment all preserve root and agent separately. Test Claude `agent_id` with no `agent_type`, root payloads, empty agent IDs, and identifiers containing `/`.

```rust
let payload = json!({
    "hook_event_name": "PostToolUse",
    "session_id": "root",
    "agent_id": "child",
    "cwd": recorder.project_dir(),
    "tool_name": "apply_patch",
    "tool_input": {"command":
        "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** End Patch\n"}
});
dispatch_as(Host::Codex, &payload, &recorder).await;
let requests = recorder.requests();
let wire = serde_json::to_value(&requests[0]).unwrap();
assert_eq!(wire["session"], "root");
assert_eq!(wire["agent_id"], "child");
assert_eq!(wire["host"], "codex");
```

- [ ] Replace the existing test that expects `SubagentStop` to end a record with this behavioral assertion and observe RED:

```rust
let recorder = RecordingOwner::start();
let out = dispatch_as(Host::Codex,
    &json!({"hook_event_name":"SubagentStop",
            "session_id":"root", "agent_id":"child"}),
    &recorder).await;
assert_eq!(out, "");
assert!(recorder.requests().is_empty());
```

- [ ] Normalize the wire identity into `Caller` at the service boundary; use the same record key in `call_tool`. Preserve the current client-name check, top-level/nested thread precedence, anonymous fallback, and one-time anonymous adoption. Read root `session_id` from valid turn metadata when available; use hook root association when metadata supplies only a thread. Do not assume `parent_thread_id` is the root for nested children. Missing root metadata remains unknown, rather than falsely identifying the child as a root.
- [ ] Add a serialized MCP/hook test proving a hook delivery to `child` is already delivered when `get_new_diagnostics` is called with `_meta.threadId = child`. Exercise the reverse order too. Before root association is known, retain identity history; Task 2 suppresses unowned-file fallback for an unresolved child.
- [ ] Make `SubagentStop` a no-op in the adapter and remove its registration. `SessionEnd` names the root without synthesizing a `root/child` string. Run GREEN, including the existing metadata precedence/adoption tests. Commit as `fix(hooks): unify agent delivery identity`.

## Task 2: Separate ownership from retained delivery reports

**Files:** Modify `bridge/delivery.rs`, `bridge/mod.rs`, `mcp/server.rs`; create `bridge/delivery_tests.rs`; extend `hooks/service.rs` tests and `mcp/session_identity_tests.rs`.

**Interfaces:** `stage`, `flush`, and `commit` use `&RecordId` from Task 1. Add `register_caller(&mut self, caller: &Caller)` and `record_write(&mut self, caller: &Caller, keys: &[String])`. File keys use the cache's URI spelling. Keep report tokens recipient-scoped. Root lookup uses the caller identity produced by Task 1.

Use private `Ownership` state with a monotonically increasing generation, a writer set, and a flag recording whether that generation has committed a delivery. A retained report stores its generation, visible diagnostic snapshot, source metadata needed for rendering, and unacknowledged recipients. Reuse existing `FlushReport` and budget logic at the output boundary. Retain URI, server owner, version, and position encoding with the report; rendering cannot depend on a cache entry that a later publication or delete has replaced. Preserve the original converted diagnostic ranges if subsequent document edits would make conversion against live document state incorrect.

- [ ] Add a serialized MCP read test with one root, two children, and diagnostics in `a.rs`, `b.rs`, and unowned `c.rs`. Arrange writer state using `record_write` for A and B; Task 3 adds actual write-entry-point coverage. Assert exact path sets: A gets A, B gets B, root gets C, and their subsequent reads are empty. Change the old test that expects every Codex child to receive the same unowned error. Observe behavioral RED.
- [ ] Add state-machine tests using existing `entry` and `diagnostic` helpers in a child module. This is the basic multiwriter contract:

```rust
let root = SessionId::from("root".to_string());
let a = Caller { record: RecordId::Session("a".to_string().into()),
                 root: Some(root.clone()) };
let b = Caller { record: RecordId::Session("b".to_string().into()),
                 root: Some(root) };
let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
delivery.set_baseline(HashMap::new());
delivery.register_caller(&a);
delivery.register_caller(&b);
delivery.record_write(&a, &["a.rs".to_string()]);
delivery.record_write(&b, &["a.rs".to_string()]);
let errors = vec![diagnostic(0, DiagnosticSeverity::ERROR, "broken")];
let entries = [entry("a.rs", &errors, SeverityFloor::Warning)];
let (first, token) = delivery.stage(&a.record, &entries);
assert_eq!(first.changed.len(), 1);
assert!(delivery.commit(&a.record, token.unwrap()));
assert_eq!(delivery.flush(&b.record, &entries).changed.len(), 1);
assert!(delivery.flush(&a.record, &entries).changed.is_empty());
```

- [ ] Extend that fixture to cover a later publication without an edit; a new B edit after A committed; acknowledgment of an older report after that edit; unchanged hashes after ownership transfer; cleared diagnostics; omitted files; muted files; and the same URI in independent root sessions. Assert old pending recipients survive the ownership change. Retry a staged report and prove a stale token cannot advance its replacement.
- [ ] Implement ownership transitions and retained reports under the delivery lock. Freeze recipients when a changed report is first materialized, including recipients other than the one currently flushing. Observe the old cache state before switching a file's ownership cycle, so a late reader cannot move an already pending report to the new writer. Retain already frozen reports; coalesce only live publications that have not been frozen. Release a frozen report after every recipient has committed it or expired. Limit each output using the existing caps; deferred files keep both their data and recipients.
- [ ] On a record with known root, route only its owned files plus its existing pending reports. On the root record, also admit unowned files. An identified thread with no known root may read its established pending/owned reports but gets no speculative root fallback; a later hook or valid root metadata resolves the association without discarding history. Anonymous connections retain their existing isolated-session behavior.
- [ ] Keep the baseline guard before creating delivery history and preserve delivery-before-cache lock order. Snapshot only necessary payload data, then release locks before asynchronous rendering. Add a test where the live cache changes or removes a file between staging and another recipient's read; that recipient still receives the retained report. Run GREEN and commit as `feat(diagnostics): route reports to file writers`.

## Task 3: Attribute actual writes through both entry points

**Files:** Modify `hooks/service.rs`, `hooks/sweep.rs`, `hooks/protocol.rs`, CLI `hook.rs` and `hook/codex.rs`, `mcp/server.rs`, `bridge/translator/{mod,edits}.rs`, and `bridge/apply/mod.rs`. Extend `hooks_socket.rs` and the existing dispatcher/apply tests.

**Interfaces:** Add an explicit `attributed: bool` to `Changed`, defaulting to false for old wire messages. Tool-derived changes set it true; filesystem notifications leave it false. Add `Sweeper::admitted_paths(&self, paths: &[PathBuf]) -> Vec<PathBuf>` and have both attribution and enqueue use the same admitted collection. Feed normalized URI keys into Task 2's `record_write` before diagnostic delivery can consume them.

- [ ] Through `dispatch_as`, test Codex add/update/delete/move patches, multiple files, CRLF input, spaces in paths, relative paths under a subdirectory, absolute paths, and content lines resembling headers. A move names both source and destination. Test non-patch tools and malformed input: they create no writer claims. Keep valid envelope parsing separate from payload decoding.

```rust
let patch = "*** Begin Patch\n*** Update File: old name.rs\n\
             *** Move to: new name.rs\n@@\n-old\n+new\n\
             *** Delete File: gone.rs\n*** End Patch\n";
assert_eq!(apply_patch_paths(patch),
           vec!["old name.rs", "new name.rs", "gone.rs"]);
assert!(apply_patch_paths("*** Update File: outside-an-envelope.rs").is_empty());
```

- [ ] Send a Claude `PostToolBatch` with `agent_id` but no `agent_type` through the real dispatcher. Assert it attributes its accepted file paths to that agent. Send `FileChanged` for the same file afterward and assert it neither adds root as writer nor starts a new ownership cycle. Watcher notifications can refresh diagnostics but cannot identify the editor.
- [ ] Add a real service test for ignored/out-of-root paths and deleted paths. Normalize existing paths through the established path/URI helpers; for missing paths, resolve the existing ancestor and preserve the remaining relative components. Reject paths outside workspace roots after normalization. Use the same key for symlink aliases and the resulting diagnostic URI. Observe RED before changing admission and attribution.
- [ ] Register hook writers before enqueueing their accepted paths and before the subsequent flush. Retain the payload's original `cwd` for patch resolution even though endpoint identity uses the checkout root. Upgrade the current patch-header scanner to validate envelope/header positions so ordinary content is never attributed as a file. Keep malformed hook handling silent, matching current dispatcher behavior.
- [ ] Cover MCP `rename_symbol`, `format_document`, and `apply_code_action` through serialized requests with an identified caller. Attribute all actual written paths, including both ends of resource operations and successful writes preceding an error. Preview/no-op requests create no ownership. Thread a request-owned write observer into the applier's blocking write work instead of setting a mutable caller on the shared `Translator`. The observer records the caller and written paths independently of the requesting future's lifetime. Preserve attribution for writes that finish after cancellation; apply the queued attribution before a flush reads the corresponding diagnostic state. Keep synchronization local to the write/delivery boundary and preserve existing document-lock order.
- [ ] Extend the cancellation/partial-apply fixtures to assert both actual disk changes and eventual recipient routing after cancelling the requesting future. This test must enter through the MCP write call, not call `record_write` directly. Run GREEN, including existing resync and partial-apply tests. Commit as `feat(diagnostics): attribute hook and mcp writes`.

## Task 4: Count protecting connections and expire records

**Files:** Create `bridge/record_lifetime.rs`; modify `bridge/mod.rs`, `bridge/delivery.rs`, `mcp/handlers.rs`, `mcp/server.rs`, `backend/endpoint.rs`, `transport.rs`, `lib.rs`, and `config/mod.rs`; regenerate the existing config schema and update the config template.

**Interfaces:** Keep one registry with the delivery state so expiry and record removal are atomic. Use `tokio::time::Instant` to permit deterministic tests. Define these operations on `RecordLifetime`; expiry returns records for `DiagnosticsDelivery` to remove, including their pending-recipient references and ownership membership.

```rust
pub(crate) fn attach(&mut self, connection: ConnectionId, caller: &Caller);
pub(crate) fn identify(&mut self, connection: ConnectionId, caller: &Caller);
pub(crate) fn close(&mut self, connection: ConnectionId, now: tokio::time::Instant);
pub(crate) fn touch_hook(&mut self, caller: &Caller, now: tokio::time::Instant);
pub(crate) fn end_root(&mut self, root: &SessionId, now: tokio::time::Instant);
pub(crate) fn expire(&mut self, now: tokio::time::Instant) -> Vec<RecordId>;
pub(crate) fn next_deadline(&self) -> Option<tokio::time::Instant>;
```

`RecordLifetime::new(grace: Duration)` supplies the policy. A connection may have named multiple records; register each association idempotently and remove its memberships exactly once on close. Identifying a child protects that child's record, not every sibling. A root connection protects its root and associated children.

- [ ] Add endpoint tests for two connections to one session, root `SessionEnd` while a connection is open, last close after end, disconnect/reconnect inside grace, and expiry after grace. Keep a separate session connected for the reconnect test so idle shutdown cannot invalidate the premise. Drive time with paused Tokio time or injected instants, not minute-long sleeps.
- [ ] Add hook-only expiry cases: an agent with no MCP history; hook renewal just before expiry; root connection arriving after the hook; root closing while the child retains its own connection; and root ending before the child closes. Assert record contents and pending tokens, not only registry counts.

```rust
let grace = Duration::from_secs(60);
let t = tokio::time::Instant::now();
let mut lifetime = RecordLifetime::new(grace);
let child = Caller {
    record: RecordId::Session("child".to_string().into()),
    root: Some("root".to_string().into()),
};
lifetime.touch_hook(&child, t);
assert!(lifetime.expire(t + Duration::from_secs(59)).is_empty());
lifetime.touch_hook(&child, t + Duration::from_secs(59));
assert!(lifetime.expire(t + Duration::from_secs(60)).is_empty());
assert_eq!(lifetime.expire(t + Duration::from_secs(119)), vec![child.record]);
```

- [ ] Add `[diagnostics].record_grace_ms`, default `60_000`, with zero meaning immediate expiry once unprotected. Include serde default, default constructor, generated config text, schema, and a config parse/default test. Keep the setting in diagnostics because `--no-backend` and HTTP also own delivery records.
- [ ] Register named connections when serving begins and anonymous adoption/identified records as calls arrive. Preserve the one-time history merge. Cleanup must run after outstanding handlers can no longer recreate records; reuse the endpoint's existing handler-drain ordering and cover HTTP and in-process stdio as well as backend sockets. Anonymous connections that never identify still lose their history immediately on close.
- [ ] Start one expiry worker with the runtime, awakened on changed deadlines and cancelled with the runtime. Under the delivery lock, expire records, recipient references, root associations, and obsolete ownership. Do not leave a timer per hook. `SessionEnd` marks the root and children and invokes immediate expiry for unprotected records. A stale deadline must be rechecked against current activity before removing anything. Hooks renew grace only for records not already marked ended.
- [ ] Keep backend idle calculation based on MCP attachments. Add a test that the backend exits after its idle timeout despite pending record grace, and a replacement starts with fresh history. Run GREEN, generate schema with `devrun task schema`, and commit as `fix(diagnostics): expire disconnected records`.

## Task 5: Verify the integrated contract and document behavior

**Files:** Extend `crates/mcpls-core/tests/hooks_socket.rs`, CLI dispatcher tests, `mcp/session_identity_tests.rs`, endpoint integration tests, and `crates/mcpls-core/tests/ra_e2e.rs`. Update `README.md` and the design's stale stage-status wording after implementation is verified.

**Interfaces:** Exercise the production CLI payload dispatcher, hook wire protocol, serialized MCP requests, and diagnostic publish handling. Reuse existing fixture servers and temporary endpoints. No new public tool is required.

- [ ] Add an integrated Codex scenario: root and two thread clients share a backend, real `PostToolUse` patch payloads attribute different files, controlled diagnostic publications include those files and an unowned dependent file, and each hook/MCP reader gets the exact allowed set. A hook acknowledgment must suppress the same thread's subsequent MCP report. Repeat the Claude scenario through agent hook payloads, with unmarked MCP calls reading root only.
- [ ] Exercise overlapping writers with different acknowledgment order, a new edit before the second writer reads, and a cache replacement before that read. Assert the report content as well as paths, so retaining only recipients cannot pass. Use the actual `Changed`, `Flush`, and `Ack` JSON framing. Include output-cap deferral in this scenario.
- [ ] Extend the real-language-server e2e fixture so two attributed edits produce diagnostics through the existing publish path; prove each agent sees only its file and root receives a dependent unowned-file error. Wait on server progress/publishes using existing fixture mechanisms. Do not assert fixed wall-clock indexing times. Record environmental skips separately from passes.
- [ ] Document retained ownership, Claude's unmarked-MCP limitation, Codex identity, `record_grace_ms`, and the same-backend reconnect condition. Keep the explanation independent of issue history and use default values only where the config/schema provides a way to verify them. Replace the design's obsolete stage-status claim with a pointer to the implementation/verification record rather than inventing a status for the watcher stage.
- [ ] Run the final checks below and review the complete diff against the spec. Resolve failures introduced by this work; report pre-existing failures with evidence. Commit as `test(diagnostics): verify per-agent delivery`, including the behavior documentation. Stop for review; do not push or open a PR.

```fish
devrun -C /home/lev/Git/lev/mcpls_worktrees/26-deliver-each-diagnostic-only-to-the-agents task fmt
devrun -C /home/lev/Git/lev/mcpls_worktrees/26-deliver-each-diagnostic-only-to-the-agents task verify
devrun -C /home/lev/Git/lev/mcpls_worktrees/26-deliver-each-diagnostic-only-to-the-agents task test-e2e
git -C /home/lev/Git/lev/mcpls_worktrees/26-deliver-each-diagnostic-only-to-the-agents diff --check
```

On Windows, run the same protocol and lifecycle tests over named pipes. If this session cannot run Windows, record that verification as outstanding instead of claiming platform coverage from Unix results.

## Completion criteria

- Every routing assertion passes through an entry point an agent actually uses, in addition to focused state-machine tests.
- Independent recipients, retained ownership, delayed acknowledgments, cap deferral, hook-only expiry, and same-backend reconnects have explicit regression coverage.
- Existing baseline, floor, mute, anonymous identity, tool metadata, resync, and cancellation behavior remains covered.
- The configuration schema agrees with the config type; final verification results name any skips or platform gaps.
- No production service, daily-driver binary, plugin installation, or external tracker state was changed by validation.

## Unresolved questions

No product decisions remain. Execution method awaits the user's selection. Windows runtime validation requires a Windows runner; absence of one is a verification limitation, not permission to silently omit its tests.
