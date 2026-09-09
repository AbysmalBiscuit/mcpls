# Independent Linux diagnostics validation

Snapshot: 2026-09-09. Worktree `/home/lev/Git/lev/mcpls_worktrees/diagnostics-validation`, branch `lev/diagnostics-validation`, starts at recovery commit `e970fefd5fae4c6c54800385d7078afdd25b0f52`. The recovery's prerequisite is `5f6fcc1137824f372f6290860cfbf81b8aba9936`. This snapshot covers issues #3, #4, #5, and #7. Native Windows and live plugin checks remain under #6.

## Findings and changes

- The Linux file-root mutation survived the original assertion. Reading `.gitignore` below a regular file also produces an error, so a generic `incomplete` assertion did not prove the explicit root guard. Both SessionStart and doctor assertions now require `project root is not a directory`. The mutation fails after that correction.
- Explicit allow rules can admit hidden paths. A real CLI fixture with `!.github/` failed against the doctor's unconditional exclusion claim. The doctor, README, and spec describe hidden entries as excluded by default. The fixture confirms both the selected path and its description.
- Recovery CLI fixtures inherited ambient argument configuration. With `MCPLS_LOG_JSON` empty, nine selected tests failed before reaching their hook behavior. The child commands now use the existing ambient-environment cleanup helper. The focused selection passed with malformed logging values after the fix.
- The spec no longer guarantees that detached work succeeds or reaches the next flush. README distinguishes disabling the listener from the local SessionStart scan. The flush socket timeout comment again explains its relationship to the owner's operation deadline.

## Mutation results

Each mutation ran separately against its named test from issue #7. Every mutated run exited 100 with that test's behavioral assertion failing. Each source file was restored in `finally`, touched to invalidate Cargo's build decision, checked against its saved SHA-256, and rerun successfully. All restored runs exited 0. Compilation failures were not counted as mutation results.

| Contract | Failing run | Restored passing run |
| --- | --- | --- |
| SessionStart event | `d44b38d9-5f7c-40d7-9538-5c19b0641ca6` | `bdbc1f6d-6170-46df-b53d-187d304178b7` |
| Context event value | `6b9adb59-c335-4419-9e00-ed68f264f56e` | `fa1214a5-38f2-4f4e-9a1e-c68c2b567c89` |
| Traversal errors | `7782fc61-bdbd-42e7-9e3d-f5f9eaa094f6` | `536526e4-547c-4748-a32c-c1bd1ea26564` |
| Explicit file-root rejection | `6baeb05f-ae9f-4559-a367-a7001bde86fb` | `1bba5a34-cb8f-4192-a011-2be241a80912` |
| Current-user call site | `1dcfc83f-90d6-4983-8a40-ae557cbb27bc` | `a9007b00-c454-4c7a-8f1f-a699b3c3575d` |
| Socket-length call site | `a0374e55-63f5-4935-ae1c-7c5bcb71298a` | `6dcaaa10-15e4-4174-a972-a028fa5f2c52` |
| Configured deadline duration | `a64b256b-173c-4ed5-a309-b0beabbc9fdd` | `fa3c490e-98b9-485e-9501-4641df8494cd` |

The file-root mutation initially survived in run `71c9f6e8-b7a2-4b65-88af-f063b915a3e2`; the restored selection passed in `098d790c-d8bd-4f05-aef3-1ab2d42d61f1`. This finding is retained rather than treating the later failure as proof that the original test was sufficient.

Raw mutation commands, logs, and source hashes are saved locally under `.devkit/mutation-evidence/`. They are local artifacts, not tracked repository files. The table records the essential results independently of those files.

## Review and test accounting

Independent reviewers inspected the recovery's CLI fixtures, the rest of `hook.rs`, Windows CI wiring, watch filtering, deadline handling, README, and design spec against the prerequisite commit. A separate review of the resulting working-tree changes found no further code defects. These were source reviews; they did not execute the host parser or Windows runtime.

The Linux recovery count is 1,132 + 10 added tests - 6 deleted tests = 1,136. Nine additions are CLI tests; the other is the concurrent-client socket test. Two additional pipe tests are Windows-only. All six deleted tests exercised the removed per-operation message helper. Its nondefault-duration assertion is replaced by the real socket test, which configures 200 ms and checks the complete deadline response. The fresh literal-1500 mutation proves that duration coverage. A separate retained test observes detached completion without claiming later delivery. This validation adds one CLI test for the hidden-path allow rule.

Production `identity.rs` remains identical to the prerequisite commit. The two identity mutations test its actual CLI call sites without changing process-global environment variables or introducing unsafe code.

