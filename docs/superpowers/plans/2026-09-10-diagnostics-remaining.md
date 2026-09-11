# Remaining diagnostics implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox syntax for tracking. Root dispatches and reviews each task before the next implementer starts.

**Goal:** Close the audited diagnostics source/configuration defects and the real rust-analyzer deduplication evidence gap for issue #1.

**Architecture:** Keep the current router, delivery records, baseline tracker, hook owner, sweeper and write footer. Correct their configuration inputs and lifecycle boundaries without replacing the design. Reproduce client-visible failures through MCP or CLI, and ownership/sweep failures through the real hook request path.

**Tech Stack:** Rust, Tokio, rmcp, LSP, TOML, Cargo nextest, devkit.

**Spec:** `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`; `.devkit/issue1-reconciliation.md` supplies the binding scope/timing rulings. Read `.devkit/issue1-dispatch-contract.md` before executing a task. The completed Stage A and Stage B/C audits are evidence inputs, not requests to repeat the audit.

## Global constraints

- Worktree `/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion`, branch `lev/diagnostics-completion`. Root records the actual BASE at each dispatch; the audit snapshot is `23b69ca88da42d8e71569ba296253f917dbb4915`.
- Implementers are `gpt-5.6-luna` at `max`; reviews use `gpt-5.6-sol` at `medium` or `gpt-6-astra` at `medium`. One implementer and one Cargo command run at a time. Workers do not dispatch agents or publish.
- Claim each task's named files before edits. Shared fixture files pass sequentially between tasks. Root owns validation task configuration, dispatch ledger and publication.
- Run project commands through devrun and `.devkit/test.env`. Preserve `CARGO_BUILD_JOBS=1`, the worktree target, strict Rustdoc, and the existing TMPDIR. Do not alter PATH, compiler wrappers, caches, installed binaries/plugins, or use sudo. Do not read CONTRIBUTING.md.
- Each behavior correction needs an entry-point RED caused by the audited behavior, then GREEN on the same assertion. Compilation/setup failures do not count. A missing real-server prerequisite is a validation gap, not a pass.
- Keep zero as unlimited for documents. Finite background sweeps reserve one ordinary-request slot. Silent active registered diagnostics owners get the existing two-second no-progress grace. The grace remains a heuristic.
- Preserve command override reset, stale-publish rejection, cap fairness, clear budgeting, mute semantics, default-off footer, and existing owner/passive delivery semantics outside the specified defects.
- Original desktop artifact recovery equivalence belongs to issue #7. Live host/cross-user validation belongs to issue #6. Neither is proved by repository tests; manual Windows validation must also remain explicitly qualified.
- Root already has execution and commit authorization. Task workers stage selectively, inspect staged diffs and commit after their task's checks; root reviews before continuing. No additional approval question is required.

## File responsibilities and test execution

All paths below are relative to the worktree. `config/mod.rs` owns validation/default creation; `lib.rs` owns startup selection and baseline setup; `bridge/settle.rs` owns startup settling and the existing footer progress observations; `bridge/delivery.rs` owns session keys; `hooks/sweep.rs` owns background admission; `hooks/service.rs` owns role transitions; `mcp/server.rs` owns write entry points/footer timing. Keep these responsibilities in place.

Reuse `tests/e2e/mcp_client.rs`, loaded by the `integration_tests` test binary, for actual `initialize` and `tools/call`. Extend its existing spawn path with this test-only constructor when required:

```rust
pub fn spawn_in_workspace(
    args: &[&str],
    workspace: &std::path::Path,
    session_id: Option<&str>,
) -> anyhow::Result<Self>
```

`None` removes `CLAUDE_CODE_SESSION_ID` from the child only; `Some("")` sets it empty. It must preserve the existing reader, teardown and binary resolution. Do not mutate the test runner's environment. Add controlled LSP process behavior only where an existing fixture cannot drive the failure. Such support lives in `tests/e2e/diagnostics_fixture.rs`, declared by `tests/e2e/mod.rs`, and must carry real LSP framing. Extend an existing process fixture if one is already suitable; the channel-only `common/mock_lsp.rs` does not by itself prove the MCP/LSP process path. Each task owns these support files only if its test needs an addition.

