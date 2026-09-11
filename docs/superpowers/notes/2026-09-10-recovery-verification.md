# Diagnostics recovery verification evidence

Snapshot: 2026-09-10. This note records the evidence available for issue #7 at
the recovery-verification head. Counts and run IDs carried from earlier notes
are historical observations, not Task 3 reruns.

The functional chain is [`22e97c91b5060d619cbc3ef5030801c7bf34b45a`](https://github.com/AbysmalBiscuit/mcpls/commit/22e97c91b5060d619cbc3ef5030801c7bf34b45a)
(`fix(core): restore notifications after respawn`),
[`68a4b31d39f41d4fe8913da6a0e09d0e7ff89acf`](https://github.com/AbysmalBiscuit/mcpls/commit/68a4b31d39f41d4fe8913da6a0e09d0e7ff89acf)
(`fix(core): publish startup clients first`), and
[`1d9cfd07a8f1258a8ad4b03f0c284ab702efce91`](https://github.com/AbysmalBiscuit/mcpls/commit/1d9cfd07a8f1258a8ad4b03f0c284ab702efce91)
(`fix(bridge): sync write targets before saves`). Task 1's focused MCP
recovery run passed 56/56; its full verification passed 1,163 tests and 12
doctests. Task 2's focused resynchronization run passed 28/28, its installed
rust-analyzer run passed 3/3 in 14.793 seconds, and its final verification
passed 1,168 tests and 12 doctests. The Task 1 and Task 2 reports are local
artifacts at
`.superpowers/sdd/2026-09-10-recovery-verification/task-1-report.md` and
`.superpowers/sdd/2026-09-10-recovery-verification/task-2-report.md`.

Immutable source anchors for the behavioral evidence are the [MCP recovery
test](https://github.com/AbysmalBiscuit/mcpls/blob/68a4b31d39f41d4fe8913da6a0e09d0e7ff89acf/crates/mcpls-core/src/recovery_tests.rs#L85),
[startup publication test](https://github.com/AbysmalBiscuit/mcpls/blob/68a4b31d39f41d4fe8913da6a0e09d0e7ff89acf/crates/mcpls-core/src/recovery_tests.rs#L95),
[unopened-target save ordering test](https://github.com/AbysmalBiscuit/mcpls/blob/1d9cfd07a8f1258a8ad4b03f0c284ab702efce91/crates/mcpls-core/src/mcp/server.rs#L2929),
[resynchronization cancellation and generation test](https://github.com/AbysmalBiscuit/mcpls/blob/1d9cfd07a8f1258a8ad4b03f0c284ab702efce91/crates/mcpls-core/src/bridge/translator/edits.rs#L2238),
[unopened-module rust-analyzer test](https://github.com/AbysmalBiscuit/mcpls/blob/1d9cfd07a8f1258a8ad4b03f0c284ab702efce91/crates/mcpls-core/tests/ra_e2e.rs#L1692),
and [timeout default assertion](https://github.com/AbysmalBiscuit/mcpls/blob/35f1fac1024a02e319df67b719ade4fea472ce2c/crates/mcpls-cli/src/hook.rs#L1350).

## Task 3 corrections

- `FLUSH_SOCKET_TIMEOUT` now has a short comment stating that the client
  timeout tracks the owner's default operation deadline and that the
  acknowledgement has a separate allowance.
- `HooksConfig.enabled` documents that disabling the listener still allows the
  local `SessionStart` watch scan.
- `HooksConfig.op_deadline_ms` no longer promises that started work reaches a
  later flush. The docs state that work can continue, while a timeout does not
  guarantee eventual success or delivery.
- The existing
  `test_the_flush_timeout_tracks_the_op_deadline_default` assertion now derives
  its expected duration from `HooksConfig::default().op_deadline_ms`. It does
  not establish configurable deadline support, and no test was added for the
  comment.

The test-first coupling check temporarily changed the default from 1,500 ms to
1,600 ms. The named test failed on its own assertion with `left: 1.5s` and
`right: 1.6s`, exit 100, in
`.devkit/task3-logs/deadline-coupling-red.log`. A trap restored the default to
1,500 ms. The supplied focused command then passed 1/1, with 1,207 tests
skipped, in nextest run `3395a21b-684d-474d-bead-abd2ef7b7934`; its log is
`.devkit/task3-logs/deadline-coupling-green.log`.

The tracked `devkit.toml` `test` task includes the timeout assertion and can
reproduce it from a clean checkout:

```fish
devrun -C /home/lev/Git/lev/mcpls_worktrees/recovery-verification task test
```

The narrow Task 3 run used local ignored validation files. Reproduce that run
only where those files exist:

```fish
devrun -C /home/lev/Git/lev/mcpls_worktrees/recovery-verification --config /home/lev/Git/lev/mcpls_worktrees/recovery-verification/.devkit/validation.toml task issue7-audit --env-file /home/lev/Git/lev/mcpls_worktrees/recovery-verification/.devkit/test.env
```

## Seven mutation contracts

The controller's local `.devkit/issue7-audit.md` accepts the explicit mutation
record in the [2026-09-09 validation
note](2026-09-09-diagnostics-validation.md). Each named assertion failed with
the mutation and passed after restoration:

| Contract | Mutated run | Restored run |
| --- | --- | --- |
| SessionStart event | `d44b38d9-5f7c-40d7-9538-5c19b0641ca6` | `bdbc1f6d-6170-46df-b53d-187d304178b7` |
| Context event value | `6b9adb59-c335-4419-9e00-ed68f264f56e` | `fa1214a5-38f2-4f4e-9a1e-c68c2b567c89` |
| Traversal errors | `7782fc61-bdbd-42e7-9e3d-f5f9eaa094f6` | `536526e4-547c-4748-a32c-c1bd1ea26564` |
| Explicit file-root rejection | `6baeb05f-ae9f-4559-a367-a7001bde86fb` | `1bba5a34-cb8f-4192-a011-2be241a80912` |
| Current-user call site | `1dcfc83f-90d6-4983-8a40-ae557cbb27bc` | `a9007b00-c454-4c7a-8f1f-a699b3c3575d` |
| Socket-length call site | `a0374e55-63f5-4935-ae1c-7c5bcb71298a` | `6dcaaa10-15e4-4174-a972-a028fa5f2c52` |
| Configured deadline duration | `a64b256b-173c-4ed5-a309-b0beabbc9fdd` | `fa3c490e-98b9-485e-9501-4641df8494cd` |

Task 3 did not repeat these unchanged historical mutations. Their raw command
logs and source-hash artifacts are unavailable; the table and tracked note
are the durable record. The original rollout, `diagnostics-recovery.patch`,
reconciliation/evidence JSON, and the raw mutation directory cannot be
reconstructed from this checkout. A rejected history-index refresh was not
retried. The available git and tracked notes do not justify a byte-equivalence
claim or a claim that unobserved deletions were comments.

## Native Windows evidence

GitHub Actions run [34451287168](https://github.com/AbysmalBiscuit/mcpls/actions/runs/34451287168)
passed at commit
[`d68565a3a83e02e75461f95caf227685ef3fb61e`](https://github.com/AbysmalBiscuit/mcpls/commit/d68565a3a83e02e75461f95caf227685ef3fb61e):

- [Integration job 102788415590](https://github.com/AbysmalBiscuit/mcpls/actions/runs/34451287168/job/102788415590)
  ran and passed the concurrent-client, deadline, overrun, SessionStart,
  hidden-path, file-root, and ignore-error tests. The captured log is
  `.devkit/logs/windows-integration-pr14.log`.
- [Unit job 102788415516](https://github.com/AbysmalBiscuit/mcpls/actions/runs/34451287168/job/102788415516)
  ran and passed the drive-prefix, canonical absolute glob, user-scoped pipe
  name, sorted confined pipe scan, busy-pipe recovery, and poisoned-pipe
  transport tests. The captured log is `.devkit/logs/windows-unit-pr14.log`.
- [Clippy job 102787427941](https://github.com/AbysmalBiscuit/mcpls/actions/runs/34451287168/job/102787427941)
  executed `cargo clippy --all-targets --all-features --workspace -- -D warnings`
  and finished successfully. The captured log is
  `.devkit/logs/windows-clippy-pr14.log`.

These jobs provide automated Windows coverage for the named code paths. Manual
Windows checks and the real Claude plugin-host gate remain in [issue
#6](https://github.com/AbysmalBiscuit/mcpls/issues/6). No local Windows or
privileged application run is claimed.

The final external-server check, strict documentation check, Task 4 fixture
change, final PR review, and CI on the completed recovery head remain pending.
This note does not claim final CI or issue closure.
