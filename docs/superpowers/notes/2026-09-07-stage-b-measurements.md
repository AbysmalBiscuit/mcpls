# Stage B measurements

A snapshot of three measurements the diagnostics stage B design rests on. The numbers below describe this machine and this checkout on 2026-09-07; each section carries the recipe for re-taking its measurement, because all three drift with the codebase and with the tool versions.

Environment for every measurement: Linux 6.18 under WSL2, the `feat/diagnostics-stage-bc` worktree at `/home/lev/Git/lev/mcpls-diag-bc`, rust-analyzer 1.98.1 (48a229c 2026-09-01), pyrefly 1.2.0.

All three used a throwaway LSP probe client that spawns a server over stdio, performs `initialize` and `initialized`, logs every inbound message with its method and full params, answers server-to-client requests with `null` so the server does not stall, and can send `textDocument/didOpen`, `textDocument/didChange`, `textDocument/didSave` and `workspace/didChangeWatchedFiles`. The probe advertised the client capabilities `build_client_capabilities` produces (`crates/mcpls-core/src/lsp/lifecycle.rs`), plus `workspace.didChangeWatchedFiles.dynamicRegistration = true`. The probe was scratch and is not in the tree; the descriptions below are enough to rebuild it.

## Flycheck publish versions

### Method

The rust_workspace fixture (`crates/mcpls-core/tests/fixtures/rust_workspace`) was copied to a scratch directory so the run could write a broken file to disk without dirtying the checkout. rust-analyzer was spawned there, the probe waited for inbound traffic to go quiet for 4 seconds, sent `didOpen` for `src/lib.rs`, waited for quiet again, then sent `didChange` with `version: 7` carrying a full-text replacement that appends

```rust
pub fn probe_type_mismatch() -> i32 {
    "this is not an i32"
}
```

wrote the same text to disk, sent `didSave`, and logged every `textDocument/publishDiagnostics` for 60 seconds. Version 7 was chosen so a matching version could not be confused with an incrementing counter the server happened to keep on its own.

### Raw observations

Eight publishes arrived, all naming `src/lib.rs`, all carrying diagnostics with `source: "rustc"`. Listed as index, elapsed seconds from process start, the `version` field, and the diagnostic codes.

| # | t | version | codes |
| --- | --- | --- | --- |
| 0 | 8.330 | field absent | E0425 |
| 1 | 8.338 | field absent | E0425, E0046 x3 |
| 2 | 8.349 | field absent | E0425, E0046 x3, unused_variables x2 |
| 3 | 16.081 | 1 | E0425, E0046 x3, unused_variables x2 |
| 4 | 24.712 | 7 | E0425 |
| 5 | 24.721 | 7 | E0425, E0046 x3 |
| 6 | 24.729 | 7 | E0425, E0046 x3, E0308 x2 |
| 7 | 24.735 | 7 | E0425, E0046 x3, E0308 x2, unused_variables x2 |

`didOpen` (version 1) went out at t=16.079, `didChange` (version 7) at t=24.079, the disk write at t=24.1, `didSave` at t=24.580.

Publishes 0 through 2 came from the startup flycheck, before any `didOpen`, and the `version` key was absent from the params object entirely rather than present and null. Publish 3 arrived 2ms after `didOpen`, stamped with the document version the `didOpen` carried, and its content is publish 2 replayed: no new flycheck had run. Publishes 4 through 7 arrived 132ms to 155ms after `didSave`, stamped with 7, which is exactly the version the `didChange` sent. Publishes 6 and 7 carry the E0308 that only exists in the changed text, so they are the result of a real flycheck run over the new file and not a replay. The four post-save publishes are cumulative: rust-analyzer republishes the whole set as cargo streams more of it.

No diagnostic with `source: "rust-analyzer"` appeared at any point in the run, so the resident and the flycheck publish could not be confused here. Whether that absence is a property of this fixture, of these client capabilities, or of this rust-analyzer build was not investigated. It matches the repository's own e2e suite, which treats the fixture's E0425 (a rustc code) as the diagnostic that is always present.

The 60 second window after `didSave` is generous for this fixture. The first flycheck at t=8.3 was a cold build that created the scratch copy's `target/`; the second, after `didSave`, was warm and finished in under a fifth of a second.

### Conclusion

A flycheck publish for a document rust-analyzer holds open carries a `version`, and it equals the version the client's last `didChange` sent. A flycheck publish for a file that was never opened carries no `version` key at all. This confirms the claim in the diagnostics injection design's "Stale publishes" and "The wait is on progress, not on document versions" sections, so the version check's two pass-through rules (store a publish with no version, store a publish naming an untracked path) are the right ones and the reasoning that rejects a version-based footer wait does not need restating.

### To re-measure

Rebuild the probe, copy the rust_workspace fixture to scratch, and repeat the sequence above. The signal to look for is the `version` field on the publishes that carry the newly introduced error code, not on the ones that arrive before `didOpen`.

## Pyrefly on an unopened file

### Method

