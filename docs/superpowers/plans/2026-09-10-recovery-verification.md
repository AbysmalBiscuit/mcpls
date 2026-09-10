# Diagnostics recovery verification

> **For agentic workers:** Execute this plan with superpowers:subagent-driven-development and test-driven-development. Each task receives a fresh implementer and independent review. Lev reviews and merges the resulting ready PR.

**Goal:** Complete the remaining diagnostics recovery behavior and reconcile issue #7 against verifiable evidence.

**Spec:** https://github.com/AbysmalBiscuit/mcpls/issues/7, its linked original recovery requirements, and the remaining-work sections of merged PRs #9 and #10. The original comparison base is `5f6fcc1137824f372f6290860cfbf81b8aba9936`. This implementation starts from the diagnostic-source fix in PR #14.

**Design:** Startup and replacement LSP processes use the same owned notification-pump lifecycle. Writes synchronize affected source documents as a batch before sending save notifications, including files the client had not opened. Preserve diagnostic ownership, session delivery, cancellation recovery, document limits, and watch notifications. Do not infer correctness from notification timing alone.

**Global Constraints**

- Work only in `/home/lev/Git/lev/mcpls_worktrees/recovery-verification`, branch `lev/recovery-verification`; do not change the user's main checkout.
- Claim files with lockm. Use devrun for builds, tests, formatting, and checks. Do not run sudo, alter RUSTC_WRAPPER/zccache settings, or read CONTRIBUTING.md.
- Use the isolated `.devkit/test.env` and `.devkit/validation.toml` supplied by the controller. Keep the production quiet debounce in external-server tests. Do not install into the user's active plugin or binary channels.
- Begin every behavior change with an assertion failure through the actual MCP entry point and framed LSP transport. Add focused race tests only for concrete concurrency requirements.
- Keep one source implementer active at a time. No nested delegation. Commit logical changes selectively with conventional commit subjects after self-review and relevant checks.
- Current-version upstream rust-analyzer behavior is not established by master source alone. A deterministic protocol test establishes our synchronization contract; the real installed server test establishes resulting compiler diagnostics.
- Historical claims require original evidence. Missing artifacts must be recorded as unavailable, not called equivalent. Native GitHub Windows CI may supply automated platform evidence; manual Windows and real Claude plugin-host gates remain in issue #6.
- A task report must include precise RED/GREEN commands, failing assertions, passing summaries, log paths, commit, changed files, and limitations. Task reviews use these artifacts; no duplicate broad test runs without a concrete concern.

## Task 1: Restore notifications across server replacement

**Files:** `crates/mcpls-core/src/lib.rs`, a focused crate-private notification lifecycle module if necessary, `crates/mcpls-core/src/bridge/translator/{mod,respawn}.rs`, `crates/mcpls-core/src/bridge/settle.rs`, and focused lifecycle/MCP test fixtures. Update CHANGELOG.md only for this behavior.

1. Add a controlled framed LSP fixture under existing test conventions. Use actual background server registration and `McplsServer::serve` over a duplex MCP connection. Complete initialize, notifications/initialized, and tools/call. Each process generation publishes a distinct diagnostic and log sentinel, answers diagnostic pulls with an empty full result, and exits on an explicit fixture control event. A fixture must not satisfy the cache assertion itself.
2. Establish startup baseline, crash generation one, and request diagnostics through MCP to trigger respawn. Observe replacement push diagnostics and log messages through public MCP tools, repeat another replacement, and confirm get_new_diagnostics delivers a new diagnostic once. Preserve an unrelated server's diagnostic. Run before implementation and retain the expected failing assertions.
3. Replace the discard receiver with the existing production diagnostics pump. Share its dependencies without duplicating notification processing. Retain per-server task ownership; abort and await the old task before invalidating its diagnostics and installing a new generation. A cancelled retirement must retain the join handle so a retry still waits for old processing to finish. Retire dead progress tokens only for the replaced server. Keep the original session baseline and all other servers' state.
4. Startup must establish pump ownership before a routable client can respawn. The replacement's pump and client/server publication must have no intervening await after retirement/invalidation. Ensure shutdown and owner drop terminate current pumps; do not detach tasks or accidentally keep the entire session alive through an Arc cycle.
5. Cover a queued old-generation publish competing with retirement and cancellation/retry at the termination barrier. These focused cases may exercise the controller internally; the main regression remains an actual MCP request. Preserve router cache gates, workspace/version filtering, subscriptions, logs/messages, and progress processing by reusing diagnostics_pump.
6. Run the focused respawn/recovery tests, then devrun verify once. Review the diff, commit, and write `.superpowers/sdd/2026-09-10-recovery-verification/task-1-report.md`.