Root must add focused tasks to `.devkit/validation.toml` before dispatch. That file currently contains broad tasks only. Define `i1-t1` through `i1-t9` using this pattern, replacing the filter with each task's specified prefix:

```toml
[tasks.i1-t1]
run = ["cargo", "nextest", "run", "-p", "mcpls-core", "-p", "mcpls", "--all-features", "--locked", "--run-ignored", "all", "-E", "test(i1_t1_)"]
guard = true

[tasks.i1-ra]
run = ["cargo", "nextest", "run", "-p", "mcpls-core", "--test", "ra_e2e", "--all-features", "--locked", "--run-ignored", "all", "-E", "test(ra_e2e_suite)"]
guard = true
env = { MCPLS_RA_FILTER = "resync_delivers_a_build_error_after_an_apply" }
```

The RA registry uses `sub_case!(sc_resync_delivers_a_build_error_after_an_apply)`, so the supplied substring selects the existing scenario. Verify that execution actually selects it; the filter matches scenario names. A selected-but-skipped real-server suite does not meet the evidence gate. Add named covering selectors for changed existing tests when each task report identifies them. A zero-test selection fails preflight.

Use absolute commands. For task 1, RED and GREEN use:

```sh
devrun -C /home/lev/Git/lev/mcpls_worktrees/diagnostics-completion --config /home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/validation.toml task build --env-file /home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/test.env
devrun -C /home/lev/Git/lev/mcpls_worktrees/diagnostics-completion --config /home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/validation.toml task i1-t1 --env-file /home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/test.env
```

For each subsequent task substitute its `i1-tN` name. Rebuild before testing changed production code with an external MCP binary, so GREEN cannot use a stale executable. Run `fmt-check` and `lint` with the same absolute prefix and env-file after source changes; root may serialize/coalesce unchanged broad checks. Store exact commands, selected test counts and RED/GREEN output in `.devkit/issue1-logs/` and the named implementation report. Root performs integrated `verify` and `docs` once after all accepted tasks.

### Task 1: Reject unroutable novel file-tool configurations

**Files:** Modify `crates/mcpls-core/src/config/mod.rs`; test `crates/mcpls-cli/tests/cli_integration.rs` and `crates/mcpls-core/tests/e2e/protocol_tests.rs`; support files from the preceding section as needed.

**Interfaces:** Consumes the resolved `ServerConfig`, effective workspace extension mappings and existing tool kinds. Produces the existing config load result with a useful validation error for a novel file-tool language with no effective extension mapping. No public API change.

- [ ] Write `i1_t1_novel_file_language_requires_mapping` through CLI startup with a config containing `language_id = "elixir"`, a command and no effective mapping. Assert rejection identifies `elixir` and tells the user to add `file_patterns` or a workspace mapping. Use an available controlled command so missing-executable failure cannot masquerade as validation.
- [ ] Add accepted cases for `*.ex`, explicit workspace extension mapping, built-in command overrides, and a workspace-only tool server. For the two mapped cases issue `get_hover` on `lib/example.ex` and assert the configured fixture's sentinel hover reaches MCP.

```rust
assert!(!output.status.success());
assert!(stderr.contains("elixir"));
assert!(stderr.contains("file_patterns"));
assert!(stderr.contains("workspace"));
// Accepted mapping cases assert the returned hover contains "elixir-fixture".
```

- [ ] Run `i1-t1`; record the unmapped config proceeding instead of rejecting. Validation must occur after merging mappings and built-ins.
- [ ] Add the effective-map membership check only for catch-all/file-tool servers. Exempt an explicitly workspace-only tool set. Do not require server patterns when a workspace mapping already provides routing.
- [ ] Run GREEN and covering config tests, then required checks. Commit `fix(config): reject unroutable file languages`. Sol medium reviews config scope and accepted cases.

### Task 2: Generate an inheriting config template

**Files:** Modify `crates/mcpls-core/src/config/mod.rs`; test `crates/mcpls-cli/tests/cli_integration.rs`. Keep template text in the existing creation module unless an existing template location is available.

**Interfaces:** Consumes first-run config creation and the normal resolver. Produces parseable TOML comments/examples with no active built-in overrides. Existing user files remain unchanged.

