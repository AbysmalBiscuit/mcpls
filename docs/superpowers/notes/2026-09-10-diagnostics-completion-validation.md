# Diagnostics completion validation

Snapshot date: 2026-09-11. This note records the final Task10 evidence for issue #1 and the accepted Task1-9 integration state. It is a validation snapshot, so commit IDs, command lines, timings, counts, and paths are intentionally exact.

Task10 started from the pre-Task10 integration head `fc3db253fde9bd515a93ba9bce3d275e98da4c71` and rebased the branch onto `origin/main` at `d119e9b4f70de7b7e1473d49ae53672da3ac6fcb`. The rebased pre-Task10 source head is `0b35fdc4986b2fee2c8b1bae5dd902996ef5f314`; the final source head used by the focused Task10 checks before this snapshot commit is `1df27bebc07ed78ee098f4db0954d294825401e8`. The final branch head includes this snapshot and the footer wording correction; its exact SHA is recorded in `.devkit/task10-report.md`.

## Reproducible validation boundary

The local validation configuration is ignored, so the task name alone is insufficient to reproduce the local-only runs. The common command boundary was:

```text
devrun -C /home/lev/Git/lev/mcpls_worktrees/diagnostics-integration --config /home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/validation.toml task NAME --env-file /home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/test.env
```

The relevant environment loaded from `test.env` was `CARGO_BUILD_JOBS=1`, `TMPDIR=/var/tmp/mcpls-i1-int-6b8x914p`, `RUSTDOCFLAGS=-D warnings`, and `MCPLS_E2E_BINARY=/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/target/debug/mcpls`. The selected RA task also set `MCPLS_RA_FILTER=resync_delivers_a_build_error_after_an_apply`. The trace run used the prepared `/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/ra-debug.env`, which adds `MCPLS_LOG=info,mcpls_core=debug,mcpls_core::lsp::transport=trace`; the normal `test.env` was unchanged.

The expanded local-only commands printed by the logs were:

```text
cargo nextest run -p mcpls-core --test ra_e2e --all-features --locked --run-ignored all -E 'test(ra_e2e_suite)' --success-output immediate
cargo nextest run -p mcpls-core -p mcpls --all-features --locked --run-ignored all -E 'test(i1_t)'
```

The first command is the `i1-ra` task and the second is the `i1-regressions` task. The `i1-regressions` task includes ignored MCP process cases; `i1-ra` enables the ignored real-server suite. Both use the worktree binary path from `MCPLS_E2E_BINARY`.

## Integrated source evidence

The historical integration mapping is preserved in [integration-report.md](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/integration-report.md). Its integrated IDs predate the rebase and remain historical evidence-stage identifiers, not current ancestors. The table below pairs each historical integrated ID with its current rebased ID.

