# Shared backend prerequisites implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land every part of the shared backend's Stage 1 that does not change the process model, so the split itself arrives against an endpoint, a runtime directory and a notification pump that are already correct.

**Architecture:** Two independent groups of changes to the existing single-process mcpls. The endpoint's name stops depending on which subdirectory a session started in, and the Unix runtime directory stops depending on a variable one host drops for MCP servers and keeps for hooks. Separately, the diagnostics pump stops treating one peer's failure as its own, which is what would otherwise let one session's disconnect stop diagnostics caching for every session sharing a backend.

**Tech Stack:** Rust 2024, tokio, rmcp 3.1.4, serde, `dunce` for canonicalization, `dirs` for the home directory, `cargo nextest` through devkit.

**Spec:** `docs/superpowers/specs/2026-09-12-shared-backend-design.md`. The identity reasoning it states without arguing is in `docs/superpowers/notes/2026-09-12-project-identity-ruling.md`.

## Global Constraints

- The workspace sets `unsafe_code = "deny"`, so `std::env::set_var` does not compile and no test may set an environment variable. Every function that reads the environment splits into a public reader and a pure inner function taking the value, the way `current_user` and `user_component` already do (`crates/mcpls-core/src/hooks/identity.rs:163-198`).
- Canonicalize through `dunce::canonicalize`, never `Path::canonicalize`. The latter returns a `\\?\C:\...` path on Windows, so one side using each would disagree on every Windows install (`crates/mcpls-core/src/hooks/identity.rs:7-12`).
- Both sides of the socket derive the same identity from the same directory on both platforms. A change to one side is a change to the other in the same task.
- This is a fork that still merges from upstream `bug-ops/mcpls`, so do not restructure or reformat upstream code you are not changing. `crates/mcpls-core/src/hooks/` does not exist upstream and is free of that constraint.
- Comments are timeless: no reference to this plan, no issue numbers, no "now we" or "used to", no TDD narration.
- Commits follow Conventional Commits, subject at most 50 characters including the type prefix, imperative, lowercase after the colon, no trailing period. Body wrapped at 72. Every commit ends with a trailer naming the model that wrote it, per `AGENTS.md`.
- Commits are GPG signed. If signing fails, stop and report rather than passing `--no-gpg-sign`.
- A CLI test that spawns `mcpls` isolates the child's runtime directory with `env_remove("XDG_RUNTIME_DIR")`, `env("TMPDIR", <temp>)` and `env("USER", <fixed name>)`. Setting `XDG_RUNTIME_DIR` is how those tests isolate today and stops working in Task 4; `TMPDIR` works before and after it. Every such test also calls `clear_ambient_env` first (`crates/mcpls-cli/tests/cli_integration.rs:27-32`).
- Run `devrun task verify` before each commit. For a single test during the loop, `cargo nextest run -p mcpls-core <filter>` is faster. `devrun task verify` runs on Linux and cannot see a Windows-only compile failure, so check every new `#[cfg]` against the gate on what it calls.

---

## File structure

| File | Responsibility after this plan |
|---|---|
| `crates/mcpls-core/src/hooks/identity.rs` | Resolves a checkout root from any directory inside it, derives the hash, and names the socket or pipe. Owns the rule both sides follow. |
| `crates/mcpls-core/src/hooks/listener.rs` | Binds the endpoint and owns the runtime directory's mode. |
| `crates/mcpls-core/src/lib.rs` | Resolves the checkout root for the workspace base and, separately, for the endpoint. Keeps the diagnostics pump running when one connection's notify fails. |
| `crates/mcpls-cli/src/main.rs` | Resolves the root on the hook side from the directory the host names. |
| `crates/mcpls-cli/src/hook.rs` | Prints the start directory, the root and the hash in the doctor report. |

Tasks 1 to 5 are the identity and runtime directory group. Task 6 is the pump. The two groups touch different files and can be reviewed independently, and they are one plan because the split needs both settled before it starts.

---

### Task 1: resolve a checkout root

**Files:**
- Modify: `crates/mcpls-core/src/hooks/identity.rs` (add after `identity_hash`, which ends at line 61)
- Test: `crates/mcpls-core/src/hooks/identity.rs`, the existing `#[cfg(test)] mod tests` at line 232