- [ ] Write `i1_t2_generated_config_inherits_builtins` through the CLI's first-run path in an isolated user-config directory. Parse the actual created file as `toml::Value`, assert there are no live `lsp_servers` values, and load it through `ServerConfig::load_from`. Compare effective built-ins with current defaults. Also supply an existing explicit override and assert the CLI does not overwrite its bytes.

```rust
assert!(generated.get("lsp_servers").is_none());
assert_eq!(std::fs::read(&existing_path)?, original_bytes);
```

- [ ] Run `i1-t2`; RED is active resolved server entries in the generated file.
- [ ] Replace resolved-default serialization in the creation path with a sparse commented template. Comment examples, including server tables and values. Update tests that currently require active tables to assert inheritance instead. Avoid active default scalar values that would pin later defaults.
- [ ] Run GREEN plus existing default-creation tests and checks. Commit `fix(config): keep generated defaults inherited`. Sol medium reviews.

### Task 3: Finish startup when every applicable server fails

**Files:** Modify `crates/mcpls-core/src/lib.rs`; test `crates/mcpls-core/tests/e2e/protocol_tests.rs`; `tests/e2e/mcp_client.rs` only if task 1 did not already provide the required spawn support.

**Interfaces:** Consumes the existing all-failed startup result and delivery baseline API. Produces a terminal empty baseline after failed routes/expected servers are cleared. No new exported interface.

- [ ] Write `i1_t3_all_failed_startup_flushes_empty`: configure an applicable built-in language with a guaranteed missing absolute command, initialize through MCP, then poll `get_new_diagnostics` with a bounded deadline. Parse its content JSON as the existing protocol-only test does.

```rust
assert!(payload.get("note").is_none());
assert_eq!(payload["changed"].as_array().map(Vec::len), Some(0));
assert_eq!(payload["cleared"].as_array().map(Vec::len), Some(0));
```

- [ ] Run `i1-t3`; RED is the bounded poll expiring on repeated startup notes, after spawn failure is confirmed.
- [ ] Set the empty baseline in `result.all_failed()` after route rebind and expected-server cleanup, using the existing zero-applicable path's baseline operation.
- [ ] Rebuild and run GREEN plus the existing protocol-only and all-failed transport tests. Commit `fix(diagnostics): settle failed server startup`. Sol medium reviews.

### Task 4: Select floors from applicable configurations

**Files:** Modify `crates/mcpls-core/src/lib.rs`; test `crates/mcpls-core/tests/e2e/protocol_tests.rs`; extend `tests/e2e/diagnostics_fixture.rs` if needed. Read the mutually-exclusive configuration fixture referenced by `tests/integration/basic_tests.rs`.

**Interfaces:** Consumes the same `applicable_configs` used by `ToolRouter`. Produces `FloorTable` from that subset before startup consumes it. FloorTable's API and ServerId representation stay intact.

- [ ] Write `i1_t4_inactive_duplicate_cannot_choose_floor`, parameterized over both declaration orders. Use mutually exclusive workspace markers with duplicate `python` IDs, active floor `warning`, inactive floor `error`. Wait for baseline, then have the active LSP publish a unique warning and call MCP `get_new_diagnostics`.

```rust
assert!(report.to_string().contains("active-warning"));
assert!(!report.to_string().contains("inactive-fixture"));
```

- [ ] Run `i1-t4`; RED is the warning disappearing in the order where the inactive entry overrides its floor. The other order is the control.
- [ ] Build the floor table from `applicable_configs` at setup. Keep applicability evaluation single-sourced; do not rerun heuristics independently for floors.
- [ ] Rebuild and run GREEN plus mutually-exclusive routing/config tests. Commit `fix(diagnostics): use applicable server floors`. Sol medium reviews.

### Task 5: Preserve silent owners' startup grace

**Files:** Modify `crates/mcpls-core/src/bridge/settle.rs` and `crates/mcpls-core/src/lib.rs`; test `crates/mcpls-core/tests/e2e/protocol_tests.rs`; extend `tests/e2e/diagnostics_fixture.rs`.

**Interfaces:** Add `ServerSettle::set_diagnostics_owners(&self, owners: impl IntoIterator<Item = ServerId>)`. The startup caller supplies only successfully registered diagnostics owners from `registered.diagnostics_flags`, before `restart_deadline()`. Preserve `progress_epoch() -> u64`, `begin`, `end_at`, and footer snapshot behavior for task 9. Registration must retain progress already received during initialization. Server retirement clears only that server's outstanding progress and owner state; it must not reset another owner or any adopted session baseline. Extend the existing retirement assertions when introducing owner state.