| Stage | Historical integrated ID | Current rebased ID |
| --- | --- | --- |
| Plan | `1640256b1b96d09f363017cbd1146fdbc9f953a4` | `3324578e1ad6f20f4ae1f380c30176a1b327bb15` |
| Plan command correction | `0976c78c8cd6394eaa7b1f4b6d4e47e132d9e5dd` | `f595abc0db62a2f54dbb27eb31b50c6527552937` |
| Task1 routing validation | `218bf26b3014a75eb37db172c8c184cb2b7506da` | `51397359ab1e1c990e656996e69814470d5e0b5d` |
| Task2 generated defaults | `ec6cd1cf2bb53b0c2b792c47cc6234a976a5fb9e` | `c1fea62803a4c7b9080fb9455b4ddf0bc6689998` |
| Task3 all-failed startup | `6a4134af9631b6a667b6694c445c20f87562ac3c` | `1fc8fe0fe698d394db873db0d6ebf51237827489` |
| Task4 applicable floors | `274dd58289f9b89ce95352ae391a1f03ceefb93c` | `d4c89cfc0699ebd4a6c2692e24c3ab87c7e93828` |
| Task4 stale floor test | `e6aca5efd328347fd8d3f7cadeaa6b5e7723a782` | `17ef024ba6a04740f22bee459f24405dee970552` |
| Task5 owner startup grace | `14bd83dbb2d1dead5f8c878c8769998b11ca2d16` | `95af9ac51014d97bc23f6f45226d4cce3d7ab421` |
| Task5 replacement grace | `47cb4198ccb67d1c6f6d88c0c91f0e39965ce6bc` | `7f0115f0962410047108b6d700cf6c2e1e0c0df5` |
| Task5 aborted cleanup | `f6a39b19595634f058ca00caaadcc9d057f70ae2` | `c1a8de124a27518d79278bd064a0377d85bff688` |
| Task6 process fallback sessions | `e71586d45764ec343a7a2a0041a7ac7042ba838c` | `55905be9b5592036d41c940d93817eb60ce8b2a0` |
| Task7 finite sweep headroom | `1d87725d794e3500d68197a1c1433106ef590b9e` | `009aee6dad40d599cdf4eda6b0b16b7cc8d7112d` |
| Task7 diagnostic responder | `7a35d5ca39de42fe7d944a0de1ae1aa5436c2d90` | `152ca18d294325f5a16fb5de6aacdb88521f1379` |
| Task8 lock-loss demotion | `d8d4b0a8595ea402531ee27be3123147a55caf9d` | `1a1f90d76583a703142bb7945bb24287af4ebb0a` |
| Task8 lock-loss forwarding proof | `e6d8488252a508ba993b766ca7eecf5e82df7110` | `89fe06733b9c6c0077946f8d9acffe11454c1fb8` |
| Task9 pre-write epoch | `40fe7e9bd472955a0d6c0f3da43ad08d7e956e98` | `0975c068b18c45b1253faae917e79971752aeb6c` |
| Task9 pre-write epoch | `bc56ed2d751da4e209e221875c8f2b10a46f8d18` | `e646f7c15dd456b12d30064b6ff2a80b6023a58b` |
| Task9 pre-write epoch | `35891f54c49721ab492201549a2dcaf4eb239009` | `173ac58659c19689472d2ceb40bb75fe001a4bbf` |
| Task9 causal write completion | `f0f325db1b01167bc56a747516e726a76e31b8f8` | `a7ff9cc248cc2af3d260147e0166c476b06c5ad8` |
| Task9 footer quiet assertion | `fc3db253fde9bd515a93ba9bce3d275e98da4c71` | `d7c21e804edb231492b7c4cfedd18d7ffcd2fdf2` |

Historical evidence roots are explicit: Tasks1-5 use `/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/`, Task6 uses `/home/lev/Git/lev/mcpls_worktrees/diagnostics-session-isolation/`, Task7 uses `/home/lev/Git/lev/mcpls_worktrees/diagnostics-sweep-headroom/`, Task8 uses `/home/lev/Git/lev/mcpls_worktrees/diagnostics-lock-loss/`, and Task9 uses `/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/`. The Task1-5 reports are in [the completion SDD directory](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.superpowers/sdd/2026-09-10-diagnostics-remaining/), and the later reports are [Task6](/home/lev/Git/lev/mcpls_worktrees/diagnostics-session-isolation/.devkit/task-report.md), [Task7](/home/lev/Git/lev/mcpls_worktrees/diagnostics-sweep-headroom/.devkit/task-report.md), [Task8](/home/lev/Git/lev/mcpls_worktrees/diagnostics-lock-loss/.superpowers/sdd/2026-09-10-diagnostics-remaining/task-8-report.md), and [Task9](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/task-report.md).

