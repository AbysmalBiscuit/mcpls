# Diagnostics recovery validation

This is the 2026-09-08 recovery snapshot. See the [independent Linux validation](2026-09-09-diagnostics-validation.md) for subsequent findings, reruns, and limits on the evidence available on the validation machine.

Snapshot: 2026-09-08. The functional follow-up is restored in `/home/lev/Git/lev/mcpls_worktrees/diagnostics-recovery`, branch `diagnostics-recovery`, based on `5f6fcc1137824f372f6290860cfbf81b8aba9936`. Devkit `issue setup` created this persistent worktree. At initial validation, nothing was committed, merged, pushed, or installed into the daily-driver plugin. Lev subsequently authorized committing and pushing the recovery branch.

## Review this recovery

Review the diff against `5f6fcc1`, not main. The original diagnostics branch is the prerequisite and remains untouched. Treat the gates below as unverified, not as issue closure.

Recovery source: Codex session `01a07df0-5a6c-73c1-afb7-a34cf1b734a2`, rollout `/home/lev/.codex/sessions/2026/09/08/rollout-2026-09-08T00-14-58-01a07df0-5a6c-73c1-afb7-a34cf1b734a2.jsonl`. Only records before 2026-09-08T00:00:00Z were considered, excluding this recovery's own appended records.

JavaScript AST extraction recovered literal patches without executing commands from the transcript. Failed calls were excluded. The computed removal of obsolete overrun tests and the fixture pipe-name substitutions were reconstructed separately. Historical sabotage operations were not replayed as fixes.

This is not a byte-for-byte replay of the lost diff. Broad comment-only rewrites were omitted, retaining existing ownership, shutdown, routing, and MCP API documentation. Local fixture narration and descriptions made inaccurate by the fixes were shortened. In particular, `mcp/server.rs` has only its canonical-path test correction, not the earlier comment deletion sweep.

## Scope and decisions

- Host output: `SessionStart`, `UserPromptSubmit`, and `PostToolBatch` output carries the matching `hookSpecificOutput.hookEventName`. CLI tests send real payloads and check the serialized response. These tests check the required event fields; they do not execute Claude Code's parser or prove live host consumption.
- #3: `watch_paths` retains traversal and ignore-rule errors alongside partial paths. A file used as the root is rejected explicitly, including on Windows. SessionStart emits a non-blocking `systemMessage`; doctor distinguishes empty, selected, and incomplete scans and explicitly leaves host registration unverified. Hidden top-level entries remain excluded.
- #4: hook clients remain silent on owner deadline responses. The unused per-operation delivery promises and their six string-only tests are removed. A generic error reports the configured duration and says work continues in the background. The real socket test checks its numeric value and text; detached completion remains tested without claiming a later flush delivered anything.
- #5: CLI child-process tests pin `runtime_dir -> current_user` and `identity_for -> ensure_socket_path_fits`. No production identity changes remain. Child environment configuration preserves the workspace's `unsafe_code = "deny"` constraint. Accepted and ruled-out digest entries are not treated as pending fixes.
- #6: native Windows Clippy is added to CI. Tests use isolated valid pipe names, real pipe fixtures for capped namespace scans, portable paths and URIs, and a same-volume relative-PATH fixture. New tests exercise concurrent clients, isolated sorted pipe enumeration, and busy-pipe recovery. Doctor labels a PATH hit as a candidate whose launch was not checked.

The permission test only asserts unreadability when the runner actually lacks access; a privileged reader is not expected to fail. Missing-root, file-root, and ignore-directory cases provide deterministic error fixtures.

## Fresh verification

| Check | Result |
| --- | --- |
| Linux full suite after mutation restoration | 1,136 passed, 40 skipped |
| Native Windows full suite after mutation restoration | 1,096 passed, 38 skipped |
| Linux Clippy, all workspace targets and features, warnings denied | Passed |
| Native Windows Clippy, all workspace targets and features, warnings denied | Passed |
| Nightly formatting check | Passed |
| Fresh mutation checks | 11 mutations, all targeted tests failed and then passed after restoration |
| Production identity file after audit | Identical to base |