- [ ] Write `i1_t5_mixed_owner_startup_uses_each_grace`. Start two applicable diagnostics owners through MCP. Reporting owner begins/ends progress promptly; silent owner publishes a named startup diagnostic after the normal quiet interval but before two seconds, for example at 1.5 seconds. Poll for baseline and assert the startup diagnostic is suppressed. Then emit a changed diagnostic and assert it is delivered, proving publication/routing works.

```rust
assert!(!baseline_flush.to_string().contains("silent-startup"));
assert!(after_change.to_string().contains("silent-after-startup"));
```

- [ ] Run `i1-t5`; RED is `silent-startup` arriving as a new diagnostic after premature adoption. The fixture records both initialization and publish acknowledgements to distinguish setup delay from the bug.
- [ ] Track progress-seen/quiet state per active diagnostics owner. Require all such owners to meet their own grace or progress-end quiet interval, with the existing global deadline as backstop. Preserve the global epoch/footer observations. Failed and non-diagnostics servers must not introduce waits. Add focused deterministic tracker tests for the exact boundary, pre-registration progress and no-owner behavior.
- [ ] Rebuild and run GREEN plus settle/footer helper tests. Commit `fix(diagnostics): preserve each owner startup grace`. Astra medium reviews tracking, pump ordering and footer compatibility.

### Task 6: Give stdio processes independent fallback sessions

**Files:** Modify `crates/mcpls-core/src/bridge/delivery.rs`; test `crates/mcpls-core/tests/e2e/protocol_tests.rs`; support `tests/e2e/mcp_client.rs` and `tests/e2e/diagnostics_fixture.rs`. Change MCP session construction only if needed to reuse the same token consistently.

**Interfaces:** Keep `SessionId::from_env_or_process()` and nonempty exported IDs unchanged. A process-local `OnceLock` stores one unique fallback token; local and forwarded delivery use it. Use existing identity/random facilities, or PID plus process-start nonce without a dependency. PID alone is insufficient for reused process IDs while owner records survive.

- [ ] Write `i1_t6_process_fallbacks_are_independent`. Launch two real stdio MCP children in the same canonical workspace with hooks enabled and unset session environment. Verify owner/passive readiness before mutation. Adopt baseline, publish a unique diagnostic on the owner, then flush each client. Both receive it once; repeated flushes from each omit it. Repeat with the first draining client reversed and with an empty environment value. Keep a nonempty explicit same-ID control showing deliberate shared dedup.

```rust
assert!(first_report.to_string().contains("process-session-probe"));
assert!(second_report.to_string().contains("process-session-probe"));
assert!(!first_repeat.to_string().contains("process-session-probe"));
assert!(!second_repeat.to_string().contains("process-session-probe"));
```

- [ ] Run `i1-t6`; RED is the second process missing its first delivery. Separate-session constructor unit tests alone do not meet this gate.
- [ ] Replace the literal `local` fallback with the stable process token. Update the old literal assertion to the process identity invariant.
- [ ] Rebuild and run GREEN plus explicit-session/ACK tests. Commit `fix(diagnostics): isolate fallback process sessions`. Astra medium reviews owner/passive behavior and environment isolation.

### Task 7: Leave sweep capacity for ordinary requests

**Files:** Modify `crates/mcpls-core/src/hooks/sweep.rs`; test/fixture changes in `crates/mcpls-core/src/hooks/service.rs`. Modify the existing shortfall rendering location if it currently claims a hard ceiling was reached; root must claim that exact file before dispatch expands scope.

**Interfaces:** Keep `Sweeper` and request types unchanged. Background admission is unbounded when `max_documents == 0`; otherwise available new background opens are `max_documents.saturating_sub(open_count).saturating_sub(1)`. Resync already-open documents independently of new-open headroom.