Task5 evidence is stage-specific. The owner-grace stage (`14bd83d...` -> `95af9ac...`) has its causal RED in [red-rerun.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/red-rerun.log) and GREEN in [green-final.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/green-final.log). The replacement-order stage (`47cb419...` -> `7f0115f...`) has its causal RED in [fix-red-r2-behavior4.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/fix-red-r2-behavior4.log) and final focused checks in [final-i1-t5-after-lint-fix.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/final-i1-t5-after-lint-fix.log), [final-i1-settle-footer-after-lint-fix.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/final-i1-settle-footer-after-lint-fix.log), and [final-i1-recovery.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/final-i1-recovery.log). The aborted-cleanup stage (`f6a39b1...` -> `c1a8de1...`) has separate RED evidence in [second-red-r2b.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/second-red-r2b.log), [second-red-r2a-failure.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/second-red-r2a-failure.log), and the behavioral portion of [second-red-r2a-cancellation.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/second-red-r2a-cancellation.log), with GREEN in [second-cancel-green-attempt5.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/second-cancel-green-attempt5.log) and final checks in [second-final-i1-t5.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/second-final-i1-t5.log), [second-final-i1-settle-footer.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/second-final-i1-settle-footer.log), and [second-final-i1-recovery.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-completion/.devkit/issue1-logs/task5/second-final-i1-recovery.log). No one RED log is credited with proving all three stages.

Task9 also has stage-specific evidence. The causal stage is the old `af56ff0550c21b00932e8978b6ae135b116390af` mapped to current `a7ff9cc248cc2af3d260147e0166c476b06c5ad8`; its real regression is [i1-t9-red.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/logs/root-fix/i1-t9-red.log), its GREEN is [i1-t9-final.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/logs/root-fix/i1-t9-final.log), and its 23-test footer/build/format/lint coverage is recorded in [i1-footer-final.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/logs/root-fix/i1-footer-final.log), [build.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/logs/root-fix/build.log), [fmt-check.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/logs/root-fix/fmt-check.log), and [lint.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/logs/root-fix/lint.log). The later old `721dd548fb9dc5246592336ddb0e3acc250220f5` maps to current `d7c21e804edb231492b7c4cfedd18d7ffcd2fdf2`; it is an assertion-only quiet follow-up in [i1-t9-quiet.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/logs/root-fix/i1-t9-quiet.log) with focused format and lint in [fmt-check-quiet.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/logs/root-fix/fmt-check-quiet.log) and [lint-quiet.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-write-epoch/.devkit/logs/root-fix/lint-quiet.log). The 721 stage is not a second causal RED/GREEN pair.

## Task10 changes and evidence

The existing `sc_resync_delivers_a_build_error_after_an_apply` scenario now captures the enclosing file path with the rustc E0428 diagnostic, scans every changed file and diagnostic, compares file path, source, code, message, and range, and permits unrelated diagnostics. Its follow-up response uses local typed serde structs with required `changed`, `cleared`, numeric `omitted`, `file_path`, and `Vec<Diagnostic>` fields. Startup notes are rejected when `note` is present with `omitted == 0`, so an empty early response cannot make the absence assertion vacuous.

The scenario also waits for one successful settled diagnostics response after hover readiness and before the rename. The helper uses the existing bounded deadline and polling interval, rejects the startup shape, and does not invoke the full `sc_get_new_diagnostics` scenario. There is no new fixture and no production change.

The first normal run is preserved in [i1-ra.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/i1-ra.log). It ran the selected scenario with actual `/home/lev/.cargo/bin/rust-analyzer`, but failed after 100.956 seconds because the initial fixture allowed rustc E0428 to arrive before baseline adoption. The trace reproduction in [i1-ra-trace.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/i1-ra-trace.log) establishes the cause: didSave occurred at 11:46:51.219, raw rustc E0428 publishes arrived at 11:46:51.471 and 11:46:51.472, and the baseline was adopted at 11:46:52.569. Later reports were empty because the introduced error had correctly become baseline state. These are fixture-readiness RED results, not production delivery failures.