**Commands:** `devrun -C /home/lev/Git/lev/mcpls_worktrees/recovery-verification --config /home/lev/Git/lev/mcpls_worktrees/recovery-verification/.devkit/validation.toml task issue7-recovery --env-file /home/lev/Git/lev/mcpls_worktrees/recovery-verification/.devkit/test.env`; use `task verify` for the full verification sequence.

## Task 2: Synchronize unopened write targets before saves

**Files:** `crates/mcpls-core/src/bridge/translator/{mod,edits}.rs`, document state/apply queue code only if needed, MCP protocol fixtures, `crates/mcpls-core/tests/ra_e2e.rs` and its focused fixture module, CHANGELOG.md.

1. Through actual MCP rename_symbol with apply=true, make ordered documentChanges touch an already-open anchor and an unopened routed source file. Verify the fixture saw the latter as unopened when rename was requested. Assert that all resulting opens/changes arrive with the written text before the first save. Include watched-file registration. Capture RED for the synchronization failure before implementation.
2. Add a real rust-analyzer case using a dedicated module untouched by previous queries. Give the module same-signature functions tally and total and call module::tally from an already-open lib.rs. Rename from the caller to total to produce compiler E0428 in the unopened module. Assert both paths were written and the collision exists on disk. Poll only get_new_diagnostics, requiring the exact module URI, rustc source, and E0428 code; never open/query that module to help the test pass. Preserve hooks disabled and production debounce. Record pre-fix behavior honestly even if scheduler timing lets this external-server case pass; the deterministic protocol RED remains mandatory.
3. Split invalidation drain into content synchronization followed by saves. Use existing routing/tracker mechanisms to open affected untracked source documents. Finish the batch's relevant opens/changes before saves, retain newly opened documents, and preserve watch notifications for registered consumers including non-routable paths.
4. Preserve cancellation and partial-failure obligations across phases. A path remains pending until required saves complete; recheck version/server generation under its path lock before recording a save. Respect document limits and surface or retain incomplete synchronization instead of silently treating watched-only delivery as success. Keep per-document/per-server saves; do not assume one arbitrary save starts a whole-workspace build.
5. Add a bounded deterministic interruption between content and saves, then retry the drain and assert the required save still arrives with correct content/version. Cover only concrete new boundary failures, reusing existing fixtures.
6. Run focused resync/rename tests and the real installed rust-analyzer regression with the freshly built binary. Run devrun verify once, review the diff, commit, and write task-2-report.md.

**Commands:** use `.devkit/validation.toml` task `issue7-resync`, `build`, `issue7-ra`, and `verify`, always with `.devkit/test.env`.

## Task 3: Reconcile recovery evidence and remaining documentation

**Files:** a dated evidence note in `docs/superpowers/notes/`, `crates/mcpls-cli/src/hook.rs` only for the missing timeout invariant if still valid, and other documentation only where the audit proves a mismatch.

1. Consume the controller's `.devkit/issue7-audit.md`, original issue summary, and historical audit. Reconcile available original hunks and deleted test counterparts against the original base. Explicitly identify evidence that cannot be reconstructed; do not assert byte-equivalence or that unobserved deletions were comments.
2. Verify the previously unreviewed CLI integration/hook sections, hidden-path documentation versus ignore-walker behavior, and server runtime/socket identity wiring. Audit each of the seven mutation requirements against actual evidence. If prior evidence is insufficient, the controller provides isolated mutation tasks: mutate one line, require the named test's own assertion failure, restore with a trap, and verify the restored named test. Never leave production mutated or change unrelated worktrees.
3. Use native GitHub Windows job logs to confirm concurrent-client, drive-prefix, and pipe-isolation tests actually ran. Check the workflow's Windows Clippy step really executes. Leave manual Windows/plugin-host checks in issue #6 with explicit links.
4. Restore the short FLUSH_SOCKET_TIMEOUT/default-server-operation-deadline invariant comment if the audit confirms it. Do not copy a long historical explanation or add tests for comment restoration. Do not conflate the existing default-bound test with configurable deadline support.
5. Write a durable dated evidence note linking commits, exact CI run/job artifacts, behavioral tests, and any unavailable history. Snapshot counts are allowed here; include reproduction commands for evidence that can be regenerated. Update the issue checklist/project status only after current requirements are accounted for; the PR closes #7 upon Lev's merge.
6. Run formatting and git diff --check for any final source/document edit, commit selectively, and write task-3-report.md. The controller performs final whole-branch review, remaining external-server/strict-doc verification where needed, ready PR creation, and native CI verification before beginning issue #1.

## Unresolved questions

No implementation decision requires user input. Whether the missing original rollout can be recovered is an evidence question, not permission to claim equivalence. Preserve that limitation explicitly if the read-only search cannot resolve it.