- [ ] Extend `HookHarness` and its real `build_handler`/socket request path for `i1_t7_sweep_reserves_tool_capacity`. Queue more external routable files than fit using Changed requests, then issue Flush and ACK. With a finite ceiling of 3, assert at most 2 are background-opened and a subsequent unrelated MCP file tool opens the third successfully. Observe the fixture's didOpen/didSave, rather than only the state count.
- [ ] Add `i1_t7_zero_limit_sweeps_unbounded` with the same Changed/Flush path and limit zero. Assert actual opens/saves occur and no zero-limit headroom failure is reported. Include a pre-opened document to ensure background resync is retained.

```rust
assert!(open_count < 3);
assert!(!unrelated_tool_response.is_error.unwrap_or(false));
assert!(lsp_notifications.iter().any(|n| n["method"] == "textDocument/didSave"));
```

- [ ] Run `i1-t7`; RED is ordinary request rejection at the finite ceiling and absent background opens for zero.
- [ ] Correct admission and shortfall wording to name background headroom/skipped coverage. Keep latest-sweep status visible until another sweep replaces it; do not introduce per-session shortfall consumption.
- [ ] Run GREEN plus existing sweep and shortfall tests. Commit `fix(hooks): reserve document sweep headroom`. Astra medium reviews admission and actual-request proof.

### Task 8: Demote an owner that loses a missing-lock race

**Files:** Modify/test `crates/mcpls-core/src/hooks/service.rs`; test `crates/mcpls-core/src/mcp/server.rs` only if needed to exercise the existing write handler through the service fixture. Use `tests/hooks_socket.rs` only if the race requires extending its existing listener fixture.

**Interfaces:** Keep `HookRole` and listener request types. `LockLost(Missing)` demotes before waiting for acquisition; only successful acquisition promotes. Uncontended reacquisition still eventually returns to Owner.

- [ ] Write `i1_t8_missing_lock_loser_forwards` using the existing disappearance/replacement and `HookHarness` fixtures. Remove the old lock, ensure a competitor acquires the new lock before the old acquisition attempt can win, then allow the old task to continue. Use a test-controlled synchronization point if necessary; do not rely on a repeated probabilistic race.
- [ ] Assert the old process becomes Passive, its real MCP flush receives the competitor's sentinel rather than its own local sentinel, and an actual applied write forwards Changed targets while emitting no local footer. Assert the competitor's request/sweep counter changes. Update the uncontended disappearance test to require eventual reacquisition rather than no transient role change.

```rust
assert!(matches!(old_role.get(), Role::Passive { .. }));
assert!(forwarded_report.contains("competitor-diagnostic"));
assert!(!forwarded_report.contains("former-owner-diagnostic"));
```

- [ ] Run `i1-t8`; RED is the loser retaining Owner/local delivery after the competitor demonstrably owns the lock.
- [ ] Demote on both Missing and Replaced lock loss before the common acquisition loop. Preserve cancellation, promotion baseline handling and socket identity checks. Any test synchronization is test-only.
- [ ] Run GREEN plus uncontended reacquisition, takeover and passive-footer tests. Commit `fix(hooks): demote after ownership lock loss`. Astra medium reviews interleavings and promotion invariants.

### Task 9: Capture write progress before apply starts

**Files:** Modify/test `crates/mcpls-core/src/mcp/server.rs` and `crates/mcpls-core/src/hooks/service.rs`; test `crates/mcpls-core/tests/e2e/protocol_tests.rs`; extend `tests/e2e/diagnostics_fixture.rs`. The service fixture calls `footer_if_written` and requires the new epoch argument. Existing translator resync behavior remains unchanged unless a test fixture needs an explicit test-only barrier.

**Interfaces:** Change internal helpers to `footer_if_written(&self, applied: bool, epoch_before: u64) -> Option<NewDiagnosticsResult>` and `footer_for_write(&self, epoch_before: u64) -> Option<NewDiagnosticsResult>`. Each write tool captures `self.context.settle.progress_epoch()` before invoking its translator operation and passes it through. The footer wait clock starts after apply/resync completes.

- [ ] Write `i1_t9_progress_during_resync_reaches_footer` using real MCP applied-write requests. The LSP fixture begins progress after accepting the write notification while a later resync operation is held open, proves the begin was observed before translator completion, then ends progress and publishes a named diagnostic after the footer grace. Assert the response waits through end plus quiet and contains that diagnostic. A begin sent only after the translator returns fails to reproduce this bug.
- [ ] Exercise rename, format and apply-code-action entry points with their normal serialized MCP arguments. Reuse the existing footer timing controls. Keep controls for pre-existing progress, apply=false, default-off, passive mode, baseline absent and a never-ending progress cap; reuse existing coverage when only helper signatures change.