The bounded helper correction is in current Task10 source commit `1df27bebc07ed78ee098f4db0954d294825401e8`, after the parser commit `30cf617f05297517c6f236592b81d95cc04e2616` (the parser's pre-rebase evidence-stage ID was `10d3dad08031178268bfd384aaaf9017cb078055`). The trace validation in [i1-ra-trace-fixed.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/i1-ra-trace-fixed.log) passed 1 selected scenario with 2 skipped in 11.743 seconds and shows baseline adoption at 11:53:31.736 before didSave at 11:53:31.879 and E0428 publication at 11:53:32.115. The normal validation in [i1-ra-fixed.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/i1-ra-fixed.log) passed 1 selected scenario with 2 skipped in 9.238 seconds, identified actual rust-analyzer, and reached the typed follow-up assertion. The accepted F1 production correction is current commit `0b35fdc4986b2fee2c8b1bae5dd902996ef5f314`, rewritten from historical `72303d974840f4832dd40e28f7c6cae9def2d23e`.

## Task10 command results

| Task or check | Result | Evidence |
| --- | --- | --- |
| `build` | exit 0; fresh workspace build completed | [build.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/build.log) |
| initial `i1-ra` | exit 100; 1 run, 0 passed, 2 skipped; actual rust-analyzer scenario reached the E0428 wait and failed at the readiness boundary | [i1-ra.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/i1-ra.log) |
| trace `i1-ra` reproduction | exit 100; 1 run, 0 passed, 2 skipped; raw E0428 preceded baseline adoption | [i1-ra-trace.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/i1-ra-trace.log) |
| fixed trace `i1-ra` | exit 0; 1 selected, 1 passed, 2 skipped; baseline precedes write and E0428, follow-up passes | [i1-ra-trace-fixed.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/i1-ra-trace-fixed.log) |
| fixed normal `i1-ra` | exit 0; 1 selected, 1 passed, 2 skipped; actual rust-analyzer and selected scenario completed | [i1-ra-fixed.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/i1-ra-fixed.log) |
| `i1-regressions` | exit 0; 33 selected, 33 passed, 1,208 skipped; ignored MCP cases included | [i1-regressions.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/i1-regressions.log) |
| `verify` | exit 0; workspace format, lint, 1,187 tests, and 12 passed doctests completed, with 54 tests and 6 doctests skipped or ignored | [verify.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/verify.log) |
| strict `docs` | exit 0; `RUSTDOCFLAGS=-D warnings`, workspace docs generated | [docs.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/docs.log) |
| post-helper `fmt-check` | exit 0 | [fmt-check-after-baseline.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/fmt-check-after-baseline.log) |
| post-helper `lint` | exit 0; warnings denied | [lint-after-baseline-fix2.log](/home/lev/Git/lev/mcpls_worktrees/diagnostics-integration/.devkit/logs/task10/lint-after-baseline-fix2.log) |

The broad build, regression, verify, and strict docs tasks validated integrated source SHA `6429540b5e4f80d51d4bd9e07a659cab6a670c45` before the final test-only baseline helper was added. The helper was compiled and checked by the final focused RA runs and lint; no production source changed after those broad tasks, so they remain valid for the integrated production source. No timeout, cache, wrapper, fixture, or installed artifact was changed.

## Documentation and review boundary

The current design and user guides retain the accepted configuration, startup, session, sweep, ownership, and delivery invariants. The footer descriptions now state the actual global behavior: if no progress begins after the captured epoch, the footer can finish after grace; if new progress begins, it waits for all outstanding work to become quiet or reaches the cap. The tools reference retains the forwarding shape and replaceable shortfall status. The focused configuration diff preserves unrelated historical paragraph wrapping.

The Task10 docs reconciliation commit is `6429540b5e4f80d51d4bd9e07a659cab6a670c45`; the final footer correction and this snapshot are the remaining docs-only change. The current source and docs commits are listed above; the final docs commit and branch HEAD are recorded in `.devkit/task10-report.md` after commit.

The one process-global test-pause slot limitation from Task8 remains visible and is accepted under nextest isolation. Issue #6 live-host wiring, cross-user validation, and manual Windows validation remain external gaps. Issue #7 original desktop-artifact equivalence remains unverified. Codex plugin packaging is assigned to another session. This note does not claim final whole-branch Astra review; root schedules that review after scoped Sol approval.