Linux final run: `c70ff437-8dfc-4aca-a864-98390f32855a`.
Windows final run: `12a95b76-b04b-4b2c-a092-5b34661b1cef`.

Before restoring production code, the recovered CLI regression selection ran against the base implementation: 7 failed for missing event fields or warnings, and the 2 identity-wiring tests passed. Run: `650f3e7a-8af3-4796-9bf4-a4690ad43a74`. Their missing coverage was then proved through the call-site mutations below.

Independent mutations were grouped to avoid rebuilding for every test. Every target test in each batch failed on its corresponding behavioral assertion, not a compiler error. Each batch was restored in a `finally` block and the identical selection passed afterward. The original session's 18 checks and stress/kill experiments are historical evidence, not claimed as rerun here.

### contracts

- SessionStart event contract: `test(test_hook_session_start_emits_absolute_watch_paths)`
- Context hook event contract: `test(test_hook_context_outputs_name_the_triggering_event_through_cli)`
- Deadline numeric value: `test(test_an_op_answers_within_its_deadline_while_its_work_runs_on)`
- Current-user runtime wiring: `test(test_doctor_identity_uses_current_user_without_xdg_runtime_dir)`
- PATH launch uncertainty: `test(test_doctor_does_not_claim_a_path_candidate_can_launch)`

RED: `edb92945-17ff-4b66-a757-2f5788f16323`, exit 100. Restored GREEN: `9aedbb85-ec40-48da-9931-980372d80f49`, exit 0.

### traversal-and-wiring

- Traversal errors retained: `test(test_watch_scan_distinguishes_missing_and_empty_roots_through_cli)`
- Deadline generic prose: `test(test_an_op_answers_within_its_deadline_while_its_work_runs_on)`
- Socket length identity wiring: `test(test_doctor_identity_rejects_socket_path_that_cannot_bind)`
- PostToolBatch event value: `test(test_hook_context_outputs_name_the_triggering_event_through_cli)`

RED: `c8c9b9f4-4a8e-4ed5-84ee-8edf9b973b08`, exit 100. Restored GREEN: `a2252e33-8f8e-4281-8cb4-fa0262255d6a`, exit 0.

### windows-runtime

- Windows file-root rejection: `test(test_watch_scan_rejects_a_file_as_project_root)`
- Windows busy-pipe classification: `test(test_doctor_reports_a_saturated_pipe_as_busy_then_recovers)`

RED: `9e36ee72-4015-49d4-b87a-118da122e60f`, exit 100. Restored GREEN: `ec9f9d26-9968-411b-9472-7f0f55b448c5`, exit 0.

## Reproduce checks

Devkit reports no canned tasks for this checkout. Linux checks:

```fish
cargo nextest run --manifest-path /home/lev/Git/lev/mcpls_worktrees/diagnostics-recovery/Cargo.toml --target-dir /home/lev/Git/lev/mcpls/target/diagnostics-followup --workspace --all-features --status-level fail
cargo clippy --manifest-path /home/lev/Git/lev/mcpls_worktrees/diagnostics-recovery/Cargo.toml --target-dir /home/lev/Git/lev/mcpls/target/diagnostics-followup --workspace --all-targets --all-features -- -D warnings
cargo +nightly fmt --manifest-path /home/lev/Git/lev/mcpls_worktrees/diagnostics-recovery/Cargo.toml --all -- --check
git -C /home/lev/Git/lev/mcpls_worktrees/diagnostics-recovery diff --check
```

Native Windows ran through PowerShell using the UNC manifest path and an isolated generated target directory. Exact commands, mutation snippets, and captured RED/GREEN output are in `/home/lev/Git/lev/mcpls_worktrees/diagnostics-recovery-evidence.json`.

A standalone diff, including this note, is saved at `/home/lev/Git/lev/mcpls_worktrees/diagnostics-recovery.patch`. Apply it only to a clean worktree at `5f6fcc1`, after `git apply --check`. It is outside both `/tmp` and the generated build cache.

## Reconciliation and pre-push review

The original final numstat is reproduced exactly: 14 tracked files, +814/-1200. Recovery before this note is +691/-387 in the same files. Every differing line between the reconstructed original and recovery is a line comment or blank line. No executable code, test, workflow, or other document content is missing from the recovered tracked changes.