A scratch Python workspace was built with `pyrefly.toml` declaring `project-includes = ["**/*.py"]`, and a `pkg` package holding `a.py` with `def greet(name: str) -> str` and `b.py` calling `greet("world")`. `pyrefly lsp` was spawned with that workspace as its root. The probe sent `didOpen` for `a.py` only, never for `b.py`, waited for quiet, rewrote `b.py` on disk to call `greet(123)`, sent `workspace/didChangeWatchedFiles` naming `b.py` with `type: 2` (Changed), and logged publishes for 60 seconds.

### Raw observations

Pyrefly did register file watchers. Two `client/registerCapability` requests for `workspace/didChangeWatchedFiles`, both with the registration id `FILEWATCHER`, arrived at t=0.066 (before `didOpen`, immediately after `initialized`) and t=6.153 (on `didOpen`). Between them they cover, under the workspace root, `**/*.py`, `**/*.pyi`, `**/*.ipynb`, `**/pyrefly.toml`, `**/.pyrefly.toml`, `**/pyproject.toml`, `**/*.pyc`, `**/*.pyx` and `**/*.pyd`, all with `kind: 7`, plus the same set of source patterns under the active interpreter's `site-packages`. So `b.py` was inside a registered watch glob.

Four publishes arrived over the whole run, all four naming `a.py`, all four with `version: 1` and an empty `diagnostics` array. Three of them (t=6.153, 6.157, 6.157) followed the `didOpen`. The fourth arrived at t=14.066, 2ms after the `workspace/didChangeWatchedFiles` notification went out at t=14.064, so pyrefly did act on the notification and re-published for the document it holds. Nothing named `b.py` was published in the following 60 seconds.

As a control that the error is real and that pyrefly finds it, `pyrefly check` run against the same workspace directory after the probe exited reports `Argument Literal[123] is not assignable to parameter name with type str in function pkg.a.greet [bad-argument-type]` at `pkg/b.py:5:18`.

### Conclusion

No. Pyrefly registers watchers and consumes `workspace/didChangeWatchedFiles`, but it publishes diagnostics only for documents it holds open, so it published nothing for the file it never received a `didOpen` for. The error in that file is one pyrefly detects, as the CLI control shows, so the silence is about delivery and not about detection. For pyrefly, telling the server about watched-file changes buys analysis currency, not diagnostic delivery. The diagnostics injection design's B2 server table lists pyrefly as needing the client to watch files, which is true of registration but overstates the payoff, and the entry should be narrowed to say so.

### To re-measure

Rebuild the probe and repeat. Two things must be confirmed before the result means anything: that the registration actually arrived and its globs cover the file being changed, and that the changed file's error is one the pyrefly CLI reports for the same workspace.

## Cargo check timing

### Method

`cargo check --workspace --all-targets` over `/home/lev/Git/lev/mcpls-diag-bc/Cargo.toml`, which is the command rust-analyzer's default flycheck runs, timed after a no-op `touch` of one source file. Four runs touching `crates/mcpls-core/src/lib.rs`, then two touching `crates/mcpls-cli/src/main.rs`. A no-op touch is the floor for a real edit, since a real edit invalidates at least as much.

GNU `time` is not installed on this machine, so elapsed wall clock came from bash's `EPOCHREALTIME` around the `cargo` invocation, which measures what `/usr/bin/time -f "%e"` reports. A warm-up run with no touch preceded the six so that the first measured run was not a cold build; it finished in 0.37 seconds, confirming the target directory was already warm.

### Raw observations

Touching `crates/mcpls-core/src/lib.rs`: 4.36, 4.22, 4.20, 4.31 seconds.

Touching `crates/mcpls-cli/src/main.rs`: 0.28, 0.27 seconds.

Median of the six is 4.21 seconds. Median of the four mcpls-core runs, which is the number that matters because it is the slower case, is 4.27 seconds.

All six exited zero, so the checkout builds clean.

The gap between the two groups is a dependency-graph effect and not noise. Verified by re-running each touch and reading cargo's own output: touching `mcpls-core/src/lib.rs` rechecks `mcpls-core` and then `mcpls` (the CLI crate), and cargo reports 4.10 seconds; touching `mcpls-cli/src/main.rs` rechecks only `mcpls`, and cargo reports 0.23 seconds. mcpls-core holds nearly all of the code and every `--all-targets` test binary, so an edit there is the realistic worst case.

### Conclusion

About 4.3 seconds for an edit in mcpls-core and about 0.3 seconds for one in mcpls-cli. A footer cap of 15000ms sits comfortably above the slow case with room for the codebase to grow; a 5 second cap would be within noise of it and would expire on ordinary mcpls-core edits. These numbers are slightly faster than but consistent with the earlier measurement quoted in the diagnostics injection design's B3 section (4.65, 4.71, 4.72, 5.10 for mcpls-core and about 0.3 for mcpls-cli), so nothing in that section needs correcting.

### To re-measure

Warm the target directory with one plain `cargo check --manifest-path /home/lev/Git/lev/mcpls-diag-bc/Cargo.toml --workspace --all-targets`, then repeat the touch-and-time loop. The numbers drift upward as the codebase grows, so re-run this rather than trusting the figures above whenever the footer cap is revisited.
