# Diagnostic source preservation implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Implement the task with TDD, then complete task review and whole-branch review.

**Goal:** Resolve issue #2 by preserving distinct producer reports in `get_diagnostics`.

**Architecture:** Deduplicate identical MCP diagnostic records across pull and cached push reports using the DTO's existing full-field equality. Preserve differing records, including their source, range, severity, code, and message. Retain the existing cache timing, fallback, and stable position ordering.

**Tech Stack:** Rust, Tokio, lsp-types, rmcp, cargo-nextest, devkit.

**Spec:** https://github.com/AbysmalBiscuit/mcpls/issues/2, captured in `/home/lev/Git/lev/mcpls_worktrees/ISSUE_SUMMARY_2.md`. This plan selects the issue's explicit keep-both alternative.

## Global Constraints

- Preserve every distinct diagnostic record; only full equality permits cross-model collapse.
- Treat source as opaque provenance. Do not rank source names or infer freshness from pull versus push.
- Preserve cache reads after the pull request, nonempty-cache fallback on pull failure, and eventually consistent cache behavior.
- Keep the current response schema and stable ordering by start position; tied positions retain pull-before-cache order.
- Keep existing dependency versions, cancellation-safe transport, and compiler wrapper configuration.
- Tests must exercise actual MCP `tools/call` dispatch, LSP response parsing, and both text and structured output. Seeding the notification cache is permitted because publication ingestion is outside this bug.
- Use devkit tasks and file claims. Work only in the issue worktree. No sudo, stash, Windows commands, daily-driver installation, upstream changes, or merges.

---

### Task 1: Preserve distinct pull and push diagnostics

**Files:**
- Modify: `crates/mcpls-core/src/bridge/translator/diagnostics.rs`
- Modify: `crates/mcpls-core/src/mcp/server.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: `Translator::handle_diagnostics`, `Translator::merge_diagnostics`, `NotificationCache::store_diagnostics`, the existing `Diagnostic` equality, and the `test_diagnostics_structured_output_preserves_source` MCP wire fixture.
- Produces: unchanged `DiagnosticsResult` and tool schemas, with distinct diagnostic records preserved.

**Binding behavior:** Preserve every distinct diagnostic record; collapse only full equality across the original pull report and cached push report. Source is opaque. Keep cache timing/fallback and stable position sorting unchanged. Preserve dependency versions and wrapper configuration.

- [ ] **Step 1: Write the real-entry-point regression before changing production code.**

Use the MCP wire fixture around `test_diagnostics_structured_output_preserves_source`. A real temporary Rust file must have enough lines for the test spans. Seed cached diagnostics under the canonical URI, then issue `tools/call` with `get_diagnostics`, return a full pull report over the existing LSP pipe fixture, and compare both result encodings against complete expected records. Reuse or minimally extract fixture setup instead of copying the entire handshake for every case.

The primary payloads are an E0046 error from rust-analyzer and an E0046 error from rustc with distinct messages and adjacent spans. The request and assertions must retain this shape:

```rust
json!({
    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
    "params": {"name": "get_diagnostics", "arguments": {"file_path": path}}
})
```

```rust
assert!(response["error"].is_null(), "{response}");
assert_eq!(response["result"]["isError"], false, "{response}");
let text: serde_json::Value = serde_json::from_str(
    response["result"]["content"][0]["text"].as_str().unwrap(),
).unwrap();
assert_eq!(text, expected);
assert_eq!(response["result"]["structuredContent"], expected);
```

Exercise this compact case matrix through the same MCP fixture:

| Pull versus cache | Expected result |
| --- | --- |
| E0046, rust-analyzer versus rustc, adjacent spans and different messages | Both complete reports |
| Equal coded records, including equal source | One report |
| Equal codeless records, including equal source | One report |
| Identical otherwise, different arbitrary source names, with and without code | Both reports |
| Codeless record with absent source versus named source | Both reports |
| Equal source/code/severity, different message or adjacent range | Both reports |
| Distinct records at the same position | Pull first, then cache |
| Older cached report and differing fresh pull report | Both reports; no freshness claim |
| Cache replaced with an empty report, followed by another call | Only the fresh pull report |

The first and nearby-error cases must fail on the old filter for lost records. Cases covering already-correct behavior pin the selected policy and need not be artificially made red.

- [ ] **Step 2: Run RED and retain the assertion failure.**

```sh
devrun -C /home/lev/Git/lev/mcpls_worktrees/diagnostic-source --config /home/lev/Git/lev/mcpls_worktrees/diagnostic-source/.devkit/validation.toml task issue2-focused --env-file /home/lev/Git/lev/mcpls_worktrees/diagnostic-source/.devkit/test.env
```

Confirm failure is loss of the cached diagnostic through the MCP response, not setup, compilation, path validation, or a timeout.

- [ ] **Step 3: Apply the minimal production change and explain the public behavior.**

Remove `DUPLICATE_RANGE_PROXIMITY_LINES` and its proximity helpers. Keep conversion, append, and sorting. Filter cached records with the existing derived equality:

```rust
let new_diagnostics: Vec<_> = cached
    .into_iter()
    .filter(|candidate| !pull.diagnostics.contains(candidate))
    .collect();
pull.diagnostics.extend(new_diagnostics);
pull.diagnostics
    .sort_by_key(|d| (d.range.start.line, d.range.start.character));
```

Replace the merge API documentation's fuzzy identity explanation with exact cross-model equality and source preservation. Update the old fuzzy E0046 test to require both records, and remove the distant-error test's obsolete proximity explanation. Add an Unreleased changelog entry documenting preserved producer reports and the resulting possibility of multiple reports for one logical problem. Keep historical changelog entries unchanged.

- [ ] **Step 4: Verify GREEN and the workspace checks.**

Run the same focused command and record its result. Then run devkit `verify`, `docs`, and `build` with `.devkit/test.env`; run `issue2-e2e` from the local validation config after the build. Existing fallback tests must remain passing. Observe meaningful source and payload assertions, not just counts.

- [ ] **Step 5: Self-review and commit the task.**

Claim source files before editing, inspect the staged diff, and commit only this task's source/tests/changelog with a conventional subject such as `fix(diagnostics): preserve distinct sources`. The controller owns the plan and progress ledger. Write the implementation report with RED/GREEN commands, relevant output, commit IDs, changed files, and any concerns to the report path provided by the controller. Do not push or open a PR from the implementer.

## Unresolved questions

None. The selected tradeoff is that distinct producer reports can describe one logical problem. Changing cache freshness or introducing aggregate provenance is outside issue #2.