| File | Lost original vs base | Recovery vs base |
| --- | --- | --- |
| `crates/mcpls-cli/src/hook.rs` | +242/-259 | +226/-121 |
| `crates/mcpls-core/src/hooks/filters.rs` | +75/-64 | +72/-34 |
| `crates/mcpls-core/src/hooks/listener.rs` | +36/-358 | +8/-129 |
| `crates/mcpls-core/src/bridge/apply/plan.rs` | +9/-22 | +3/-2 |
| `crates/mcpls-core/src/bridge/translator/edits.rs` | +30/-106 | +6/-4 |
| `crates/mcpls-core/src/lsp/watched_files.rs` | +8/-43 | +2/-1 |
| `crates/mcpls-core/src/mcp/server.rs` | +41/-253 | +1/-1 |

The other seven files are byte-identical to the reconstructed originals, including all of `cli_integration.rs` and `.github/workflows/ci.yml`. The direct original-to-recovery diff is +815/-125; its 690-line net increase matches the aggregate gap. Subtracting the two base-relative numstats gives different gross counts because Git aligns the hunks differently.

Reconstruction applied the successful literal patches to `5f6fcc1` in memory, rebuilt the obsolete-test removal and Windows fixture substitutions, and applied the three omitted computed comment passes from the log. It parsed literal JavaScript objects with Acorn rather than executing transcript commands. The second and third comment passes used the exact old comment blocks printed in the original tool results. Recorded formatting boundaries were reproduced. Failed calls, validation-note generation, and temporary mutate/restore operations were excluded.

Trace anchors in the original rollout:

- `call_s7UBjfZlwFeDQ9t3naRFvX4f`: prefix-selected comment replacements.
- `call_ilsD3csnk6Hdkpfk1yWXFvfO`: long-comment replacements, using blocks printed by `call_B17pAR4BsNVLIhb1WWe3IqOw`.
- `call_u9pqwpWKG9gQfs6bQHn0MUkK`: remaining MCP comments, using blocks printed by `call_YyFVn6cP6M1XUBKxczFJwgbT`.
- `call_xoIatqFXJkOUV8gRzqrpDIZH`: final original per-file numstat.

The external evidence file `/home/lev/Git/lev/mcpls_worktrees/diagnostics-recovery-reconciliation.json` records per-file original blob hashes, both numstats, comment-edit counts, and zero non-comment differences. Compare each reconstructed blob against `5f6fcc1:<file>` with `git diff --numstat`, then against the recovered file to inspect the omitted comments. The original tracked source is reconstructible; the lost validation note and historical experiments are not silently treated as fresh evidence.

Codex read all of `cli_integration.rs`, `hook.rs`, and `ci.yml` during this pre-push audit. No new blocking defect was identified. This is an author review, not an independent second reviewer. The review checked the CLI entry points and child environment setup for both #5 tests, hook event serialization, doctor output branches, per-fixture Windows pipe names, busy-pipe release/reacquisition, and the Windows Clippy matrix with its existing downstream CI gate. A branch-only push does not trigger this workflow: its triggers are pushes to main and pull requests. Local native Windows results remain the evidence until a PR runs CI.

## Remaining gates

#6 is not complete. Automated native execution does not prove:

- Live Claude Code plugin wiring: matching doctor roots/hashes and a live owner PID, actual watch registration, an external edit, and diagnostics reaching the next model turn.
- Two real Windows accounts: distinct identities, pipe DACL behavior, and exclusion of another account's owners from doctor results. Changing USERNAME in a child fixture is not a security test.
- Real-project drive-case and 8.3 short-name agreement.
- A real pipe-namespace enumeration failure, an actually poisoned transport mutex, or exhaustion of the platform's instance cap.

The killed-test cleanup and repeated Windows stress experiment from the lost session were not rerun in this recovery. The native full suite and explicit busy/file-root mutations above were rerun.

Integration still needs Lev's decision. Committing and pushing the recovery branch are authorized separately. Keep the worktree and its external patch until the changes have been reviewed and preserved in Git; do not merge, install, or close issues as part of this handoff.