## Verification

The initial Linux baseline passed 1,136 tests with 40 skipped, run `7cff1cab-fae9-4ecc-a8b3-29c0e26bdfea`.

The malformed-environment selection failed nine tests in run `f1d1547a-2f90-44e5-ac6d-d5f9a3cf7a68`. After child-environment cleanup, all 12 selected tests passed in `f2a1a247-6e40-4b10-bb90-7d543318de0e`. The hidden-path description test then failed in `87632b49-0242-459d-a067-e07c316a8c6c`; after correcting the doctor, all 13 selected tests passed in `d8a08632-f5c1-484e-8f18-0272666b41d1`.

With the compiler wrapper enabled, the full Linux suite passed 1,137 tests with 40 skipped, run `61b805e1-7976-4ba8-ba9e-08436d127628`. The full suite passed again after the permission-fixture correction, run `c9b05be2-323b-48bf-871b-2c1b2ed16451`, with the same totals. Documentation tests passed 12 with 6 ignored. Nightly formatting and `git diff --check` passed.

Clippy initially failed through the configured `zccache` 1.13.22 wrapper with `multiple input filenames provided`, treating the toolchain's `rustc` path and `crates/mcpls-core/src/lib.rs` as source inputs. Lev subsequently authorized disabling the wrapper in the Clippy task. The task now sets `RUSTC_WRAPPER = ""`, and workspace Clippy passed for all targets and features with warnings denied. The `verify` dry run confirmed that other tasks receive no wrapper override. The earlier direct Clippy run caught a long API-doc first paragraph; that paragraph was split before the final lint check.

Lev confirmed with standard shell tools that `sudo ls -la` could list a mode-000 directory while ordinary `ls -la` returned permission denied. The two permission fixtures now probe the relevant access operation and report the fixture unavailable when the runner can still access it. Permissions are restored before either return or assertion. Both tests passed under the ordinary runner after the correction, run `168f5fdf-c41a-4ee4-8372-53d404949119`. The application and its tests were not run under sudo; no privileged application result is claimed.

The initial build failed in `zccache` with exit 113. Baseline and mutation runs used a command-scoped empty `RUSTC_WRAPPER`; no global Cargo configuration was changed. Final build and test commands preserve the configured wrapper. The Clippy task is the explicitly authorized exception.

## Reproduce the standard checks

The main checkout's task configuration initially could not be parsed because the verification sequence used multiline inline tables. That syntax was corrected to an array of tables while adding the Clippy override. The local validation tasks came from the valid configuration at commit `01f0536`, with the same task-specific override added. To recreate the standard verification configuration in this worktree:

```fish
git -C /home/lev/Git/lev/mcpls show 01f0536:devkit.toml > /home/lev/Git/lev/mcpls_worktrees/diagnostics-validation/.devkit/standard-validation.toml
printf '\n[tasks.lint.env]\nRUSTC_WRAPPER = ""\n' >> /home/lev/Git/lev/mcpls_worktrees/diagnostics-validation/.devkit/standard-validation.toml
devrun -C /home/lev/Git/lev/mcpls_worktrees/diagnostics-validation --config /home/lev/Git/lev/mcpls_worktrees/diagnostics-validation/.devkit/standard-validation.toml task verify
git -C /home/lev/Git/lev/mcpls_worktrees/diagnostics-validation diff --check
```

The verification task runs nightly formatting, Clippy with warnings denied, the workspace nextest suite, and documentation tests. Mutation and permission tasks in the local `validation.toml` are additional session fixtures.

## Evidence limits and remaining work

The recovery is committed and fetchable at `e970fefd`; issue #7's statement that it exists only as uncommitted work is stale. The earlier [recovery snapshot](2026-09-08-diagnostics-followup-validation.md) records an exact reconstruction and native Windows runs. Its original rollout, raw evidence, and reconciliation files are absent on this machine. The recovery's base-relative diff can be inspected, but the lost-original comparison cannot be independently reconstructed from the available files. Those earlier claims remain historical evidence.

The chmod-based permission fixtures check actual readability or metadata access before asserting denial behavior. The translator fixture concerns a stat error, despite #5 calling it an unreadable `.gitignore` fixture. These checks prevent privileged access from being mistaken for a production failure; denial assertions still run under an ordinary user. Accepted scope tradeoffs recorded in #5 remain accepted and are not silently counted as new fixes.

Native Windows execution, real multi-account behavior, and live Claude Code delivery remain in #6. The local validation changes have not been pushed, merged, installed, or used to close issues.