**Interfaces:**
- Consumes: `crate::error::{Error, Result}`, already imported at line 18.
- Produces: `pub fn project_root(start: &Path) -> Result<PathBuf>`, the canonical checkout root enclosing `start`. `fn root_from(canonical: &Path, home: Option<&Path>) -> PathBuf`, the pure walk, for tests that cannot set `HOME`.

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` in `crates/mcpls-core/src/hooks/identity.rs`:

```rust
    /// A `.git` entry marks a working tree, so a directory inside one
    /// resolves to the tree rather than to itself.
    #[test]
    fn test_root_from_finds_the_nearest_git_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        let nested = root.join("crates").join("core");
        std::fs::create_dir_all(&nested).expect("nested dirs");

        assert_eq!(root_from(&nested, None), root);
    }

    /// A linked worktree holds a `.git` file rather than a directory, and it
    /// is its own root: two worktrees of one repository hold different files
    /// and must never share an endpoint.
    #[test]
    fn test_root_from_accepts_a_git_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        std::fs::write(root.join(".git"), "gitdir: /elsewhere/.git/worktrees/w")
            .expect("git file");
        let nested = root.join("src");
        std::fs::create_dir(&nested).expect("nested dir");

        assert_eq!(root_from(&nested, None), root);
    }

    /// With no marker anywhere, the start directory is the root, which is
    /// the behaviour of a hash taken straight from the working directory.
    #[test]
    fn test_root_from_falls_back_to_the_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let start = dunce::canonicalize(dir.path()).expect("canonical");

        assert_eq!(root_from(&start, None), start);
    }

    /// The walk stops before the home directory, so a dotfiles repository
    /// there never becomes the root of a project beneath it, which would
    /// point a language server at the whole of home.
    #[test]
    fn test_root_from_stops_before_home() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dunce::canonicalize(dir.path()).expect("canonical");
        std::fs::create_dir(home.join(".git")).expect("git dir");
        let project = home.join("notes");
        std::fs::create_dir(&project).expect("project dir");

        assert_eq!(root_from(&project, Some(&home)), project);
    }

    /// The nearest marker wins, so a session inside a submodule gets the
    /// submodule rather than the repository containing it.
    #[test]
    fn test_root_from_prefers_the_nearest_marker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let outer = dunce::canonicalize(dir.path()).expect("canonical");
        std::fs::create_dir(outer.join(".git")).expect("outer git");
        let inner = outer.join("vendor").join("dep");
        std::fs::create_dir_all(&inner).expect("inner dirs");
        std::fs::write(inner.join(".git"), "gitdir: ../../.git/modules/dep")
            .expect("inner git file");

        assert_eq!(root_from(&inner, None), inner);
    }

    /// Resolving a root that is already a root returns it unchanged, which
    /// is what lets a process re-derive its own identity from the directory
    /// it was started in.
    #[test]
    fn test_root_from_is_idempotent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        let nested = root.join("src");
        std::fs::create_dir(&nested).expect("nested dir");

        let once = root_from(&nested, None);
        assert_eq!(root_from(&once, None), once);
    }

    /// `project_root` canonicalizes, so a start directory reached through a
    /// symlink resolves to the same root as the real path.
    #[test]
    fn test_project_root_canonicalizes_its_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        let nested = root.join("src");
        std::fs::create_dir(&nested).expect("nested dir");

        assert_eq!(project_root(&nested).expect("root"), root);
    }

    /// A start directory that does not exist is an error rather than a
    /// silently different endpoint.
    #[test]
    fn test_project_root_rejects_a_missing_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("absent");

        assert!(project_root(&missing).is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core root_from project_root`

Expected: compilation fails with `cannot find function root_from in this scope` and `cannot find function project_root in this scope`.

- [ ] **Step 3: Write the implementation**

Add to `crates/mcpls-core/src/hooks/identity.rs`, after `identity_hash`:

```rust
/// The checkout root enclosing `dir`: the nearest directory at or above it
/// holding an entry named `.git`, or `dir` itself when none does.
///
/// Both sides of the socket resolve this before hashing, so two sessions in
/// one checkout reach one endpoint however deep in it they started. The test
/// is an entry named `.git` rather than anything git reports, because that
/// entry marks a working tree: a linked worktree and a submodule each hold
/// one, so each is its own root, while `git rev-parse --git-common-dir`
/// would collapse every worktree of a repository onto one endpoint and serve
/// one tree's contents under another tree's name.
///
/// # Errors
///
/// Returns an error if `dir` cannot be canonicalized, which means it does
/// not exist or is not reachable.
pub fn project_root(dir: &Path) -> Result<PathBuf> {
    let canonical = dunce::canonicalize(dir).map_err(|e| Error::FileIo {
        path: dir.to_path_buf(),
        source: e,
    })?;
    let home = dirs::home_dir().map(|home| dunce::canonicalize(&home).unwrap_or(home));
    Ok(root_from(&canonical, home.as_deref()))
}

/// The walk itself, over an already canonical path.
///
/// Takes the home directory rather than reading it, for the same reason
/// [`user_component`] takes its raw value: the rule can then be exercised
/// without a test setting a process-global variable, which this workspace
/// cannot do at all.
///
/// Home and the filesystem root are never tested, only stopped at. A
/// dotfiles repository in the home directory would otherwise become the root
/// of every directory beneath it, which points a language server at the
/// whole of home.
fn root_from(canonical: &Path, home: Option<&Path>) -> PathBuf {
    for candidate in canonical.ancestors() {
        if candidate.parent().is_none() || home.is_some_and(|home| candidate == home) {
            break;
        }
        if candidate.join(".git").exists() {
            return candidate.to_path_buf();
        }
    }
    canonical.to_path_buf()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core root_from project_root`

Expected: all eight tests PASS.

- [ ] **Step 5: Run the full suite and lint**

Run: `devrun task verify`

Expected: PASS. `dirs` is already a workspace dependency (`Cargo.toml:26`), so no manifest change is needed.

- [ ] **Step 6: Commit**

```bash
git add crates/mcpls-core/src/hooks/identity.rs
git commit -m "feat(hooks): resolve a checkout root for identity"
```

---

### Task 2: both sides hash the root

**Files:**
- Modify: `crates/mcpls-core/src/lib.rs:588-601` (`canonicalized_cwd_identity`) and its caller at `crates/mcpls-core/src/lib.rs:769`
- Modify: `crates/mcpls-cli/src/main.rs:40-56` and `crates/mcpls-cli/src/main.rs:72-79`
- Modify: `crates/mcpls-cli/src/hook.rs:234-260` (`doctor` and `doctor_scanning`'s first report line)
- Test: `crates/mcpls-cli/tests/cli_integration.rs`
- Comment to update: `crates/mcpls-core/src/hooks/identity.rs:3-6`, the module doc, which says mcpls hashes its own startup working directory and the hook hashes `CLAUDE_PROJECT_DIR`. Both hash the checkout root enclosing those directories after this task, and the agreement stops resting on the host's choice of working directory.

**Interfaces:**
- Consumes: `project_root` from Task 1, re-exported through `crates/mcpls-core/src/hooks/mod.rs:17`.
- Produces: nothing new. `canonicalized_cwd_identity` is renamed to `canonicalized_root_identity` and its `PathBuf` becomes the checkout root rather than the working directory.

- [ ] **Step 1: Export `project_root`**

In `crates/mcpls-core/src/hooks/mod.rs:17`, extend the re-export:

```rust
pub use identity::{SocketIdentity, identity_for, identity_hash, project_root};
```

- [ ] **Step 2: Write the failing test**

Add to `crates/mcpls-cli/tests/cli_integration.rs`:

```rust
/// A session started in a subdirectory of a checkout derives the endpoint
/// the checkout's own root derives, which is what lets the two share one
/// backend.
#[test]
fn hook_doctor_reports_the_checkout_root_from_a_subdirectory() {
    let project = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let root = dunce::canonicalize(project.path()).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    let nested = root.join("crates").join("core");
    fs::create_dir_all(&nested).unwrap();

    let root_hash = mcpls_core::hooks::identity_hash(&root).unwrap();

    let mut cmd = Command::cargo_bin("mcpls").unwrap();
    let output = clear_ambient_env(&mut cmd)
        .env("CLAUDE_PROJECT_DIR", &nested)
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", runtime.path())
        .env("USER", "mcpls-test")
        .args(["hook", "doctor"])
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&output.stdout);

    assert!(
        report.contains(&root_hash),
        "doctor reported a hash other than the checkout root's: {report}"
    );
    assert!(
        report.contains(&format!("root: {}", root.display())),
        "doctor did not name the checkout root: {report}"
    );
}
```

`Command::cargo_bin("mcpls")` is how every test in this file reaches the built binary, and `clear_ambient_env` (`crates/mcpls-cli/tests/cli_integration.rs:27-32`) is mandatory: without it a developer's `MCPLS_CONFIG` changes the outcome. The three runtime-directory variables keep the doctor's foreign scan, which probes up to sixteen sockets with a timeout each (`crates/mcpls-cli/src/hook.rs:225`), away from the sockets of live mcpls processes on the machine.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo nextest run -p mcpls-cli hook_doctor_reports_the_checkout_root`

Expected: FAIL. The reported hash is the nested directory's, and the report has no `root:` line.

- [ ] **Step 4: Resolve the root on the MCP side**

In `crates/mcpls-core/src/lib.rs`, rename `canonicalized_cwd_identity` to `canonicalized_root_identity`, replace its body, and rewrite its doc comment:

```rust
/// The checkout root enclosing this process's working directory, and the
/// socket identity derived from it, from one resolution rather than two, so
/// a `Status` answer's `root` and `hash` describe the same directory by
/// construction rather than by coincidence.
fn canonicalized_root_identity() -> Result<(hooks::SocketIdentity, PathBuf), Error> {
    let dir = std::env::current_dir().map_err(Error::Io)?;
    let root = hooks::project_root(&dir)?;
    let identity = hooks::identity_for(&root)?;
    Ok((identity, root))
}
```

Update the caller at `crates/mcpls-core/src/lib.rs:769` to the new name. `project_root` canonicalizes, so the `dunce::canonicalize` call this body replaced is not lost.

The root this returns also reaches the hook sweeper, built from `HookLocation { identity, root }` (`crates/mcpls-core/src/lib.rs:897`), so sweeps cover the checkout rather than the subdirectory a session started in. That is the intent, not a side effect of renaming.

- [ ] **Step 5: Resolve the root on the hook side**

In `crates/mcpls-cli/src/main.rs`, in both the hook dispatch branch and the `Doctor` branch, resolve the root between canonicalizing and deriving the identity. The dispatch branch becomes:

```rust
                let project_dir = dunce::canonicalize(&raw_project_dir).unwrap_or(raw_project_dir);
                let root = mcpls_core::hooks::project_root(&project_dir)
                    .unwrap_or_else(|_| project_dir.clone());
                // A failed `identity_for` (an unreachable directory, or an
                // over-long socket path) must not suppress `SessionStart`:
                // that arm never touches the socket, which is the entire
                // reason it exists, so every socket-using arm degrades on
                // its own when `identity` is `None` rather than the whole
                // dispatch short-circuiting here.
                let identity = mcpls_core::hooks::identity_for(&root).ok();
                let out = hook::dispatch_payload(&stdin, &root, identity.as_ref()).await;
```

Keep the existing comment above `identity`, which still describes why the failure is tolerated. The `Doctor` branch resolves the root the same way and passes `&project_dir` to `hook::doctor`, which keeps its current parameter and resolves the root itself in the next step.

Passing `&root` rather than `&project_dir` to `dispatch_payload` is deliberate: the `SessionStart` arm answers with watch paths, and the watcher's business is the checkout rather than the subdirectory a session happened to start in.

- [ ] **Step 6: Report both directories in the doctor**

In `crates/mcpls-cli/src/hook.rs`, in `doctor_scanning`, replace the first report line at line 249 so it names the start directory, the root and the hash:

```rust
    let root = mcpls_core::hooks::project_root(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    let mut lines = vec![format!("hook sees: {}", project_dir.display())];
    lines.push(format!("root: {} -> {}", root.display(), identity.hash));
```

`doctor` resolves the root from the start directory rather than taking it as a parameter, because `project_root` is a pure function of that directory: two resolutions of one start cannot disagree, and twenty-two call sites keep their signature.

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo nextest run -p mcpls-cli hook_doctor_reports_the_checkout_root`

Expected: PASS.

- [ ] **Step 8: Run the full suite**

Run: `devrun task verify`

Expected: PASS. Existing doctor tests that assert on the old `hook sees: <dir> -> <hash>` line will fail; update each to the two-line form rather than loosening its assertion.

- [ ] **Step 9: Commit**

```bash
git add crates/mcpls-core/src/hooks/mod.rs crates/mcpls-core/src/lib.rs crates/mcpls-cli/src/main.rs crates/mcpls-cli/src/hook.rs crates/mcpls-cli/tests/cli_integration.rs
git commit -m "feat: hash the checkout root on both sides"
```

---

### Task 3: the workspace base is the root

**Files:**
- Modify: `crates/mcpls-core/src/lib.rs:663-673` (the workspace base)

**Interfaces:**
- Consumes: `hooks::project_root` from Task 1.
- Produces: nothing. The change is one call site.

This task adds no test. The behaviour it changes is `project_root`, tested in Task 1; what it adds is the decision to call it here, and the only test that could observe that decision would have to run `serve_with` with a mutated process working directory, spawning real language servers to read back a value the call site states in two lines. It stays its own commit because it changes which files a session can ask about, and a reviewer bisecting a report of servers rooted in the wrong place wants that change alone in one diff.

- [ ] **Step 1: Read what the base is today**

The base is `std::env::current_dir()` at `crates/mcpls-core/src/lib.rs:666`, reached only when `workspace.roots` is empty or holds at least one relative root:

```rust
    let workspace_roots = if config.workspace.roots.is_empty()
        || config.workspace.roots.iter().any(|root| root.is_relative())
    {
        let workspace_base = std::env::current_dir().map_err(Error::Io)?;
        resolve_workspace_roots(&config.workspace.roots, &workspace_base)?
    } else {
```

An empty `roots` therefore defaults to the working directory. A backend rooted at whichever subdirectory its spawning frontend sat in would put a second session's files outside every workspace root, which is why the base moves with the identity rather than after it.

- [ ] **Step 2: Anchor the base on the checkout**

Replace the two lines inside that branch:

```rust
        let cwd = std::env::current_dir().map_err(Error::Io)?;
        let workspace_base = hooks::project_root(&cwd)?;
        resolve_workspace_roots(&config.workspace.roots, &workspace_base)?
```

Then rewrite the comment above the branch. Its first sentence, that `current_dir()` always returns an absolute path, no longer describes the code, and the behaviour that replaces it needs stating:

```rust
    // Relative roots and the empty default anchor on the checkout enclosing
    // the working directory rather than on the working directory itself, so
    // two sessions of one checkout, started at different depths, see the
    // same files. Configs loaded from a TOML file ...
```

Keep the rest of the comment, about relative roots in a TOML file already being rebased and about a fully-absolute `workspace.roots` not paying for a `current_dir()` that may be unreadable.

Resist adding a `workspace_base` wrapper around `project_root`. A private function whose body is one call to a public function one module over is the one-line cast the project's instructions rule out, and its tests would re-run Task 1's under new names.

- [ ] **Step 3: Run the full suite**

Run: `devrun task verify`

Expected: PASS. The config tests that mutate the working directory (`crates/mcpls-core/src/config/mod.rs:2387` onward, through `crate::test_support::CwdGuard`) enter temporary directories, which hold no `.git` entry and sit outside the home directory, so `project_root` returns the start directory and their expectations do not move. If one of them fails, check whether the temporary directory it entered lies inside a checkout on this machine before changing the test. A failure in `crates/mcpls-core/src/lib.rs:1613` means the same thing.

- [ ] **Step 4: Commit**

```bash
git add crates/mcpls-core/src/lib.rs
git commit -m "feat: anchor workspace roots on the checkout"
```

---

### Task 4: the runtime directory stops reading `XDG_RUNTIME_DIR`

**Files:**
- Modify: `crates/mcpls-core/src/hooks/identity.rs:199-211` (`runtime_dir`)
- Modify: `crates/mcpls-cli/tests/cli_integration.rs`, every test that isolates a child's runtime directory (see Step 6)
- Test: `crates/mcpls-core/src/hooks/identity.rs`, the existing `mod tests`
- Comments to update: `crates/mcpls-core/src/lib.rs:611-613`, which says an env override would not do because the derivation already reads `XDG_RUNTIME_DIR`, and `plugin/README.md:66`, which tells a user with an over-long socket path to move `XDG_RUNTIME_DIR`. `TMPDIR` is the variable in both after this task.

**Interfaces:**
- Consumes: `shared_temp_runtime_dir(user: Option<String>) -> PathBuf`, already present at `identity.rs:225-230`.
- Produces: nothing. `runtime_dir` keeps its signature and loses a branch.

- [ ] **Step 1: Understand why the branch goes**

Codex launches a stdio MCP server with `env_clear()` and a fixed list that carries `HOME`, `USER`, `LOGNAME` and `TMPDIR` but not `XDG_RUNTIME_DIR` (`codex-rs/rmcp-client/src/utils.rs:122-134`), while its hook commands inherit the whole environment (`codex-rs/hooks/src/engine/command_runner.rs:191`). On any Linux host that sets the variable, a Codex frontend therefore binds under the system temporary directory while a Codex hook connects under `$XDG_RUNTIME_DIR/mcpls`, and the two never meet. No identity rule fixes that, so the variable goes.

- [ ] **Step 2: Write the failing test**

```rust
    /// The runtime directory reads only variables every host passes, so a
    /// server and a hook of one project agree on where the socket lives.
    /// `XDG_RUNTIME_DIR` is not one of them: one host drops it for an MCP
    /// server and keeps it for a hook command.
    #[cfg(not(windows))]
    #[test]
    fn test_runtime_dir_ignores_the_xdg_variable() {
        assert_eq!(runtime_dir(), shared_temp_runtime_dir(current_user()));
    }
```

The gate is not optional: `runtime_dir` and `shared_temp_runtime_dir` are both `#[cfg(not(windows))]` (`crates/mcpls-core/src/hooks/identity.rs:205`, `:224`), and this test module gates per test rather than as a whole (`:253`, `:278`), so an ungated test fails to compile on the Windows test job.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo nextest run -p mcpls-core test_runtime_dir_ignores_the_xdg_variable`

Expected: FAIL on a machine where `XDG_RUNTIME_DIR` is set, which includes this one. PASS anywhere it is unset, including GitHub's Ubuntu runners. The test guards against the branch returning; it cannot prove the branch was there, so confirm the failure by reading `runtime_dir` rather than trusting a green run.

- [ ] **Step 4: Delete the branch**

Replace `runtime_dir` and its doc comment:

```rust
/// Where sockets go on this platform: the system temporary directory,
/// `$TMPDIR` on macOS and `/tmp` on Linux, in a directory carrying the user.
///
/// `XDG_RUNTIME_DIR` would be the better directory, being a tmpfs the user
/// owns and cleaned when the session ends, and it cannot be used. Codex
/// launches a stdio MCP server with a cleared environment and a fixed list
/// that omits it, while its hook commands inherit the whole environment
/// (`codex-rs/rmcp-client/src/utils.rs:122-134`;
/// `codex-rs/hooks/src/engine/command_runner.rs:191`), so a server and a
/// hook of one project would look for the socket in two different places on
/// any host that sets it.
#[cfg(not(windows))]
fn runtime_dir() -> PathBuf {
    shared_temp_runtime_dir(current_user())
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo nextest run -p mcpls-core test_runtime_dir_ignores_the_xdg_variable`

Expected: PASS.

- [ ] **Step 6: Run the full suite**

Run: `devrun task verify`

Expected: FAIL in every `cli_integration.rs` test that sets `XDG_RUNTIME_DIR` to isolate a spawned child's runtime directory (`:143`, `:220`, `:259`, `:296`, `:358`, `:719`, `:749`, `:945`, `:1000`). The variable was never the subject of those tests. It was how they kept child processes off the developer's live sockets, and the replacement is the pair `std::env::temp_dir()` reads:

```rust
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", runtime.path())
        .env("USER", "mcpls-test")
```

Where a test builds the child's identity by hand (`:965-975`), change `runtime.path().join("mcpls")` to `runtime.path().join("mcpls-mcpls-test")` so the listener binds where the child now looks. The socket-length test at `:945` sets `TMPDIR` to its over-long path instead of `XDG_RUNTIME_DIR`, or its assertion on `socket: none; could not derive an identity` starts failing for want of a long directory. `test_doctor_identity_uses_current_user_without_xdg_runtime_dir` at `:915` keeps its assertion and loses an `env_remove` that is now a no-op. `crates/mcpls-core/tests/ra_e2e.rs:230` forwards the variable to a compiler wrapper rather than to mcpls and stays as it is.

- [ ] **Step 7: Commit**

```bash
git add crates/mcpls-core/src/hooks/identity.rs crates/mcpls-core/src/lib.rs crates/mcpls-cli/tests/cli_integration.rs plugin/README.md
git commit -m "fix(hooks): drop XDG_RUNTIME_DIR from the socket path"
```

---

### Task 5: the runtime directory is private

**Files:**
- Modify: `crates/mcpls-core/src/hooks/listener.rs:223-235` (where the runtime directory is created)
- Test: `crates/mcpls-core/src/hooks/listener.rs`, a new `#[cfg(all(test, not(windows)))] mod runtime_dir_tests` beside `probe_tests` at line 1126

**Interfaces:**
- Consumes: nothing new.
- Produces: `fn ensure_private_dir(dir: &Path) -> std::io::Result<()>`, Unix only, which creates a directory and sets it to `0o700`.

The file's `mod tests` at line 984 carries `#[cfg(windows)]` as well as `#[cfg(test)]`, so tests added there run nowhere on Linux and the RED step would report zero tests rather than a missing function. `probe_tests` at line 1126 is already gated `#[cfg(all(test, not(windows)))]` and is the pattern to copy.

- [ ] **Step 1: Read the current creation**

Read `crates/mcpls-core/src/hooks/listener.rs:220-260`. The directory is created with `create_dir_all` at the process umask, and nothing sets a mode. The socket's scoping therefore rests on the umask rather than on anything the code does, which the design's cross-user non-goal now names explicitly.

- [ ] **Step 2: Write the failing test**

```rust
#[cfg(all(test, not(windows)))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod runtime_dir_tests {
    use super::*;

    /// The runtime directory is private to its user. Nothing else keeps one
    /// user's socket out of another's reach on a shared temporary directory,
    /// and the design's cross-user non-goal rests on this.
    #[test]
    fn test_ensure_private_dir_sets_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent = tempfile::tempdir().expect("temp dir");
        let dir = parent.path().join("mcpls-someone");

        ensure_private_dir(&dir).expect("dir created");

        let mode = std::fs::metadata(&dir).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "mode was {:o}", mode & 0o777);
    }

    /// Creating a directory that already exists is not an error, because two
    /// processes of one project race to create it.
    #[test]
    fn test_ensure_private_dir_accepts_an_existing_dir() {
        let parent = tempfile::tempdir().expect("temp dir");
        let dir = parent.path().join("mcpls-someone");

        ensure_private_dir(&dir).expect("first create");
        ensure_private_dir(&dir).expect("second create");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo nextest run -p mcpls-core ensure_private_dir`

Expected: FAIL, with `cannot find function ensure_private_dir`.

- [ ] **Step 4: Write the implementation**

Add to `crates/mcpls-core/src/hooks/listener.rs`:

```rust
/// Create `dir` if it is missing and make it owner-only.
///
/// The socket lives in a directory every user on the machine shares, and its
/// name carrying the user is not on its own a boundary: another user can
/// pre-create the name, and a permissive umask leaves the socket connectable
/// by anyone. The mode is what makes the name's scoping hold. Setting it on
/// every call is deliberate: `chmod` on a directory this user owns is
/// idempotent, and a directory another user pre-created fails here with
/// `EPERM` rather than being bound into silently.
#[cfg(not(windows))]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}
```

The gate is on the function rather than inside it because its only caller, `lock_file`, is itself `#[cfg(not(windows))]` (`crates/mcpls-core/src/hooks/listener.rs:220`). An ungated function is dead code on Windows, and the Windows clippy job runs with `-D warnings` (`.github/workflows/ci.yml:95`), so it would fail the push while `devrun task verify` stayed green.

Then replace the `create_dir_all` call at line 230 with `ensure_private_dir`, keeping its existing error mapping.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p mcpls-core ensure_private_dir`

Expected: both PASS.

- [ ] **Step 6: Run the full suite**

Run: `devrun task verify`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/mcpls-core/src/hooks/listener.rs
git commit -m "fix(hooks): make the runtime directory owner-only"
```

---

### Task 6: a failed notify does not stop the pump

**Files:**
- Modify: `crates/mcpls-core/src/lib.rs:184-186` (the pump's `PublishDiagnostics` arm) and `crates/mcpls-core/src/lib.rs:218-278` (`handle_publish_diagnostics`)
- Test: `crates/mcpls-core/src/lib.rs`, the pump tests beginning at line 2662
- Comment to update: `crates/mcpls-core/src/lib.rs:148-151`, the `diagnostics_pump` doc, which lists a failing `notify_resource_updated` among the reasons the task exits. Two reasons remain after this task.

**Interfaces:**
- Consumes: the existing `PumpShared` at `crates/mcpls-core/src/lib.rs:120-130` and the test helpers `make_cache`, `make_subs`, `make_peer_cell`, `make_tracker`, `make_settle`, `make_delivery`, `make_floors` and `no_workspace_roots` at `crates/mcpls-core/src/lib.rs:2662-2703`.
- Produces: `handle_publish_diagnostics` returns `()` rather than `bool`.

- [ ] **Step 1: Understand the defect**

`handle_publish_diagnostics` returns `true` when the single peer's notify fails, and the pump treats `true` as its own death:

```rust
                        if handle_publish_diagnostics(&server_id, caches_diagnostics, p, &shared).await {
                            break;
                        }
```

One process serving one session can get away with that, because the peer leaving and the process ending are the same event. A process serving many sessions cannot: one session disconnecting would stop diagnostics caching for every other session attached to it. The fix removes the branch rather than changing its condition, so the pump has no way left to stop on a publish.

- [ ] **Step 2: Write the failing test**

The failure path needs a real peer whose notify fails, which `make_peer_cell` cannot give: an empty cell returns early and never notifies. Build one by starting a service with `serve_directly` and cancelling it, which leaves the peer holding a channel nobody receives on.

Do not reach for `ServiceExt::serve` here. For `RoleServer` it loops awaiting an `initialize` request before it returns (`rmcp-3.1.4/src/service/server.rs:497-500`), and this test sends none, so `serve(...).await` would never resolve and the step would hang rather than fail. `serve_directly` skips initialization and returns a `RunningService` directly, not a future and not a `Result` (`rmcp-3.1.4/src/service.rs:1265`). After `cancel()`, `Peer::send_notification` fails with `ServiceError::TransportClosed` because the receiver its channel feeds is gone (`rmcp-3.1.4/src/service.rs:826-832`).

Add to the pump tests in `crates/mcpls-core/src/lib.rs`:

```rust
        /// A bare handler, to obtain a real peer. Every `ServerHandler`
        /// method has a default, so this needs no body.
        #[derive(Debug)]
        struct BarePeerHandler;

        impl rmcp::ServerHandler for BarePeerHandler {}

        /// A peer whose service has already stopped, so every notification
        /// it sends fails.
        async fn broken_peer() -> rmcp::Peer<rmcp::RoleServer> {
            let (server_io, _client_io) = tokio::io::duplex(1024);
            let running = rmcp::service::serve_directly(BarePeerHandler, server_io, None);
            let peer = running.peer().clone();
            running.cancel().await.expect("the bare service stops");
            peer
        }

        /// A peer that cannot be notified is one connection's problem. The
        /// pump carries every connection's diagnostics caching, so it keeps
        /// running and keeps caching after a notify fails.
        #[tokio::test]
        async fn test_pump_keeps_caching_after_a_failed_notify() {
            let cache = make_cache();
            let subs = make_subs();
            let peer_cell = make_peer_cell();
            peer_cell
                .set(broken_peer().await)
                .expect("peer cell is empty");

            let first: Uri = "file:///test/first.rs".parse().unwrap();
            let second: Uri = "file:///test/second.rs".parse().unwrap();
            // Subscribed, so the pump reaches the notify rather than
            // returning at the subscription check.
            subs.subscribe(make_uri(std::path::Path::new("/test/first.rs")).unwrap())
                .await
                .expect("subscribe");

            let (tx, rx) = mpsc::channel(8);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let c = Arc::clone(&cache);
            tokio::spawn(diagnostics_pump(
                ServerId::from("rust"),
                rx,
                cancel_rx,
                true,
                PumpShared {
                    notification_cache: c,
                    subs: Arc::clone(&subs),
                    peer_cell: Arc::clone(&peer_cell),
                    workspace_roots: no_workspace_roots(),
                    document_tracker: make_tracker(),
                    settle: make_settle(),
                    delivery: make_delivery(),
                    floors: make_floors(),
                },
            ));

            for uri in [&first, &second] {
                tx.send(LspNotification::PublishDiagnostics(
                    PublishDiagnosticsParams {
                        uri: uri.clone(),
                        diagnostics: vec![],
                        version: None,
                    },
                ))
                .await
                .unwrap();
            }
            drop(tx);

            let cached = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    tokio::task::yield_now().await;
                    let found = {
                        let guard = cache.lock().await;
                        guard.get_diagnostics(second.as_str()).is_some()
                    };
                    if found {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the pump stopped after the first notify failed");
            assert!(cached, "the publish after a failed notify was not cached");
        }
```

`ResourceSubscriptions::subscribe` takes an owned `String` and returns `Result<bool, String>` (`crates/mcpls-core/src/bridge/resources.rs:146`); `make_uri` returns `Result<String, ResourceUriError>` (`:68`) and is already imported at `crates/mcpls-core/src/lib.rs:56`. No pump test subscribes anything today, so this is the first one that does.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo nextest run -p mcpls-core test_pump_keeps_caching_after_a_failed_notify`

Expected: FAIL with `the pump stopped after the first notify failed`. The first publish notifies a dead peer, `handle_publish_diagnostics` returns `true`, the pump breaks, and the second publish never reaches the cache.

- [ ] **Step 4: Remove the branch**

In `handle_publish_diagnostics`, change the signature to return nothing, change every `return false` to a bare `return`, and replace the final expression:

```rust
    if let Err(error) = peer
        .notify_resource_updated(ResourceUpdatedNotificationParam::new(mcp_uri))
        .await
    {
        tracing::debug!(%error, "a subscriber could not be notified");
    }
```

Delete the comment above it, which describes a return value that no longer exists. Then simplify the pump's arm:

```rust
                    LspNotification::PublishDiagnostics(p) => {
                        handle_publish_diagnostics(&server_id, caches_diagnostics, p, &shared).await;
                    }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo nextest run -p mcpls-core test_pump_keeps_caching_after_a_failed_notify`

Expected: PASS.

- [ ] **Step 6: Run the full suite**

Run: `devrun task verify`

Expected: PASS. A test asserting the pump stops on a failed notify is asserting the defect; rewrite it to assert the pump continues.

- [ ] **Step 7: Commit**

```bash
git add crates/mcpls-core/src/lib.rs
git commit -m "fix: keep the pump alive when a notify fails"
```

---

## What this plan does not cover

**Per-connection resource subscriptions.** One subscription set and one write-once peer serve the whole process today (`crates/mcpls-core/src/bridge/resources.rs:123`; `crates/mcpls-core/src/lib.rs:127`, set at `crates/mcpls-core/src/transport.rs:339`), so a process serving many connections has one shared set and can notify only whichever connection arrived first. Fixing it means a registry keyed by connection, a fan-out in the pump, and threading a connection id into the `resources/subscribe` handlers in a file of five thousand lines. That is a plan of its own rather than a task, and it belongs with the split, which is what first makes a second connection possible. Task 6 above is the half of it that stands alone, because a pump that stops on one peer's failure is a defect whether or not anything else changes.

**Project configuration is still found at the working directory.** `ServerConfig::load_with_trust` looks for `./mcpls.toml` relative to the process working directory (`crates/mcpls-core/src/config/mod.rs:956`), and Task 3 does not move it. A user who starts a session in a subdirectory therefore gets servers anchored on the checkout while the `mcpls.toml` at that checkout goes unread, which is exactly the file the identity ruling points a monorepo user at for `workspace.roots`. Until discovery moves, the global config is the place for those roots. Moving it is a change to upstream code with its own trust implications and gets a task in the split plan.

**The split itself.** The frontend and its stub service, the detached backend, the frozen handshake line and the configuration fingerprint it carries, the spawn lock, idle shutdown and its ordering, `--no-backend`, the doctor's backend report, session identity arriving from the handshake, and the deletion of the owner, passive, demotion and forwarding paths. None of it is startable until the endpoint's name and the runtime directory are settled, which is what the six tasks above settle.

Stages 2 to 4 of the design each get their own plan after that.