```rust
let epoch_before = self.context.settle.progress_epoch();
// Existing translator operation and forwarding remain between these statements.
let footer = self.footer_if_written(applied, epoch_before).await;
```

- [ ] Run `i1-t9`; RED is a response after grace but before the controlled progress end/diagnostic. Require the LSP ordering proof in the failure log so scheduler luck cannot count as RED.
- [ ] Move epoch capture into all write handlers and thread the argument through both helpers. Keep grace/cap values and wait start after apply. Update direct helper callers with explicit pre-write epochs.
- [ ] Rebuild and run GREEN plus covering footer tests, including `hooks::service::tests::test_a_passive_instance_runs_no_footer_but_still_reports_its_writes`. Root adds that existing test to the focused covering selector. Commit `fix(diagnostics): capture progress before writes`. Astra medium reviews all write paths and the causal test boundary.

### Task 10: Close real-RA evidence and reconcile design claims

**Files:** Modify `crates/mcpls-core/tests/ra_e2e.rs`; `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`; create `docs/superpowers/notes/2026-09-10-diagnostics-completion-validation.md`. Update an existing user-facing diagnostics document only where it repeats an altered invariant, after root names and claims it.

**Interfaces:** Reuse `sc_resync_delivers_a_build_error_after_an_apply`, `find_rustc_e0428`, and the existing MCP client. No source behavior change or extra real-RA fixture. Preserve the completed dated Stage A and Stage B/C plans as historical snapshots; reconcile the current design and new validation note only, plus any specifically approved user-facing invariant.

- [ ] Immediately after the existing verified rustc E0428 hit, issue one follow-up `get_new_diagnostics`, parse with the existing assertion helper, and require that the same file/source/code/message/range diagnostic is absent. Other unrelated new diagnostics may still arrive.

```rust
let next = client.call_tool("get_new_diagnostics", &json!({}))
    .map_err(|e| format!("follow-up flush failed: {e}"))?;
let text = assertions::assert_tool_ok(&next);
let next_report: Value = serde_json::from_str(&text)
    .map_err(|e| format!("bad follow-up JSON: {e}"))?;
assert_ne!(find_rustc_e0428(&next_report).as_ref(), Some(&hit));
```

- [ ] Run the focused `i1-ra` task with a verified matching scenario filter and built worktree binary. Record real rust-analyzer/rustc execution and non-repetition. This closes missing evidence; it may pass on the existing implementation, so do not invent a production defect or require a fake RED.
- [ ] Update design/deferred prose to the verified final invariants: sparse inherited defaults, novel language mapping requirement, applicable floors, terminal failed startup baseline, process fallback identity, silent-owner grace, reserved background capacity, demotion after lock loss and pre-write epoch/post-write wait timing. Record shortfall as latest-sweep status cleared/replaced by the next sweep. Preserve accepted already-proven behaviors and qualify broad watcher benefits by tested server.
- [ ] Write the validation snapshot with each audit finding mapped to its task commit and real RED/GREEN evidence. Root supplies integrated `verify`/`docs` results and final review status. State original artifact equivalence as pending issue #7 and live host/cross-user checks as pending issue #6; include any manual Windows gap. Existing source/config completion must not imply those external checks passed.
- [ ] Commit test evidence separately from timeless design reconciliation if needed: `test(diagnostics): prove real rustc deduplication` and `docs(diagnostics): reconcile completion evidence`. Sol medium reviews evidence wording; root schedules one Astra medium whole-branch review.

## Completion and unresolved questions

All task reports must include full BASE/HEAD, owned files, command/log paths, selected assertions, status and self-review under the dispatch contract. Root accepts both spec-compliance and code-quality review verdicts before proceeding. Root reconciles the final branch ledger and handles ready stacked PRs; workers do not push or merge.

No user design decisions remain. Root preflight must add the focused devrun selectors, verify nonempty selection and package task 1 with its exact file claims. Test synchronization mechanics can be chosen by the implementer within the named fixture files; a reliable causal ordering assertion is mandatory, not an unresolved product choice.
