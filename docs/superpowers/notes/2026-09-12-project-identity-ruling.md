# Project identity ruling

How the shared backend's endpoint name is derived, and why. Settled on 2026-09-12
against the shared backend design
(`docs/superpowers/specs/2026-09-12-shared-backend-design.md`), which carries the
resulting prose under "Identity is the checkout root". This note keeps the reasoning
and the case analysis the design states without arguing.

The question the design left open: whether the identity keeps hashing the process
working directory, or resolves a checkout root. The position argued against the root
was that a rule which merges two checkouts under one name is far worse than one which
splits a single checkout into two backends, so the conservative rule wins.

## 1. Ruling

The identity is the checkout root, not the working directory: both sides canonicalize the directory the host hands them, walk up to the nearest ancestor-or-self holding an entry named `.git`, file or directory, and hash that. No git is invoked, no dependency is added, and the rule is fixed rather than configurable. The asymmetry argument is right about which failure is worse and wrong about what it implies, because a rule keyed on a `.git` entry keys on a working tree, so it can split but never merge two checkouts; it is as safe as the working directory on the axis that matters and shares in the cases the working directory does not.

## 2. The rule

`project_root(start) -> PathBuf`, in `crates/mcpls-core/src/hooks/identity.rs`, called by both `identity_hash` callers:

1. `start` is canonicalized through `dunce`, exactly as `identity_hash` does today (`identity.rs:49-53`). A failure to canonicalize is the error it is today.
2. Candidates are `start` and then each ancestor in order. The walk stops, without testing it, at the first candidate that is the home directory or the filesystem root. Home is what `dirs` reports, which is `HOME` on Unix and the profile folder on Windows; `dirs` is already a workspace dependency. Compare canonical to canonical.
3. The first candidate for which `candidate.join(".git").exists()` holds is the root. File or directory, no parsing, no reading its contents.
4. No candidate matches: the root is `start`.
5. The hash covers the root. The handshake's "canonical project root" field carries the root. `mcpls hook doctor` prints the start directory, the root and the hash.

The function is idempotent, `project_root(project_root(x)) == project_root(x)`, because a root either holds `.git` or is its own start. That is what lets the backend re-derive its identity from its own working directory: the frontend on Unix, and the hook on Windows, spawn the backend with `current_dir(root)`.

Which directory is `start`:

- Frontend: the process working directory, as `canonicalized_cwd_identity` reads it today (`crates/mcpls-core/src/lib.rs:594-601`). Both hosts launch a stdio server there; Claude Code measured, Codex by source (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:273,282`; `codex-rs/core/src/session/mcp_runtime.rs:25-30`).
- Claude Code hook: `CLAUDE_PROJECT_DIR`, as today (`crates/mcpls-cli/src/main.rs:40-48,72-74`).
- Codex hook adapter: the payload's `cwd`. Every Codex hook payload carries it as a required string (`codex-rs/hooks/src/schema.rs:283,306,328,352,372,489`), it is the turn's working directory (`codex-rs/core/src/hook_runtime.rs:133,175,236,278,352,388,410,447,484,544`), and the hook command is run in that same directory (`codex-rs/hooks/src/engine/dispatcher.rs:94-103`; `codex-rs/hooks/src/engine/command_runner.rs:55-62`). The adapter must not read `CLAUDE_PROJECT_DIR`, which a Codex run started from inside a Claude Code shell inherits and which then names the wrong project.
- Neither named: the process working directory, which is what the `.` fallback canonicalizes to today.

Two changes follow from the rule and are part of it:

- The backend's workspace base is the root, not `current_dir()`. Today the base is the working directory at `crates/mcpls-core/src/lib.rs:665`, and a backend rooted at whichever subdirectory its spawning frontend sat in would put a second session's files outside every workspace root. With the base at the root, a session started anywhere inside the checkout gets the same servers a session started at the root gets, and the Stage 3 watcher watches the checkout.
- The runtime directory on Unix stops reading `XDG_RUNTIME_DIR`. This is a precondition, not a refinement: without it the Codex hook side never meets a Codex frontend on Linux under any identity rule. Section 5 has the citations. The `shared_temp_runtime_dir` fallback that already exists (`identity.rs:225-230`) reads only `TMPDIR`, through `temp_dir()`, and `USER`/`LOGNAME`, all of which survive Codex's filter.

Configurability: none. A knob would have to reach the hook process, which loads no mcpls configuration and under Codex receives no environment mcpls controls. Two processes on different rules derive different endpoint names and never open a handshake, so the configuration fingerprint cannot report the mismatch; it only sees connections that already agree on the name. `workspace.roots` keeps its current meaning, scoping the language servers inside a backend, and `--no-backend` remains the escape for a checkout the rule serves badly.

Case by case:

| Case | Result |
|---|---|
| Two sessions in different subdirectories of one checkout | One root, one backend, servers rooted at the checkout. |
| Several git worktrees of one repository | Each linked worktree has its own `.git` file, so each is its own root and backend. Sessions inside one worktree share. `git rev-parse --git-common-dir` would collapse them, which is why nothing resolves through git. |
| Codex entry with explicit `cwd` outside the checkout | The frontend's root is that directory's root; every hook resolves to the checkout. Two endpoints. The backend serves the session and hears no hooks; the doctor shows both sides. No directory rule can fix this, and the entry's `cwd` is treated as the user's instruction. An explicit `cwd` inside the checkout agrees with the hooks, which the working directory rule could not do. |
| Explicit `cwd` inside the checkout | Same root as the hooks. Shares. |
| Not a repository, or a non-git VCS | Falls to step 4: the directory itself, today's behaviour. Subdirectory sessions split, answers stay correct. Adding `.hg` or `.jj` to the test is one line if it is ever wanted; it is not wanted now. |
| Nested repositories and submodules | Nearest `.git` entry wins. A session inside a submodule gets the submodule's backend; one at the outer root gets the outer backend, whose servers may also index the submodule's files. Two backends can hold one file, which is a split. Nothing merges. |
| Monorepo with several ecosystems | One backend at the checkout root. The marker scan already runs to `heuristics_max_depth` below each root (`crates/mcpls-core/src/config/server.rs:98-114`), so every ecosystem's server spawns, each rooted at the checkout. A sub-project a language server cannot discover from the root is named in `workspace.roots`, which is what that setting is for. |
| Codex hook side | Payload `cwd`, above. Needs the runtime directory fix to meet the frontend at all. |
| Configurable | No. Default and only rule is the above. |

## 3. Why

**The asymmetry holds and does not decide this.** Splitting too eagerly costs a backend; merging too eagerly serves one tree's contents under another tree's name. The second is worse. But the merge failure needs two working trees under one name, and the only rules that produce it are the ones keyed on the repository rather than the tree: the common dir, the remote, the main checkout's path. A `.git` entry is per tree. Every worktree, submodule and nested repository carries its own, so the nearest-entry rule partitions the filesystem into trees and never puts two under one name. What it does merge is two directories in one tree, and those hold the same files by definition. The one merge it performs is the one the design wants.

The delivery-record half of the objection is handled elsewhere. Records are keyed by session and agent, not by backend (design, "Sessions, agents and records"). Two sessions on one backend read two records however the backend was named. Identity decides which language servers a session shares; it cannot make one session consume another's record. So the record-crossing failure the diagnostics design guarded against is not on the table for either rule.

**The working directory splits exactly where splitting buys nothing.** rust-analyzer resolves the Cargo workspace from whichever member it is pointed at and indexes the whole workspace. That is unmeasured here. It is how rust-analyzer's project discovery is documented to work. Two sessions launched in `crates/mcpls-core` and `crates/mcpls-cli` under the working directory rule therefore hold two full copies of one index, which is the OOM in the problem statement, with nothing gained from the split. Under the root rule they hold one.

**The cost is a stat per ancestor.** No git subprocess, no dependency, no parsing. A hook pays it on every invocation; a checkout ten levels below home is ten `exists()` calls, which is noise beside the socket connect the hook already makes. The code lives in `identity.rs`, which upstream does not have at all (upstream `crates/mcpls-core/src/` has no `hooks` module), so the rule buys no merge conflict. The one upstream line it touches is the workspace base at `lib.rs:665`, upstream `lib.rs:563`.

**The home guard is one comparison and prevents the worst outcome.** A dotfiles repository in the home directory would otherwise become the root of every non-git directory beneath it, and a language server rooted at home indexes home. Both hosts pass what the guard reads: `HOME` is on Codex's Unix list, `USERPROFILE` on its Windows list.

**Not configurable, because the failure of a wrong setting is silence.** Every other mismatch in this design is reported through a handshake both sides reach. A rule mismatch means no handshake. A knob whose wrong value produces a permanent silent no-op is worse than no knob.

## 4. What it costs and what it still gets wrong

- **Servers root at the checkout, not the launch directory.** Today a session launched in a subdirectory gets servers rooted there. Under the rule it gets servers rooted at the checkout. For a Cargo workspace that is the same index. For a monorepo whose Rust project sits two or more levels below the root with no `Cargo.toml` above it, rust-analyzer at the root does not find it, and the fix is `workspace.roots` in a trusted project `mcpls.toml` or the global config. This is the one real behaviour change, and it is the same behaviour a session launched at the root has always had.
- **A project nested inside an unrelated repository.** A scratch project under a notes repository resolves to the notes repository. Same failure shape as the monorepo, same fix, or `git init` in the scratch project.
- **Explicit Codex `cwd` outside the checkout.** Still two endpoints. Unfixable by any directory rule; the doctor is the only tool.
- **Non-git checkouts still split by subdirectory.** Today's behaviour, unchanged.
- **The marker scan runs from the root.** `is_applicable_recursive` walks to depth ten below each root at backend start. On a large checkout that is more directories than a subdirectory launch scanned. Once per backend, not per session.
- **A `.git` entry that is not a checkout.** A stray file named `.git`, or a working tree driven by `GIT_DIR` and `GIT_WORK_TREE` with no entry inside it. The first misroots without merging anything; the second falls to the directory itself. Both rare enough to leave.
- **Two variables, two directories.** The Claude hook trusts `CLAUDE_PROJECT_DIR` and the Codex adapter trusts the payload. Whether Claude Code's own payload `cwd` equals `CLAUDE_PROJECT_DIR` is unverified and does not matter while the Claude side keeps the variable the diagnostics design measured.

## 5. What this contradicts in the design doc

1. **A Codex frontend and a Codex hook land in different runtime directories on Linux.** `runtime_dir()` prefers `$XDG_RUNTIME_DIR/mcpls` (`crates/mcpls-core/src/hooks/identity.rs:206-211`). Codex launches a stdio server with `env_clear()` plus a fixed list (`codex-rs/rmcp-client/src/stdio_server_launcher.rs:283-284`; `codex-rs/rmcp-client/src/utils.rs:14-19`), and the Unix list is `HOME`, `LOGNAME`, `PATH`, `SHELL`, `USER`, `__CF_USER_TEXT_ENCODING`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, `TZ` (`utils.rs:122-134`). No `XDG_RUNTIME_DIR`. Codex hook commands are built without clearing the environment; `build_command` only adds the handler's own variables (`codex-rs/hooks/src/engine/command_runner.rs:191`). So wherever the user's environment sets `XDG_RUNTIME_DIR`, the frontend binds `/tmp/mcpls-{user}/{hash}.sock` and the hook connects to `$XDG_RUNTIME_DIR/mcpls/{hash}.sock`. They never meet. This breaks the design's one endpoint per project, under "The endpoint", for the Stage 4 Codex adapter regardless of the identity rule, and the design's own reading of the environment, under "Sessions, agents and records", names the fixed list without noticing what it drops. Windows is unaffected: the pipe prefix reads `USERNAME`, and Codex's Windows list carries `USERNAME` and `USERPROFILE` (`codex-rs/protocol/src/shell_environment.rs:119-131`). Fix: delete the `XDG_RUNTIME_DIR` branch and use `shared_temp_runtime_dir` on every Unix host.
2. **The non-goals said socket permissions already scope an endpoint to one user, which is not something the code does.** Nothing under `crates/mcpls-core/src/hooks/` sets a permission. The listener creates the runtime directory with `create_dir_all` at the default mode (`crates/mcpls-core/src/hooks/listener.rs:230`) and binds the socket under the process umask. Scoping on Unix comes from the directory name carrying the user plus a umask that happens to withhold write from others; under umask `000` the socket is connectable by anyone, and under the `/tmp` fallback another user can pre-create `/tmp/mcpls-{user}`. For that sentence to be true, the listener sets `0o700` on the directory after creating it, which `std::os::unix::fs::PermissionsExt` does without unsafe code. With the fix in item 1 the `/tmp` fallback becomes the only Unix path, so this stops being a corner.
3. **The socket length check does not depend on the project path.** The socket path is `runtime_dir/{hash}.sock` with a fixed sixteen-character hash (`identity.rs:118-120`), so resolving a longer or shorter root changes nothing about it. Only the runtime directory's own length can trip `sockaddr_un.sun_path`.
4. **Two passages disagreed about the Codex environment.** Under "Configuration and trust" Codex "passes only what the server's own entry names"; under "Sessions, agents and records" it passes "a fixed list plus what the server's own entry names". The second is right (`utils.rs:19,122`), and the list in question is the one in item 1.
5. **The Stage 4 obstacle list was short by one.** It named the missing changed-files field as the Codex adapter's one real obstacle. The adapter also has to learn its project directory, since Codex exports nothing like `CLAUDE_PROJECT_DIR`; the payload's `cwd` is the answer, per section 2. Not a contradiction, a gap the ruling fills.
6. **Citations verified against `rust-v0.147.0`, with drift.** `stdio_server_launcher.rs:270` in the decision entry is `:273` and `:282` here; `mcp_runtime.rs:88,115` is `:25-30`, `:67-71` and `:92-94`. The doc says its lines come from `0.154.0`, so this is drift rather than error. The claims themselves hold: the fallback cwd is the primary environment's cwd or the session cwd, with no move toward a repository root. Codex does resolve a git root, but only for trust (`codex-rs/core/src/config/mod.rs:3411`), and it never feeds the server's cwd.

## 6. Where this landed

The design doc carries the rule as prose under "Identity is the checkout root", the
Stage 1 scope names `project_root`, the workspace base and the runtime directory, the
cross-user non-goal names the directory mode, and the verification list gained the
subdirectory, worktree and Codex runtime directory cases. Items 1 and 2 of section 5
are changes the design now requires rather than observations about it.
