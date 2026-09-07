# Delivery acknowledgement implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop a hook flush from marking diagnostics delivered before the hook has them, so a hook that gives up on its deadline or is killed before it prints costs the agent a repeat rather than a permanent, silent loss.

**Architecture:** `DiagnosticsDelivery::flush` splits into `stage`, which computes the report and parks the record changes it implies under a token, and `commit`, which applies them when that token comes back. The hook socket's `flush` answer carries the token; the hook sends an `ack` on the same connection once the answer is in hand; only that `ack` commits. The MCP tool and the footer keep advancing immediately, because their transport is the session's own and losing it ends the session. The owner never waits for an acknowledgement: a staged report nobody acknowledges is simply replaced by the session's next `stage`, which diffs against the committed record and so offers it again.

**Tech Stack:** Rust edition 2024, MSRV 1.88, tokio (`features = ["full"]`), `serde`/`serde_json`. Tests run under `cargo nextest run`.

**Spec:** `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`.

**Where this plan extends the spec rather than implementing it.** The spec's Protocol section pins three `Request` lines (`changed`, `flush`, `status`) and says `flush` "drains the delivery record". It does not describe an acknowledgement, a token on the `flush` answer, or a staged record; it assumes the answer the owner writes is the answer the hook prints. That assumption is what this plan removes. The `ack` op, the `token` field, `DiagnosticsDelivery::stage`/`commit`, and the rule that only the socket door defers its advance are extensions. Task 3 amends the spec's Protocol section so the spec and the code agree again. The spec's other rules the plan argues from stay as written: one report per problem (its "A footer consumes" paragraph), the delivery-before-cache lock order, `op_deadline_ms = 1500`, and the silent-failure rule for hooks.

## Global constraints

- Rust edition 2024, MSRV 1.88. Clippy runs with pedantic and nursery; `unwrap_used` and `expect_used` warn; `missing_docs` warns. The work must leave `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean. `--all-features` is not optional: `transport-http` is an optional feature CI enables everywhere it builds.
- `cargo fmt --check` must be clean at the commit. Task 3 runs `cargo fmt --check && cargo clippy --workspace --all-targets --all-features -- -D warnings` as its own step after its tests pass and before the commit step.
- Every `#[cfg(test)] mod tests` block this plan writes into already opens with `#[allow(clippy::unwrap_used, clippy::expect_used)]`; no new test module is created.
- Lock order is `context.delivery` before `context.notification_cache`, both `tokio::sync::Mutex`. A site that needs only `delivery` takes only `delivery`. No `std::sync::Mutex` guard is held across an `.await`; the only `std` mutexes on this path are `HookRole::role` (a clone, then dropped) and the sweeper's `last_shortfall` (a clone, then dropped), and neither changes. Where every lock sits is spelled out under "Where the locks sit" below, and each task's code matches it.
- Every fault in the hook path exits 0 having printed nothing. An acknowledgement that fails must not change that, and must not withhold a report the hook already has: the acknowledgement is sent after the flush answer is in hand and its outcome is ignored.
- The response is read as exactly one NDJSON line per request, in arrival order, by position. The acknowledgement keeps that: it is a request and its answer is read like any other.
- Comments are timeless: present tense, no reference to this plan, its tasks, the defect's history, or the TDD cycle. A comment describes what the code does and why, for a reader who knows nothing about how it got there.
- The commit follows Conventional Commits: `type(scope): description`, imperative, 50 characters or fewer including the prefix, no trailing period, lowercase after the colon, body wrapped at 72 columns, ending with the two trailer lines `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01PZTxadnYDr7Eh3XuzZzc9y`. The commit step spells the message with `git commit -F -` and a quoted heredoc. Commits are GPG signed; if signing fails, stop and report rather than passing any signing override.
- Stage selectively: `git add` names each file this plan touches, never a whole top-level directory.
- Every test that builds a filesystem path builds it with a drive letter on Windows, the way the existing helpers (`broken_uri`, `temp_identity`) already do; the new tests reuse those helpers rather than building paths of their own.
- Work happens in the worktree at `/home/lev/Git/lev/mcpls-diag-bc` on branch `feat/diagnostics-stage-bc`. Never change the branch checked out anywhere; never use `git stash`. Always pass absolute paths to `git -C`.
- This is one commit at the tip of the branch. Tasks 1 and 2 end with their tests passing and no commit; Task 3 commits everything.
- Line numbers in this plan are against `b3e13d0`, the branch tip when it was written. The `mcpls hook doctor` work in flight in this worktree (uncommitted at the time of writing; `ProbeOutcome::Unintelligible`, status error answers) shifts `crates/mcpls-cli/src/hook.rs` from `:267` onward and `crates/mcpls-core/src/hooks/listener.rs` from `:532` onward, and factors the client framing into a free `exchange(stream, requests)` that Task 2 Step 10 builds on. Land that work first, then locate every cited range by the symbol named beside it rather than by the number.

---

## File structure

Every file a task modifies, so a worker sees the whole footprint before starting.

- `crates/mcpls-core/src/bridge/delivery.rs`: `DiagnosticsDelivery` gains `pending` and `next_token`; `flush` becomes `stage` + `commit`, with `flush` kept as the immediate form. `flush`'s total-budget guard also stops using `delivered_any` as a proxy for an untouched budget. Task 1.
- `crates/mcpls-core/src/hooks/protocol.rs`: `Response::Flush` gains `token`; `Request::Ack` and `Response::Ack` are added; the pinned literals change. Task 2.
- `crates/mcpls-core/src/hooks/listener.rs`: the client half grows a `Connection` that can run more than one exchange, and `send_and_acknowledge`; the deadline overrun message becomes per-op. Task 2.
- `crates/mcpls-core/src/hooks/mod.rs`: re-exports `send_and_acknowledge`. Task 2.
- `crates/mcpls-core/src/hooks/service.rs`: the `Flush` arm carries the token; an `Ack` arm commits; the harness gains `flush_acknowledged`; three tests change and two are added. Task 2.
- `crates/mcpls-core/src/mcp/server.rs`: `Advance`, `flush_now` takes it, `flush_for_hook` returns the token, `commit_for_hook` is added, `flush_from_owner` acknowledges; one test is added. Task 2.
- `crates/mcpls-core/tests/hooks_socket.rs`: `Response::Flush` literals gain `token`; the deadline test pins the flush overrun text; one test is added for the acknowledgement on one connection. Task 2.
- `crates/mcpls-cli/src/hook.rs`: `PostToolBatch` and `UserPromptSubmit` go through `send_and_acknowledge`; the recording owner answers `Ack` and can hang up after a flush; three tests change and two are added. Task 2 makes it compile; Task 3 does the rest.
- `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`: the Protocol section gains the `ack` line and the staged-record sentence. Task 3.

## Where the locks sit

- `DiagnosticsDelivery::stage` and `::commit` are synchronous methods on a value that lives behind `context.delivery`. They take no lock themselves.
- `McplsServer::flush_now` (Task 2) takes `context.delivery`, then `context.notification_cache`, runs `stage` or `flush` and `source_map` under both, and drops both before it awaits `new_diagnostics_payload`. This is the shape it has today (`crates/mcpls-core/src/mcp/server.rs:1060-1070`); only what runs inside the block changes.
- `McplsServer::commit_for_hook` (Task 2) takes `context.delivery` alone, calls `commit`, and drops it. It never touches the cache.
- The socket handler's `Ack` arm (Task 2) reaches `delivery` only through `commit_for_hook`. The `build_handler` doc comment already says no arm takes a lock itself; that stays true.
- Nothing holds any guard while a socket write or read is awaited. The staged report lives inside `DiagnosticsDelivery`, so between the `flush` answer and the `ack` no task is waiting and no lock is held.
- The client side (`send_and_acknowledge`) holds no lock of any kind.

## Why a repeat is the worst outcome

The invariant every interleaving below reduces to:

> The session record only ever takes a hash that a reader confirmed having in hand, or that the tool or footer door put into its own response. `stage` writes nothing to the record. `commit` writes exactly the updates the acknowledged report carried, and only if that report is still the session's staged one. `flush` (the immediate form) stages and commits in one synchronous call under the delivery lock.

So a hash reaches the record by one of two routes: an acknowledged hook report, or a tool/footer response. Neither route can record a hash no one was sent. What remains to check is the reverse: whether a hash someone was sent can fail to be recorded, which is where repeats come from, and whether a hash can be recorded for a reader that never printed it, which is where losses would come from.

Each case names the record the reader ends up with, the worst thing the agent sees, and the test that pins it.

1. **Hook flush, hook dies or times out before reading the answer.** `stage` parked updates `P` under token `t`; nothing acknowledges. The record is unchanged. The next `stage` for that session diffs against the unchanged record and reports the same files again, replacing `P`. Worst case: the agent sees the report once, on the later flush. Pinned by `test_an_unacknowledged_flush_is_offered_again` (service.rs) and `test_a_staged_report_is_offered_again_until_it_is_acknowledged` (delivery.rs).

2. **Hook flush, the owner's `op_deadline` detaches the handler.** The client gets `Response::Error` at the deadline and never acknowledges. The handler later runs `stage`, parking `P`. Same as case 1: the next `stage` replaces `P` and offers the report. This is what makes the overrun message for `flush` true. Pinned by the deadline test in `hooks_socket.rs`, which now asserts the flush overrun text.

3. **Tool flush racing a staged hook flush, one session.** Hook `stage` parks `P(t)`. Before the ack, the tool's `flush` runs: it stages again (replacing `P` with `P'(t')`, same updates because the record has not moved) and commits `t'` in the same call. The record now holds the tool's hashes; the tool result shows them. The hook's ack for `t` arrives: `commit` finds no pending for `t` and returns `false`. Record consistent, held once. The agent sees the diagnostics in the tool result and, if the hook printed, in the hook context too. Worst case: a duplicate. If the tool's flush ran first and the hook's `stage` second, the hook's diff against the advanced record is empty and carries no token; the agent sees them once. Pinned by `test_an_immediate_flush_supersedes_a_staged_report` (delivery.rs) and `test_an_immediate_flush_supersedes_a_staged_hook_report` (server.rs, through the real lock block).

4. **Two hook processes flush the same session concurrently.** `H1` stages `P1(t1)`; `H2` stages `P2(t2)`, replacing `P1`. Both answers carry the same delta if the cache did not move between them, or `P2` carries the newer hashes if it did. Ack `t1` arrives: no pending under `t1`, `false`, nothing written. Ack `t2` arrives: `P2` commits. The record holds exactly what `H2`'s reader was sent. If only `t1` arrives (`H2` died), nothing commits and the next flush offers the report again: the agent saw it via `H1` and sees it once more. If only `t2` arrives, the record is right and `H1`'s reader may also have printed it: a duplicate. If the cache moved between the two stages so that `H1` was sent `X@h1` and `H2` was sent `X@h2`, the record ends at `h2`, which is what `H2` printed; `X@h1` was printed by `H1` and superseded, not lost. Worst case in every branch: a duplicate. Pinned by `test_an_acknowledgement_for_a_replaced_report_commits_nothing` (delivery.rs), which stages two different deltas and proves the record ends at the acknowledged one.

5. **An ack arriving after a newer stage replaced its pending.** Case 4's `t1` branch in isolation: `commit` compares tokens and writes nothing. The newer pending is the one whose acknowledgement matters. Worst case: the newer report is printed and acknowledged (once) or not (offered again). Same test as case 4.

6. **`end_session` between stage and ack.** `end_session` drops the record and the pending. The ack finds nothing and returns `false`. The session is over; there is no reader to lose anything. Pinned by `test_ending_a_session_drops_its_staged_report` (delivery.rs).

7. **The ack is written but its answer is not read before the client deadline.** `send_and_acknowledge` wraps only the connect and the flush exchange in the deadline-gated region that can return `Err`. The ack exchange runs after the answers are in hand, under the same deadline but with its outcome discarded. So a deadline landing between the ack write and the ack read leaves the owner committed and the hook printing: exactly once. A deadline landing before the ack write leaves the owner uncommitted and the hook printing: a repeat next time. Pinned by `test_a_refused_acknowledgement_still_injects_the_context` (hook.rs), which has the owner hang up after the flush answer.

8. **A stage that finds nothing to report while an older pending exists.** The cache moved back to match the record (a file broke and was fixed before anyone saw it). `stage` removes the older pending and returns no token. A late ack for the older report writes nothing. Without the removal the late ack would record the broken hash and the next flush would print a spurious "no diagnostics" line. Not a loss either way; the removal avoids the noise. Pinned by the `token == None` assertion in `test_a_staged_report_is_offered_again_until_it_is_acknowledged`.

9. **Passive instance forwarding a flush.** The passive is a client with the same abandoning timeout, so it acknowledges after the owner's answer is in hand and before it returns the tool result. If the forward fails, the owner's pending stays unacknowledged and is offered again; the passive says the owner could not be reached, as it does today. Pinned by the existing `test_a_passive_instance_forwards_its_flush_to_the_owner`, which now proves a second forward reports nothing because the first was acknowledged.

**Where it can still lose, and what it costs.**

- The hook is killed after its ack line has left it and before the host has its answer. The diagnostics in that report are not offered again until their file's hash changes. This is the irreducible residual: the owner cannot see past the hook's exit, and nothing short of a host-side receipt would close it. Its exact extent, and why the ack sits where it does, follow.

**Why the ack is sent before the print, and not after.** The host's interrupt path (unresolved question 1, verified) is `SIGTERM` to the hook's process group and a discard of anything the hook wrote. The host does not act on a hook's stdout as it arrives; it collects it and parses it once the process exits. `crates/mcpls-cli/src/main.rs:56-64` writes the string `dispatch_payload` returned and then exits. So "the host has the answer" is decided at process exit, not at the `write` call, and a write that succeeded into the pipe buffer is discarded like any other if `SIGTERM` lands before exit. Loss therefore needs exactly one thing: the kill landing after the ack line has left the hook and before the hook exits. Under the plan's ordering that interval is the ack round trip, one JSON serialization, one `write`, and process teardown. Under print-then-ack it is the ack round trip and process teardown. The reorder moves the window later and shortens it by the serialization and the write, which are the smallest terms in it; the round trip and the teardown, which dominate, are the same either way. `SIGTERM` arrives on a human timescale, uniformly random against the hook's microsecond-scale sequence, so exposure is proportional to the interval's length and to nothing else; there is no moment in the hook's life at which the kill is more likely. In particular the print cannot draw the kill: nothing the hook writes reaches the user until after the hook exits, so the interrupt that kills it was triggered by the turn, not by the answer. Case 7 (a client deadline landing between the ack write and the ack read) is unaffected by the order: the print never depends on the ack's fate under either, and the ack line either left the process or it did not.

What the reorder would cost is structural: the ack has to leave `send_and_acknowledge`, cross the `dispatch_payload` boundary into `main`, and carry an open `Connection` with it, which makes that type public; and the passive instance cannot do it at all, because its "print" is `rmcp` sending the tool result after `flush_from_owner` returns. A hook's life is on the order of single-digit milliseconds (spawn, connect, the owner's flush, the ack) and the loss window on the order of a hundred microseconds; an interrupt that lands during a hook's life lands in the window a percent or two of the time, and the reorder changes that by a fraction of a percent. These are estimates from the shape of the code, not measurements. The one change that would shrink the dominant term, writing the ack and exiting without reading its answer, is rejected on the same grounds: it saves one local round trip and gives up one-answer-per-request and the harness's "the ack answered, so the commit is done" property. The plan keeps ack-then-print. If a host-side receipt ever exists, it belongs here; until then the ordering is a wash and the simpler shape wins.
- The passive forward's tool result is lost on the MCP transport after the ack. Same class as the owner's own tool door today, and the same cost.
- A client that never acknowledges (a `mcpls hook` from a build without this change against an owner with it, which can only happen during an upgrade with a live owner, since `plugin/hooks/hooks.json` runs `mcpls hook` from the same `PATH` the host spawns the server from) is offered the same report on every flush until the owner restarts. The cost is repeated context, not loss.
- A muted file (severity floor `off`) that had a record entry produces an update with no report line, so a flush can carry a token and no context. The client acknowledges anyway. Harmless: the commit forgets the entry, which is all it was going to do.

---

## Task 1: stage and commit in the delivery core

**Files:**
- Modify: `crates/mcpls-core/src/bridge/delivery.rs:106-282` (the struct and `flush`), tests at the bottom of the same file.

**Interfaces:**
- Consumes: `FileEntry`, `FlushReport`, `ChangedFile`, `SessionId`, `SeverityFloor` as they exist in the file today.
- Produces:

```rust
impl DiagnosticsDelivery {
    /// Report what changed for `session` since its last committed flush,
    /// and stage the record changes that report implies under a token.
    /// `None` when there is nothing to commit.
    pub fn stage(&mut self, session: &SessionId, entries: &[FileEntry<'_>]) -> (FlushReport, Option<u64>);

    /// Apply the changes staged under `token`, if that is still the
    /// session's staged report. `false` when it is not.
    pub fn commit(&mut self, session: &SessionId, token: u64) -> bool;

    /// `stage` and `commit` in one call: for a reader whose answer either
    /// arrives or ends the session.
    pub fn flush(&mut self, session: &SessionId, entries: &[FileEntry<'_>]) -> FlushReport;
}
```

- [ ] **Step 1: write the failing tests**

Append to the existing `mod tests` in `crates/mcpls-core/src/bridge/delivery.rs`, after `test_sub_floor_churn_does_not_report_a_file_with_nothing_to_show`. The helpers `diagnostic` and `entry` already exist in that module.

```rust
    #[test]
    fn test_a_staged_report_is_offered_again_until_it_is_acknowledged() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let entries = [entry("a.rs", &diags, SeverityFloor::Warning)];

        let (first, _) = delivery.stage(&session, &entries);
        assert_eq!(first.changed.len(), 1);

        let (second, token) = delivery.stage(&session, &entries);
        assert_eq!(
            second.changed.len(),
            1,
            "nothing confirmed the first report reached its reader, so it is \
             offered again rather than marked delivered"
        );

        assert!(delivery.commit(&session, token.expect("a report with content carries a token")));

        let (third, token) = delivery.stage(&session, &entries);
        assert!(third.changed.is_empty(), "the acknowledged report is not offered again");
        assert_eq!(token, None, "nothing to commit means nothing to acknowledge");
    }

    #[test]
    fn test_an_acknowledgement_for_a_replaced_report_commits_nothing() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let one = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let two = vec![
            diagnostic(1, DiagnosticSeverity::ERROR, "boom"),
            diagnostic(2, DiagnosticSeverity::ERROR, "bang"),
        ];

        let (_, stale) = delivery.stage(&session, &[entry("a.rs", &one, SeverityFloor::Warning)]);
        let (_, current) = delivery.stage(&session, &[entry("a.rs", &two, SeverityFloor::Warning)]);

        assert!(
            !delivery.commit(&session, stale.expect("token")),
            "a later report replaced this one; committing it would record a \
             hash its reader was never sent"
        );
        assert!(delivery.commit(&session, current.expect("token")));

        let (after_two, _) = delivery.stage(&session, &[entry("a.rs", &two, SeverityFloor::Warning)]);
        assert!(
            after_two.changed.is_empty(),
            "the record holds the acknowledged report's hash"
        );
        let (after_one, _) = delivery.stage(&session, &[entry("a.rs", &one, SeverityFloor::Warning)]);
        assert_eq!(
            after_one.changed.len(),
            1,
            "and not the replaced report's hash"
        );
    }

    #[test]
    fn test_an_immediate_flush_supersedes_a_staged_report() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let entries = [entry("a.rs", &diags, SeverityFloor::Warning)];

        let (_, staged) = delivery.stage(&session, &entries);
        let now = delivery.flush(&session, &entries);
        assert_eq!(
            now.changed.len(),
            1,
            "the tool door reports what the hook door has not yet confirmed"
        );

        assert!(
            !delivery.commit(&session, staged.expect("token")),
            "the flush already advanced the record, so a late acknowledgement \
             has nothing left to apply"
        );

        let (after, token) = delivery.stage(&session, &entries);
        assert!(after.changed.is_empty());
        assert_eq!(token, None);
    }

    #[test]
    fn test_ending_a_session_drops_its_staged_report() {
        let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig::default());
        let session = SessionId::from("s".to_string());
        let diags = vec![diagnostic(1, DiagnosticSeverity::ERROR, "boom")];
        let entries = [entry("a.rs", &diags, SeverityFloor::Warning)];

        let (_, token) = delivery.stage(&session, &entries);
        delivery.end_session(&session);

        assert!(!delivery.commit(&session, token.expect("token")));
        let (again, _) = delivery.stage(&session, &entries);
        assert_eq!(
            again.changed.len(),
            1,
            "a session that starts over starts from the baseline, not from a \
             report its previous life never confirmed"
        );
    }
```

- [ ] **Step 2: run the tests and watch them fail**

Run: `cargo nextest run -p mcpls-core bridge::delivery`
Expected: FAIL to compile, `no method named `stage``.

- [ ] **Step 3: implement**

Replace the struct at `crates/mcpls-core/src/bridge/delivery.rs:106-112` with:

```rust
/// The record changes one staged report implies, held back until the
/// reader confirms the report reached it.
#[derive(Debug)]
struct PendingFlush {
    token: u64,
    /// `Some(hash)` records the file as delivered at that hash; `None`
    /// forgets it, for a file that cleared or was muted.
    updates: Vec<(String, Option<u64>)>,
}

/// Per-session records of what has already been delivered.
#[derive(Debug)]
pub struct DiagnosticsDelivery {
    config: DiagnosticsConfig,
    sessions: HashMap<SessionId, HashMap<String, u64>>,
    baseline: Option<HashMap<String, u64>>,
    /// At most one staged report per session. Never names a session
    /// `sessions` lacks: `stage` seeds the record before it stages, and
    /// `end_session` drops both.
    pending: HashMap<SessionId, PendingFlush>,
    next_token: u64,
}
```

In `new`, add `pending: HashMap::new(), next_token: 0,` to the literal. In `end_session`, add `self.pending.remove(session);` after the existing `remove`.

Replace `flush` (`:183-281`) with three methods. `diff` is the existing pass with the three record writes turned into pushes onto `updates`; everything else in the pass, including the budget arithmetic and the doc comment about zero caps and deferral, stays as it is.

```rust
    /// Report what changed for `session` since its last committed flush,
    /// and stage the record changes that report implies.
    ///
    /// The record does not move here. It moves in [`Self::commit`], once
    /// the reader confirms it has the report, so a reader that gives up
    /// on its deadline or dies before it prints leaves the record where
    /// it was and the next `stage` offers the same report again. The
    /// token is `None` when the report implies no record change, which
    /// is also when there is nothing for a reader to acknowledge. A
    /// stage replaces whatever the session had staged before: a report
    /// nobody has confirmed is superseded by the newer one, and an
    /// acknowledgement for the old one then commits nothing.
    ///
    /// A zero `max_per_file` or `max_total` means that cap is unlimited,
    /// matching `workspace.max_documents`/`max_file_size`'s convention:
    /// there is no other way to write "no limit", and a literal zero cap
    /// has no sensible reading (`severity = "off"` already covers "deliver
    /// nothing"). Running low on a finite total budget defers a whole file
    /// to the next flush rather than truncating it further: a partially
    /// delivered file reads as the complete picture, which is worse than
    /// waiting. The one exception is a file that does not fit even a
    /// fresh, untouched budget — no later flush would do better either, so
    /// that file is delivered truncated to the budget instead of withheld
    /// forever.
    pub fn stage(
        &mut self,
        session: &SessionId,
        entries: &[FileEntry<'_>],
    ) -> (FlushReport, Option<u64>) {
        let (report, updates) = self.diff(session, entries);
        if updates.is_empty() {
            self.pending.remove(session);
            return (report, None);
        }
        self.next_token += 1;
        let token = self.next_token;
        self.pending
            .insert(session.clone(), PendingFlush { token, updates });
        (report, Some(token))
    }

    /// Apply the record changes staged under `token`.
    ///
    /// `false`, and nothing written, when `token` is not the session's
    /// staged report: a later stage replaced it, an immediate flush
    /// superseded it, or the session ended. The record then already
    /// reflects something a reader was sent more recently, or nothing.
    pub fn commit(&mut self, session: &SessionId, token: u64) -> bool {
        let staged = match self.pending.get(session) {
            Some(pending) if pending.token == token => self.pending.remove(session),
            _ => None,
        };
        let Some(PendingFlush { updates, .. }) = staged else {
            return false;
        };
        let record = self.sessions.entry(session.clone()).or_default();
        for (key, hash) in updates {
            match hash {
                Some(hash) => {
                    record.insert(key, hash);
                }
                None => {
                    record.remove(&key);
                }
            }
        }
        true
    }

    /// [`Self::stage`] and [`Self::commit`] in one call, for a reader whose
    /// answer either arrives or ends the session: the MCP tool and the
    /// footer, whose transport is the session's own.
    pub fn flush(&mut self, session: &SessionId, entries: &[FileEntry<'_>]) -> FlushReport {
        let (report, token) = self.stage(session, entries);
        if let Some(token) = token {
            self.commit(session, token);
        }
        report
    }

    /// One pass over `entries` against `session`'s committed record: the
    /// report, and the record writes it implies, in key order.
    fn diff(
        &mut self,
        session: &SessionId,
        entries: &[FileEntry<'_>],
    ) -> (FlushReport, Vec<(String, Option<u64>)>) {
        let record = &*self
            .sessions
            .entry(session.clone())
            .or_insert_with(|| self.baseline.clone().unwrap_or_default());

        let mut report = FlushReport::default();
        let mut updates = Vec::new();
        let mut budget = (self.config.max_total > 0).then_some(self.config.max_total);
        let mut delivered_any = false;

        for entry in entries {
            let hash = Self::visible_hash(entry.diagnostics, entry.floor);
            let previous = record.get(entry.key).copied();

            match (hash, previous) {
                (None, Some(_)) if entry.floor == SeverityFloor::Off => {
                    // Muted, not fixed. Forgetting the entry without
                    // reporting means the file starts fresh if its floor
                    // ever rises again, and the agent is not told its
                    // problems are gone when they were only silenced.
                    updates.push((entry.key.to_string(), None));
                }
                (None, Some(_)) => {
                    if budget == Some(0) {
                        // Leave the record in place so the next flush
                        // offers this file again, the same deferral a
                        // changed file gets.
                        report.omitted += 1;
                    } else {
                        if let Some(remaining) = budget.as_mut() {
                            *remaining -= 1;
                        }
                        updates.push((entry.key.to_string(), None));
                        report.cleared.push(entry.key.to_string());
                    }
                }
                (None, None) => {}
                (Some(current), Some(before)) if current == before => {}
                (Some(current), _) => {
                    let mut visible: Vec<_> = entry
                        .diagnostics
                        .iter()
                        .filter(|d| entry.floor.admits(d.severity))
                        .cloned()
                        .collect();
                    let per_file_omitted = if self.config.max_per_file == 0 {
                        0
                    } else {
                        let dropped = visible.len().saturating_sub(self.config.max_per_file);
                        visible.truncate(self.config.max_per_file);
                        dropped
                    };

                    let budget_omitted = match budget {
                        None => Some(0),
                        Some(remaining) if visible.len() <= remaining => {
                            budget = Some(remaining - visible.len());
                            Some(0)
                        }
                        Some(remaining) if !delivered_any => {
                            let shortfall = visible.len() - remaining;
                            visible.truncate(remaining);
                            budget = Some(0);
                            Some(shortfall)
                        }
                        Some(_) => None,
                    };

                    let Some(budget_omitted) = budget_omitted else {
                        report.omitted += 1;
                        continue;
                    };

                    delivered_any = true;
                    updates.push((entry.key.to_string(), Some(current)));
                    report.changed.push(ChangedFile {
                        key: entry.key.to_string(),
                        diagnostics: visible,
                        omitted: per_file_omitted + budget_omitted,
                    });
                }
            }
        }

        (report, updates)
    }
```

The module doc at the top of the file says "A flush answers one question: what is different since this session last asked?" Change "last asked" to "was last confirmed to have been told", so the doc matches the record's new meaning.

- [ ] **Step 4: run the tests and watch them pass**

Run: `cargo nextest run -p mcpls-core bridge::delivery`
Expected: PASS, the four new tests and every existing one (the existing ones call `flush`, whose behaviour is unchanged).

- [ ] **Step 5: write the failing test for the clear-then-change budget hole**

`flush` decides whether to deliver a file truncated by asking `!delivered_any`, which is a proxy for "the budget is untouched". A cleared file spends budget at `delivery.rs:225` without setting that flag, so the proxy and the real condition come apart, and a file that would fit a fresh budget is delivered with zero diagnostics and then permanently recorded as delivered.

Add to `crates/mcpls-core/src/bridge/delivery.rs`'s test module:

```rust
#[test]
fn test_a_clear_that_empties_the_budget_defers_the_next_file_whole() {
    let mut delivery = DiagnosticsDelivery::new(DiagnosticsConfig {
        max_total: 1,
        ..DiagnosticsConfig::default()
    });
    let session = SessionId::from("s".to_string());
    let broken = vec![diagnostic(0, DiagnosticSeverity::ERROR, "boom")];

    delivery.flush(&session, &[entry("a.rs", &broken, SeverityFloor::Warning)]);

    let report = delivery.flush(
        &session,
        &[
            entry("a.rs", &[], SeverityFloor::Warning),
            entry("b.rs", &broken, SeverityFloor::Warning),
        ],
    );
    assert_eq!(report.cleared, vec!["a.rs".to_string()]);
    assert!(
        report.changed.is_empty(),
        "the clear spent the whole budget, so b.rs waits for a flush that \
         can carry it rather than being sent empty"
    );

    let next = delivery.flush(&session, &[entry("b.rs", &broken, SeverityFloor::Warning)]);
    assert_eq!(
        next.changed.len(),
        1,
        "a deferred file is offered again, and the record must not claim it \
         was already delivered"
    );
    assert_eq!(next.changed[0].diagnostics.len(), 1);
    assert_eq!(next.changed[0].omitted, 0);
}
```

- [ ] **Step 6: run it and watch it fail**

Run: `cargo nextest run -p mcpls-core test_a_clear_that_empties_the_budget_defers_the_next_file_whole`
Expected: FAIL on the `report.changed.is_empty()` assertion. `b.rs` arrives with an empty `diagnostics` vec and `omitted: 1`, and the third assertion would fail too, because the record already holds `b.rs`'s hash.

- [ ] **Step 7: test the real condition instead of the proxy**

In `flush`, replace the `!delivered_any` arm of the `budget_omitted` match:

```rust
                    let budget_omitted = match budget {
                        None => Some(0),
                        Some(remaining) if visible.len() <= remaining => {
                            budget = Some(remaining - visible.len());
                            Some(0)
                        }
                        // Too big for a whole budget, so no later flush
                        // does better and withholding it withholds it
                        // forever. With nothing left to spend, the next
                        // flush's fresh budget is the better offer.
                        Some(remaining)
                            if remaining > 0 && visible.len() > self.config.max_total =>
                        {
                            let shortfall = visible.len() - remaining;
                            visible.truncate(remaining);
                            budget = Some(0);
                            Some(shortfall)
                        }
                        Some(_) => None,
                    };
```

That leaves `delivered_any` read nowhere. Delete both its declaration (`let mut delivered_any = false;`) and its assignment (`delivered_any = true;`).

- [ ] **Step 8: run the delivery tests and watch them pass**

Run: `cargo nextest run -p mcpls-core bridge::delivery`
Expected: PASS. `test_a_file_larger_than_the_total_budget_is_delivered_truncated_once` still truncates five into two, because five exceeds a whole budget of two and the budget is untouched. `test_cleared_files_spend_the_total_budget` is unaffected: it carries no changed file.


---

## Task 2: the wire, the owner, and the core client

**Files:**
- Modify: `crates/mcpls-core/src/hooks/protocol.rs:60-102` and its tests.
- Modify: `crates/mcpls-core/src/hooks/listener.rs:437-476` (the deadline message), `:492-530` and `:669-688` (the client half).
- Modify: `crates/mcpls-core/src/hooks/mod.rs:18`.
- Modify: `crates/mcpls-core/src/hooks/service.rs:191-261` (the handler), `:482-487` (the harness), `:727-753`, `:835-842`, `:867-913`, `:982-985` (tests).
- Modify: `crates/mcpls-core/src/mcp/server.rs:1049-1123` and `:1268`; one test added near `test_the_hook_render_pins_every_line_it_produces`.
- Modify: `crates/mcpls-core/tests/hooks_socket.rs:128, 168, 227-229, 248-250, 262-301, 274, 325, 369-371, 400-402`.
- Modify: `crates/mcpls-cli/src/hook.rs:196` and `:647-655, :708-734` (compile only; Task 3 does the rest).

**Interfaces:**
- Consumes: `DiagnosticsDelivery::{stage, commit, flush}` from Task 1.
- Produces:

```rust
// protocol.rs
pub enum Request {
    Changed { session: String, paths: Vec<PathBuf>, event: ChangeEvent },
    Flush { session: String },
    Ack { session: String, token: u64 },
    EndSession { session: String },
    Status,
}
pub enum Response {
    Changed { queued: usize },
    Flush { context: Option<String>, token: Option<u64> },
    Ack,
    EndSession,
    Status { hash: String, socket: PathBuf, pid: u32, owner: bool, root: PathBuf, hooks_seen: u64 },
    Error { message: String },
}

// listener.rs
pub async fn send_and_acknowledge(identity: &SocketIdentity, requests: &[Request], timeout: Duration) -> Result<Vec<Response>>;

// server.rs
pub(crate) enum Advance { Now, OnAcknowledgement }
async fn flush_now(&self, session: &SessionId, advance: Advance) -> (NewDiagnosticsResult, Option<u64>);
pub(crate) async fn flush_for_hook(&self, session: &SessionId) -> (Option<String>, Option<u64>);
pub(crate) async fn commit_for_hook(&self, session: &SessionId, token: u64) -> bool;
```

- [ ] **Step 1: change the pinned wire literals and add the two new pins**

In `crates/mcpls-core/src/hooks/protocol.rs` tests:

`test_the_flush_response_pins_the_wire_shape_with_context_present` becomes:

```rust
    #[test]
    fn test_the_flush_response_pins_the_wire_shape_with_context_present() {
        let literal = r#"{"op":"flush","context":"2 errors in a.rs","token":7}"#;
        let value = Response::Flush {
            context: Some("2 errors in a.rs".to_string()),
            token: Some(7),
        };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }
```

`test_the_flush_response_pins_the_wire_shape_with_context_absent` becomes:

```rust
    #[test]
    fn test_the_flush_response_pins_the_wire_shape_with_context_absent() {
        // Neither `Option` has `skip_serializing_if`, so an absent context
        // or token is a present key holding JSON `null`, not an omitted
        // key.
        let literal = r#"{"op":"flush","context":null,"token":null}"#;
        let value = Response::Flush {
            context: None,
            token: None,
        };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }
```

`test_an_omitted_context_key_also_deserializes_to_none`'s expected value becomes `Response::Flush { context: None, token: None }`; the literal `{"op":"flush"}` stays.

Add, after `test_the_status_request_pins_the_wire_shape`:

```rust
    #[test]
    fn test_the_ack_request_pins_the_wire_shape() {
        let literal = r#"{"op":"ack","session":"s1","token":7}"#;
        let value = Request::Ack {
            session: "s1".to_string(),
            token: 7,
        };
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Request>(literal).expect("deserialize"),
            value
        );
    }

    #[test]
    fn test_the_ack_response_pins_the_wire_shape() {
        let literal = r#"{"op":"ack"}"#;
        let value = Response::Ack;
        assert_eq!(
            serde_json::to_value(&value).expect("serialize"),
            serde_json::from_str::<serde_json::Value>(literal).expect("json")
        );
        assert_eq!(
            serde_json::from_str::<Response>(literal).expect("deserialize"),
            value
        );
    }
```

- [ ] **Step 2: run the protocol tests and watch them fail**

Run: `cargo nextest run -p mcpls-core hooks::protocol`
Expected: FAIL to compile, `no field `token``, `no variant named `Ack``.

- [ ] **Step 3: change the wire types**

In `crates/mcpls-core/src/hooks/protocol.rs`, add to `Request` after `Flush`:

```rust
    /// Sent by a hook once a `flush` answer carrying a token is in its
    /// hands, so the owner can mark that report delivered. Sent on the
    /// same connection as the `flush`. Never sent for an answer with no
    /// token, which had nothing to mark.
    Ack {
        /// The Claude Code session the acknowledged flush was for.
        session: String,
        /// The token the `flush` answer carried.
        token: u64,
    },
```

Change `Response::Flush` to:

```rust
    /// Answers a [`Request::Flush`].
    Flush {
        /// The diagnostics context to inject before the next turn, absent
        /// when there is nothing new to report.
        context: Option<String>,
        /// Names the record changes this answer implies, for the
        /// [`Request::Ack`] that commits them. Absent when the answer
        /// implies none, which is also when no acknowledgement is owed.
        token: Option<u64>,
    },
    /// Answers a [`Request::Ack`], whether or not anything was left to
    /// commit: the client cannot act on the difference.
    Ack,
```

The comment above `mod tests` (`:104-108`) says the design spec pins the three `Request` lines under its Protocol heading. Change "three" to "four"; Task 3 adds the fourth line to the spec.

- [ ] **Step 4: make the workspace compile against the new shape**

Every `Response::Flush { context }` pattern and literal gains `token`. The full list, so nothing is missed:

- `crates/mcpls-core/src/hooks/service.rs:241`: the handler; rewritten in Step 7.
- `crates/mcpls-core/src/hooks/service.rs:735, 804, 985, 1370`: patterns; add `token: _` (or `..`) to each.
- `crates/mcpls-core/src/hooks/service.rs:750, 982`: `Response::Flush { context: None }` becomes `Response::Flush { context: None, token: None }`.
- `crates/mcpls-core/src/hooks/service.rs:770, 794`: `matches!(.., Response::Flush { context: Some(_) })` becomes `Response::Flush { context: Some(_), .. }`.
- `crates/mcpls-core/src/hooks/service.rs:908-910`: `slow_flush_handler`'s literal gains `token: None`.
- `crates/mcpls-core/src/mcp/server.rs:1106`: rewritten in Step 9.
- `crates/mcpls-core/src/hooks/listener.rs:869`: the Windows-only test's literal gains `token: None`. Cannot be compiled on Linux; edit it by eye and note it in the handoff.
- `crates/mcpls-core/tests/hooks_socket.rs:128, 168, 274, 325`: `Response::Flush { context: None }` gains `token: None`.
- `crates/mcpls-core/tests/hooks_socket.rs:227-229, 248-250, 369-371, 400-402`: the `Some("hello")` and `Some("drained")` literals gain `token: None`.
- `crates/mcpls-cli/src/hook.rs:196`: `if let Response::Flush { context } = response` becomes `if let Response::Flush { context, .. } = response`.
- `crates/mcpls-cli/src/hook.rs:721-723`: the recording owner's `Flush` arm gains `token: behavior.flush_text.as_ref().map(|_| 1)`, so a flush with text carries a token the way a real owner's does, and one without does not.
- `crates/mcpls-cli/src/hook.rs:708-734`: `answer` gains an arm `Request::Ack { .. } => Response::Ack,`.
- `crates/mcpls-cli/src/hook.rs:647-655`: `op_name` gains `Request::Ack { .. } => "ack",`.

Run: `cargo check --workspace --all-targets --all-features`
Expected: clean apart from the `service.rs:231-244` handler arm, which Step 7 rewrites, and the `server.rs:1106` match, which Step 9 rewrites. If either blocks `check`, do Steps 7 and 9 first and come back.

- [ ] **Step 5: write the failing socket-level tests**

In `crates/mcpls-core/src/hooks/service.rs`, add a harness method next to `send` (`:482-487`):

```rust
        /// Send one flush over the real socket and acknowledge its answer
        /// on the same connection, the way `mcpls hook` does.
        async fn flush_acknowledged(&self, session: &str) -> Response {
            crate::hooks::send_and_acknowledge(
                &self.identity,
                &[Request::Flush {
                    session: session.to_string(),
                }],
                Duration::from_secs(5),
            )
            .await
            .expect("the owner answers")
            .remove(0)
        }
```

Rewrite `test_a_flush_op_and_the_tool_share_one_record` (`:727-753`) so the first flush is acknowledged:

```rust
    #[tokio::test]
    async fn test_a_flush_op_and_the_tool_share_one_record() {
        let harness = HookHarness::owner_with_one_error().await;
        let Response::Flush {
            context: Some(text),
            ..
        } = harness.flush_acknowledged("s1").await
        else {
            panic!("the first flush reports the error");
        };
        assert!(text.contains("broken.rs"));

        let second = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert_eq!(
            second,
            Response::Flush {
                context: None,
                token: None
            },
            "one report per problem, whichever door asked for it"
        );
    }
```

Add after it:

```rust
    #[tokio::test]
    async fn test_an_unacknowledged_flush_is_offered_again() {
        let harness = HookHarness::owner_with_one_error().await;
        let first = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert!(
            matches!(
                first,
                Response::Flush {
                    context: Some(_),
                    token: Some(_)
                }
            ),
            "a report with content carries the token its acknowledgement names: {first:?}"
        );

        let second = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert!(
            matches!(
                second,
                Response::Flush {
                    context: Some(_),
                    token: Some(_)
                }
            ),
            "the hook that read the first answer may have died before printing \
             it, and nothing said otherwise, so the report is offered again \
             rather than lost: {second:?}"
        );
    }

    #[tokio::test]
    async fn test_a_stale_acknowledgement_commits_nothing() {
        let harness = HookHarness::owner_with_one_error().await;
        let Response::Flush {
            token: Some(stale), ..
        } = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await
        else {
            panic!("the first flush carries a token");
        };
        let Response::Flush {
            token: Some(current),
            ..
        } = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await
        else {
            panic!("the second flush carries a token");
        };
        assert_ne!(stale, current);

        let answer = harness
            .send(Request::Ack {
                session: "s1".to_string(),
                token: stale,
            })
            .await;
        assert_eq!(answer, Response::Ack);

        let third = harness
            .send(Request::Flush {
                session: "s1".to_string(),
            })
            .await;
        assert!(
            matches!(
                third,
                Response::Flush {
                    context: Some(_),
                    ..
                }
            ),
            "the acknowledged token named a report a later flush replaced, so \
             the record did not move and the report is still owed: {third:?}"
        );
    }
```

Rewrite the doc comment on `test_a_passive_instance_whose_owner_is_gone_says_so_rather_than_reading_itself` (`:835-842`), whose premise is no longer true:

```rust
    /// A forward that fails must not fall back to this process's own
    /// record.
    ///
    /// The owner's record is the session's record. A diff against this
    /// process's own would be against a baseline the session never
    /// agreed to, and would leave the two disagreeing about what the
    /// session has been shown for the rest of its life. The failed
    /// forward costs nothing on the owner's side: its staged report is
    /// unacknowledged, and the next flush offers it again.
```

Rewrite the doc comment on `test_a_forwarded_flush_outwaits_a_slow_owner` (`:867-872`):

```rust
    /// The forwarded flush waits at least as long as the owner's own op
    /// deadline.
    ///
    /// The owner answers at its deadline rather than before it. A client
    /// bound tighter gives up on a report it asked for and is offered it
    /// again next time, which costs the agent a turn for nothing.
```

In `crates/mcpls-core/tests/hooks_socket.rs`, in `test_an_op_answers_within_its_deadline_while_its_work_runs_on` (`:262-301`), replace the first assertion with one that pins the flush overrun text:

```rust
    let Ok(Response::Error { message }) = response else {
        panic!(
            "a hook that hangs blocks the agent, and the host's own timeout is \
             600 seconds, so the bound has to be ours and it has to answer \
             rather than drop the connection; got {response:?}"
        );
    };
    assert!(
        message.contains("the next flush offers it again"),
        "a flush that outran its deadline was never acknowledged, so what \
         its work stages is offered again; the message has to say that and \
         not promise something else: {message}"
    );
```

Add to `hooks_socket.rs`, after `test_two_requests_share_one_connection`:

```rust
#[tokio::test]
async fn test_send_and_acknowledge_acks_a_tokened_flush_on_the_same_connection() {
    let (_guard, identity) = temp_identity();
    let listener = HookListener::acquire(&identity)
        .await
        .expect("acquire")
        .expect("owner");
    let (_tx, cancel) = tokio::sync::watch::channel(false);
    let seen: Arc<std::sync::Mutex<Vec<Request>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    tokio::spawn(listener.serve(
        handler(move |req| {
            recorder.lock().expect("seen").push(req.clone());
            Box::pin(async move {
                match req {
                    Request::Flush { .. } => Response::Flush {
                        context: Some("2 errors in a.rs".to_string()),
                        token: Some(7),
                    },
                    Request::Ack { .. } => Response::Ack,
                    _ => Response::Error {
                        message: "unexpected".to_string(),
                    },
                }
            })
        }),
        Duration::from_millis(1500),
        cancel,
    ));

    let answers = mcpls_core::hooks::send_and_acknowledge(
        &identity,
        &[Request::Flush {
            session: "s1".to_string(),
        }],
        Duration::from_secs(5),
    )
    .await
    .expect("the owner answers");

    assert_eq!(
        answers,
        vec![Response::Flush {
            context: Some("2 errors in a.rs".to_string()),
            token: Some(7),
        }],
        "the caller gets the flush answers and nothing else; the \
         acknowledgement's own answer is consumed on its behalf"
    );
    let seen = seen.lock().expect("seen").clone();
    assert_eq!(
        seen,
        vec![
            Request::Flush {
                session: "s1".to_string(),
            },
            Request::Ack {
                session: "s1".to_string(),
                token: 7,
            },
        ],
        "the acknowledgement names the session and the token the answer \
         carried, and follows the flush on the connection it came in on"
    );
}
```

Add `use std::sync::Arc;` to the imports at the top of `hooks_socket.rs`; the file does not import it yet. `handler` is the helper the file already defines at `:59-63`, whose signature is exactly what this closure satisfies.

Also in `crates/mcpls-core/src/hooks/service.rs`, extend `test_a_passive_instance_forwards_its_flush_to_the_owner` (`:821-833`) with a second forward, which is what proves the passive acknowledged the first:

```rust
        let again = passive.call_flush_tool().await;
        assert!(
            !again.contains("broken.rs"),
            "the passive acknowledged the first answer once it had it in hand, \
             so the owner does not offer the report again: {again}"
        );
```

In `crates/mcpls-core/src/mcp/server.rs` tests, add after `test_the_hook_render_pins_every_line_it_produces`:

```rust
    /// The tool door advances the record in the same lock block that
    /// computes its report, so a hook report staged for the same session
    /// is superseded rather than committed on top of it.
    #[tokio::test]
    async fn test_an_immediate_flush_supersedes_a_staged_hook_report() {
        let parts = test_server_with_footer_and_one_error().await;
        let session = SessionId::from("s1".to_string());

        let (staged, token) = parts
            .server
            .flush_now(&session, Advance::OnAcknowledgement)
            .await;
        assert_eq!(staged.changed.len(), 1);
        let token = token.expect("a staged report with content carries a token");

        let (now, none) = parts.server.flush_now(&session, Advance::Now).await;
        assert_eq!(now.changed.len(), 1, "the hook's report was never confirmed");
        assert_eq!(none, None, "an immediate flush leaves nothing to acknowledge");

        assert!(
            !parts.server.commit_for_hook(&session, token).await,
            "the record already holds what the tool result showed"
        );

        let (after, _) = parts
            .server
            .flush_now(&session, Advance::OnAcknowledgement)
            .await;
        assert!(after.changed.is_empty());
    }
```

- [ ] **Step 6: run the new tests and watch them fail**

Run: `cargo nextest run -p mcpls-core hooks::service && cargo nextest run -p mcpls-core --test hooks_socket && cargo nextest run -p mcpls-core mcp::server::tests::test_an_immediate_flush`
Expected: FAIL to compile: `send_and_acknowledge` and `Advance` do not exist.

- [ ] **Step 7: the socket handler**

In `crates/mcpls-core/src/hooks/service.rs`, replace the `Flush` arm (`:231-244`) and add the `Ack` arm before `EndSession`:

```rust
                Request::Flush { session } => {
                    role.record_hook_request();
                    let session = SessionId::from(session);
                    let (report, token) = server.flush_for_hook(&session).await;
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(text) = report {
                        parts.push(text);
                    }
                    if let Some(shortfall) = sweeper.last_shortfall() {
                        parts.push(shortfall);
                    }
                    Response::Flush {
                        context: (!parts.is_empty()).then(|| parts.join("\n")),
                        token,
                    }
                }
                Request::Ack { session, token } => {
                    server
                        .commit_for_hook(&SessionId::from(session), token)
                        .await;
                    Response::Ack
                }
```

`Ack` does not call `record_hook_request`: it is the second half of a `Flush` already counted, and counting it would double every flush in `mcpls hook doctor`'s `hooks seen`. Add that clause to the doc comment on `record_hook_request` (`:105-108`): after "Not called for `Status`, which is …", add "nor for `Ack`, which is the second half of a `Flush` already counted."

Update the `build_handler` doc comment (`:194-198`) so its lock statement covers the new arm:

```rust
/// Lock order, the same one the rest of the crate follows: delivery before
/// cache. Every path through here reaches the cache only by way of
/// `McplsServer::flush_for_hook`, which takes delivery then cache and drops
/// both before it awaits the payload build, and reaches delivery alone
/// only by way of `McplsServer::commit_for_hook` and `end_session`. No arm
/// of this match takes either lock itself. Do not add one that does.
```

- [ ] **Step 8: the deadline overrun message**

In `crates/mcpls-core/src/hooks/listener.rs`, `serve_connection` (`:439-476`): the request is moved into `handler(request)`, so capture the message first. Replace the `Ok(request) => { ... }` arm with:

```rust
            Ok(request) => {
                let overrun = overrun_message(&request, op_deadline);
                let work = tokio::spawn(handler(request));
                match tokio::time::timeout(op_deadline, work).await {
                    Ok(Ok(response)) => response,
                    Ok(Err(_join_error)) => Response::Error {
                        message: "the handler panicked".to_string(),
                    },
                    Err(_elapsed) => Response::Error { message: overrun },
                }
            }
```

Add after `serve_connection`:

```rust
/// What a client is told when its op outran the deadline: what the work
/// it did not wait for does once it finishes, which differs per op.
///
/// A `flush` that outruns the deadline stages a report nobody
/// acknowledges, so the record does not move and the next `flush` offers
/// the same report; saying so is what lets a client treat the error as a
/// deferral rather than a loss.
fn overrun_message(request: &Request, op_deadline: Duration) -> String {
    let ms = op_deadline.as_millis();
    match request {
        Request::Changed { .. } => format!(
            "op exceeded {ms}ms; the paths are queued when the work finishes and their \
             diagnostics reach a later flush"
        ),
        Request::Flush { .. } => format!(
            "op exceeded {ms}ms; nothing confirmed this report was delivered, so the next \
             flush offers it again"
        ),
        Request::Ack { .. } => {
            format!("op exceeded {ms}ms; the record advances when the work finishes")
        }
        Request::EndSession { .. } => format!(
            "op exceeded {ms}ms; the session's record is dropped when the work finishes"
        ),
        Request::Status => format!("op exceeded {ms}ms"),
    }
}
```

- [ ] **Step 9: the owner's server side**

In `crates/mcpls-core/src/mcp/server.rs`, add above `impl McplsServer`'s `flush_now` (anywhere before it in the file; next to `FooterTiming` at `:292` is a reasonable home):

```rust
/// When a flush's record changes take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Advance {
    /// In the flush itself. For a reader on this process's own transport,
    /// whose answer either arrives or ends the session: the tool and the
    /// footer.
    Now,
    /// When the reader acknowledges the report, or never. For the hook
    /// socket, whose client gives up on a timer and may be gone before
    /// the answer is written; a report it never acknowledges is offered
    /// again.
    OnAcknowledgement,
}
```

Replace `flush_now` (`:1049-1070`) with:

```rust
    /// Flush `session`'s record and render it, advancing the record as
    /// `advance` says.
    ///
    /// The caller checks `has_baseline()` first. Both the tool and the
    /// footer must, because `stage` seeds a session's record from the
    /// baseline and `set_baseline` never rewrites one that already exists.
    ///
    /// The token is `Some` only under `Advance::OnAcknowledgement`, and
    /// only when the report implies a record change.
    // What enforces the delivery-before-cache order is where the two
    // acquires sit, not how long either guard lives afterward. Clippy's fix
    // for this lint moves the `delivery` acquire below the cache acquire,
    // which is the exact reversal that order forbids, so the acquires stay
    // where they are and the lint is silenced instead.
    #[allow(clippy::significant_drop_tightening)]
    async fn flush_now(
        &self,
        session: &SessionId,
        advance: Advance,
    ) -> (NewDiagnosticsResult, Option<u64>) {
        let (report, token, sources) = {
            let mut delivery = self.context.delivery.lock().await;
            let cache = self.context.notification_cache.lock().await;
            let entries = routable_entries_borrowed(&cache, &self.context.floors);
            let (report, token) = match advance {
                Advance::Now => (delivery.flush(session, &entries), None),
                Advance::OnAcknowledgement => delivery.stage(session, &entries),
            };
            let sources = source_map(&cache, &report);
            (report, token, sources)
        };
        (self.new_diagnostics_payload(&report, &sources).await, token)
    }
```

Change the tool's call at `:1046` to `to_tool_result(Ok(self.flush_now(&session, Advance::Now).await.0))` and the footer's at `:1268` to `let (mut report, _) = self.flush_now(&session, Advance::Now).await;`.

Replace `flush_for_hook` (`:1072-1086`) and add `commit_for_hook`:

```rust
    /// `session`'s flush, rendered as the text a hook prints, with the
    /// token the hook acknowledges once it has that text.
    ///
    /// The same report the tool would run, against the same record, so a
    /// hook and an agent never see the same diagnostic twice once either
    /// has confirmed it. The record moves only in `commit_for_hook`: the
    /// hook gives up on a timer, and a report it gave up on is offered
    /// again rather than marked delivered. Silent before a baseline exists
    /// for the same reason the footer is: `stage` seeds a session's record
    /// from the baseline and `set_baseline` never rewrites one that
    /// already exists, so flushing early would leave that session
    /// permanently believing the workspace started clean.
    pub(crate) async fn flush_for_hook(&self, session: &SessionId) -> (Option<String>, Option<u64>) {
        if !self.context.delivery.lock().await.has_baseline() {
            return (None, None);
        }
        let (report, token) = self.flush_now(session, Advance::OnAcknowledgement).await;
        (render_for_hook(&report), token)
    }

    /// Mark the report staged under `token` delivered to `session`.
    ///
    /// Takes `delivery` alone. `false` when `token` no longer names the
    /// session's staged report, in which case the record already reflects
    /// something sent more recently, or nothing.
    pub(crate) async fn commit_for_hook(&self, session: &SessionId, token: u64) -> bool {
        self.context.delivery.lock().await.commit(session, token)
    }
```

Replace `flush_from_owner` (`:1088-1123`), whose doc comment states a premise this change removes:

```rust
    /// The owner's flush for this session, in the shape every other
    /// instance answers with, acknowledged once the answer is in hand.
    ///
    /// Never falls back to this process's own record. The owner's record
    /// is the session's record; a diff against this process's own would
    /// be against a baseline the session never agreed to, and would leave
    /// the two permanently disagreeing about what this session has been
    /// shown. A forward that fails costs nothing on the owner's side: its
    /// staged report is unacknowledged and the next flush offers it
    /// again. Saying the owner could not be reached is the honest answer,
    /// and it is the MCP tool rather than a hook, so the rule that an
    /// edit must never fail on the socket does not apply.
    async fn flush_from_owner(&self, identity: &SocketIdentity) -> NewDiagnosticsResult {
        let request = hooks::Request::Flush {
            session: SessionId::from_env_or_process().to_string(),
        };
        let timeout =
            Duration::from_millis(self.context.diagnostics.hooks.op_deadline_ms) + HOOK_FLUSH_GRACE;
        let answer = hooks::send_and_acknowledge(identity, std::slice::from_ref(&request), timeout)
            .await
            .map(|mut responses| responses.remove(0));
        match answer {
            Ok(hooks::Response::Flush { context, .. }) => NewDiagnosticsResult::from_owner(context),
            Ok(hooks::Response::Error { message }) => {
                tracing::warn!(%message, "the hook socket's owner refused this session's flush");
                NewDiagnosticsResult::owner_unreachable()
            }
            Ok(other) => {
                tracing::warn!(
                    ?other,
                    "the hook socket's owner answered a flush with something else"
                );
                NewDiagnosticsResult::owner_unreachable()
            }
            Err(error) => {
                tracing::warn!(%error, "could not reach the hook socket's owner for this session's flush");
                NewDiagnosticsResult::owner_unreachable()
            }
        }
    }
```

- [ ] **Step 10: the core client: `Connection` and `send_and_acknowledge`**

In `crates/mcpls-core/src/hooks/listener.rs`, the client framing lives in one free function, `exchange(stream, requests)`, shared by `send_many_inner` and `answer_one`, with a doc comment explaining that `mcpls hook doctor` depends on both reading the same framing. Keep that property and give the loop a home that outlives one call, so a caller can run a second exchange on the same stream. Replace the free `exchange` and `send_many_inner` with:

```rust
/// One open connection to whoever owns a socket, so a caller can run more
/// than one exchange on it: a flush and, once its answer is in hand, the
/// acknowledgement that lets the owner advance the record. One connection
/// rather than two because a connect is the one step that can find a
/// Windows pipe busy; an exchange on an open connection cannot.
struct Connection {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<Box<dyn HookStream>>>>,
    writer: tokio::io::WriteHalf<Box<dyn HookStream>>,
}

impl Connection {
    async fn open(identity: &SocketIdentity) -> Result<Self> {
        Ok(Self::over(connect(identity).await?))
    }

    fn over(stream: Box<dyn HookStream>) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
        }
    }

    /// Write every request in `requests`, in order, then read back one
    /// response line for each.
    ///
    /// The one place that writes the request framing and reads the
    /// response framing, for [`send_many_inner`], [`answer_one`] and
    /// [`send_and_acknowledge`] alike: if any two disagreed on either,
    /// `mcpls hook doctor` would misread a healthy owner's answer and
    /// report [`ProbeOutcome::Busy`] for it, which is exactly the
    /// misdiagnosis this module exists to prevent.
    async fn exchange(&mut self, requests: &[Request]) -> Result<Vec<Response>> {
        for request in requests {
            let mut line = serde_json::to_string(request)?;
            line.push('\n');
            write_line(&mut self.writer, &line).await?;
        }
        let mut responses = Vec::with_capacity(requests.len());
        for _ in requests {
            let line = self.lines.next_line().await?.ok_or_else(|| {
                Error::Transport("the hook socket closed before answering".to_string())
            })?;
            responses.push(serde_json::from_str(&line)?);
        }
        Ok(responses)
    }
}

/// Write `request` on `stream` and read back one response line.
async fn answer_one(stream: Box<dyn HookStream>, request: &Request) -> Result<Response> {
    let mut responses = Connection::over(stream)
        .exchange(std::slice::from_ref(request))
        .await?;
    Ok(responses.remove(0))
}

async fn send_many_inner(identity: &SocketIdentity, requests: &[Request]) -> Result<Vec<Response>> {
    Connection::open(identity).await?.exchange(requests).await
}
```

The read loop inside `exchange` is the free function's own body moved onto persistent halves; nothing about the framing changes, which is what keeps the doctor's probe reading the same bytes the hook does. Then add `send_and_acknowledge` after `send_many`:

```rust
/// Send `requests` down one connection and, when the last flush among
/// them was answered with a token, acknowledge it on that connection
/// before returning.
///
/// The answers are returned whether or not the acknowledgement lands. By
/// the time it is sent the caller's report is in hand, and an owner that
/// never hears it offers the same report again next time; withholding the
/// report over a failed acknowledgement would be the one outcome the
/// acknowledgement exists to rule out. The acknowledgement shares the
/// caller's deadline but not its error path.
///
/// It is sent before the caller prints, not after. The host reads a
/// hook's output at the hook's exit and discards it if the hook is
/// killed first, so the interval in which a kill loses the report runs
/// from the ack leaving this process to the process exiting under either
/// order; sending after the print would trim that interval by the print
/// alone and leave the round trip and the exit, which dominate it, where
/// they are.
///
/// # Errors
///
/// Returns an error if no one owns the socket, the connection drops before
/// every answer arrives, or `timeout` elapses before they do.
pub async fn send_and_acknowledge(
    identity: &SocketIdentity,
    requests: &[Request],
    timeout: Duration,
) -> Result<Vec<Response>> {
    let deadline = tokio::time::Instant::now() + timeout;
    let exchanged = tokio::time::timeout_at(deadline, async {
        let mut connection = Connection::open(identity).await?;
        let responses = connection.exchange(requests).await?;
        Ok::<_, Error>((connection, responses))
    })
    .await
    .map_err(|_elapsed| {
        Error::Transport(format!(
            "the hook socket did not answer within {}ms",
            timeout.as_millis()
        ))
    })?;
    let (mut connection, responses) = exchanged?;

    if let Some(ack) = acknowledgement_for(requests, &responses) {
        match tokio::time::timeout_at(deadline, connection.exchange(&[ack])).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::debug!(%error, "the flush acknowledgement was not answered; the owner offers the report again");
            }
            Err(_elapsed) => {
                tracing::debug!("the flush acknowledgement outran the deadline; the owner offers the report again");
            }
        }
    }
    Ok(responses)
}

/// The acknowledgement `responses` call for: one for the last `Flush` in
/// `requests` whose answer carries a token, or none.
fn acknowledgement_for(requests: &[Request], responses: &[Response]) -> Option<Request> {
    requests
        .iter()
        .zip(responses)
        .rev()
        .find_map(|(request, response)| match (request, response) {
            (
                Request::Flush { session },
                Response::Flush {
                    token: Some(token), ..
                },
            ) => Some(Request::Ack {
                session: session.clone(),
                token: *token,
            }),
            _ => None,
        })
}
```

`probe` keeps calling `answer_one` and is otherwise untouched; it sends `Status`, which never carries a token, so it has no acknowledgement to send.

In `crates/mcpls-core/src/hooks/mod.rs:18`, add `send_and_acknowledge` to the `listener` re-export list.

- [ ] **Step 11: run the core tests and watch them pass**

Run: `cargo nextest run -p mcpls-core hooks:: && cargo nextest run -p mcpls-core --test hooks_socket && cargo nextest run -p mcpls-core mcp::server && cargo nextest run -p mcpls-core bridge::delivery`
Expected: PASS. The extended `test_a_passive_instance_forwards_its_flush_to_the_owner` is the one that proves `flush_from_owner` acknowledges: its second forward reports nothing only because the first was committed.

---

## Task 3: the hook client, the spec, and the commit

**Files:**
- Modify: `crates/mcpls-cli/src/hook.rs:14-17` (imports), `:131-168` (the two flush arms), `:657-700` (`OwnerBehavior`), `:756-796` (constructors), `:927-971` (the test `serve_connection`), `:1322-1343` and `:1377-1437` (tests), plus two new tests.
- Modify: `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`, the Protocol section under "### Protocol" (the code block and the paragraph after it).

**Interfaces:**
- Consumes: `send_and_acknowledge`, `Request::Ack`, `Response::Ack`, `Response::Flush { token }` from Task 2.
- Produces: nothing later tasks use; this is the last task.

- [ ] **Step 1: write the failing client tests**

In `crates/mcpls-cli/src/hook.rs` tests, add to `OwnerBehavior` (`:664-686`) a field, with its `Default` set to `false`:

```rust
        /// Whether the connection is closed right after a `Flush` is
        /// answered, before any acknowledgement can be read. `false` by
        /// default; one test sets it to prove a report already in hand is
        /// printed regardless of what the acknowledgement meets.
        hang_up_after_flush: bool,
```

Add a constructor after `start_with_changed_error` (`:790-796`):

```rust
        /// The same, closing the connection the moment a `Flush` has been
        /// answered.
        fn start_hanging_up_after_flush(flush_text: Option<String>) -> Self {
            Self::start_with(OwnerBehavior {
                flush_text,
                hang_up_after_flush: true,
                ..OwnerBehavior::default()
            })
        }
```

In the test `serve_connection` (the generic loop under `RecordingOwner`, `:927-971` at `b3e13d0`), `request` is moved into `requests` by the push, so decide before the push and act after the flush. Directly after the `flush_delay` sleep block, before the `let mut out = ...` that builds the answer line, add:

```rust
            let hang_up = behavior.hang_up_after_flush && matches!(request, Request::Flush { .. });
```

and after the `writer.flush().await` check that ends the loop body, add:

```rust
            if hang_up {
                return;
            }
```

Rewrite `test_user_prompt_submit_flushes_and_injects_context` (`:1322-1343`):

```rust
    #[tokio::test]
    async fn test_user_prompt_submit_flushes_and_acknowledges() {
        let recorder = RecordingOwner::start();
        let out = dispatch_against(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            &recorder,
        )
        .await;

        let requests = recorder.requests();
        assert_eq!(
            recorder.ops(),
            vec!["flush".to_string(), "ack".to_string()],
            "a flush with content is acknowledged once its answer is in hand: {requests:?}"
        );
        let Request::Flush { session } = &requests[0] else {
            panic!("expected a flush request: {:?}", requests[0]);
        };
        assert_eq!(session.as_str(), "s1");
        let Request::Ack { session, token } = &requests[1] else {
            panic!("expected an ack request: {:?}", requests[1]);
        };
        assert_eq!(session.as_str(), "s1");
        assert_eq!(
            *token, 1,
            "the acknowledgement names the token the answer carried, so the \
             owner commits that report and not a later one"
        );
        assert_eq!(
            recorder.connections(),
            1,
            "the acknowledgement rides the flush's own connection"
        );

        let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            json!(DEFAULT_FLUSH_TEXT)
        );
    }
```

In `test_post_tool_batch_sends_changed_then_flush` (`:1377-1437`), change the length assertion and add the third request:

```rust
        let requests = recorder.requests();
        assert_eq!(
            recorder.ops(),
            vec!["changed".to_string(), "flush".to_string(), "ack".to_string()],
            "a changed, the flush, then the acknowledgement: {requests:?}"
        );
```

leaving the `Changed` and `Flush` destructurings as they are, and change the final connection assertion's message to:

```rust
        assert_eq!(
            recorder.connections(),
            1,
            "changed, flush and the acknowledgement travel on one connection: a \
             second connect would be a second chance to find the pipe busy on \
             Windows, and the owner commits nothing until the acknowledgement \
             arrives"
        );
```

Add two tests after `test_a_changed_error_does_not_swallow_the_batch_s_flush`:

```rust
    #[tokio::test]
    async fn test_a_flush_with_nothing_to_report_is_not_acknowledged() {
        let recorder = RecordingOwner::start_with_flush(None);
        let out = dispatch_against(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            &recorder,
        )
        .await;

        assert_eq!(out, "");
        assert_eq!(
            recorder.ops(),
            vec!["flush".to_string()],
            "an answer with no token implies no record change, so there is \
             nothing to acknowledge"
        );
    }

    /// The report is in hand before the acknowledgement is sent, and the
    /// acknowledgement's fate does not gate the print. An owner that hangs
    /// up on it offers the report again next time; losing the report here
    /// would be the one outcome the acknowledgement exists to rule out.
    #[tokio::test]
    async fn test_a_refused_acknowledgement_still_injects_the_context() {
        let recorder =
            RecordingOwner::start_hanging_up_after_flush(Some(DEFAULT_FLUSH_TEXT.to_string()));
        let out = dispatch_against(
            &json!({ "hook_event_name": "UserPromptSubmit", "session_id": "s1" }),
            &recorder,
        )
        .await;

        assert_eq!(
            out,
            additional_context_output(Some(DEFAULT_FLUSH_TEXT.to_string())),
            "the flush answer was read before the owner hung up, so it is \
             printed: {out}"
        );
    }
```

- [ ] **Step 2: run the client tests and watch them fail**

Run: `cargo nextest run -p mcpls-cli hook::`
Expected: FAIL. `test_user_prompt_submit_flushes_and_acknowledges` sees `["flush"]`, `test_post_tool_batch_sends_changed_then_flush` sees two ops, `test_a_refused_acknowledgement_still_injects_the_context` passes already (nothing acknowledges yet), and the hang-up field fails to compile until `OwnerBehavior` has it. Confirm the first two fail on the op list, not on a compile error elsewhere.

- [ ] **Step 3: route the two flush arms through `send_and_acknowledge`**

In `crates/mcpls-cli/src/hook.rs`, change the import at `:14-17` to:

```rust
use mcpls_core::hooks::{
    ChangeEvent, ProbeOutcome, Request, Response, SocketIdentity, probe, send,
    send_and_acknowledge, watch_paths,
};
```

`send_many` is no longer used in this file; drop it from the import or clippy fails on it.

Replace the `PostToolBatch` and `UserPromptSubmit` arms (`:131-168`):

```rust
        "PostToolBatch" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            let paths = payload
                .tool_calls
                .into_iter()
                .filter_map(|call| call.tool_input.file_path)
                .collect();
            let requests = [
                Request::Changed {
                    session: payload.session_id.clone(),
                    paths,
                    event: ChangeEvent::Change,
                },
                Request::Flush {
                    session: payload.session_id,
                },
            ];
            let responses = send_and_acknowledge(identity, &requests, FLUSH_SOCKET_TIMEOUT).await?;
            let context = responses.into_iter().nth(1).and_then(flush_context);
            Ok(additional_context_output(context))
        }

        "UserPromptSubmit" => {
            let Some(identity) = identity else {
                return Ok(String::new());
            };
            let responses = send_and_acknowledge(
                identity,
                &[Request::Flush {
                    session: payload.session_id,
                }],
                FLUSH_SOCKET_TIMEOUT,
            )
            .await?;
            let context = responses.into_iter().next().and_then(flush_context);
            Ok(additional_context_output(context))
        }
```

Update the doc comment on `FLUSH_SOCKET_TIMEOUT` (`:24-31`) so its last sentence reads: "A missing owner still fails fast, because the connect itself fails immediately rather than waiting out this bound. The acknowledgement that follows a flush answer shares this bound and never turns a report already in hand into an error."

- [ ] **Step 4: run the client tests and watch them pass**

Run: `cargo nextest run -p mcpls-cli hook::`
Expected: PASS, including the doctor tests, which send `Status` and are untouched by this.

- [ ] **Step 5: amend the spec's Protocol section**

In `docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`, under `### Protocol`, the code block lists three lines. Add a fourth after the `flush` line:

```
{"op":"ack","session":"<id>","token":<n>}
```

In the paragraph that follows, replace the sentence `` `flush` drains the delivery record and does not resync. `` with:

> `flush` stages the session's report and answers it with a token; the record advances only when an `ack` carrying that token arrives, which the hook sends on the same connection once the answer is in hand. A report nobody acknowledges is offered again by the next `flush`, so a hook that gives up on its deadline, or is killed before it prints, costs a repeat rather than a loss. An answer with nothing to commit carries no token and gets no `ack`. The MCP tool and the footer advance the record in the flush itself: their transport is the session's own, and losing it ends the session. `flush` does not resync.

Then, still in the same section, the sentence beginning "Work already started keeps running after the deadline answers; what it produces reaches the next flush." is true for `changed` and, with the staged record, for `flush`; leave it.

- [ ] **Step 6: run the whole workspace, then formatting and lints**

Run: `cargo nextest run --workspace --all-features`
Expected: PASS. The rust-analyzer and pyrefly end-to-end suites need those servers on `PATH`; if they are absent they are skipped or fail on the missing binary, which is unrelated and to be reported rather than fixed here.

Run: `cargo fmt --check && cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: both clean. This is where an unused `send_many` import, a missing doc comment on `Request::Ack`'s fields, or `significant_drop_tightening` on the new `flush_now` block shows up; the `#[allow]` on `flush_now` is carried over from the existing function for that last one.

- [ ] **Step 7: review the staged diff**

Run: `git -C /home/lev/Git/lev/mcpls-diag-bc add crates/mcpls-core/src/bridge/delivery.rs crates/mcpls-core/src/hooks/protocol.rs crates/mcpls-core/src/hooks/listener.rs crates/mcpls-core/src/hooks/mod.rs crates/mcpls-core/src/hooks/service.rs crates/mcpls-core/src/mcp/server.rs crates/mcpls-core/tests/hooks_socket.rs crates/mcpls-cli/src/hook.rs docs/superpowers/specs/2026-09-06-diagnostics-injection-design.md`

Then `git -C /home/lev/Git/lev/mcpls-diag-bc diff --staged --stat` and read `git -C /home/lev/Git/lev/mcpls-diag-bc diff --staged` end to end. Check three things: no comment refers to this plan, a task, "previously", "now we", or the TDD cycle; every `Response::Flush` site has `token`; the only files staged are the nine named above.

- [ ] **Step 8: commit**

```bash
git -C /home/lev/Git/lev/mcpls-diag-bc commit -F - <<'EOF'
fix(hooks): commit a flush only once acknowledged

A flush advanced the session's delivery record before its answer
reached the hook, so a client that gave up on its deadline, or was
killed before it printed, had its report marked delivered and never
offered again. The hook exits 0 and prints nothing on every fault,
so the loss looked exactly like a clean workspace, on a feature whose
whole promise is that silence means clean.

The flush stages its record changes under a token the answer
carries, and the hook acknowledges on the same connection once the
answer is in hand. Only that acknowledgement commits. A report nobody
acknowledges is offered again by the next flush, and a stale
acknowledgement commits nothing, so every overlap between doors and
between concurrent hooks resolves to a repeat rather than a loss: the
record only ever takes a hash some reader confirmed having in hand.
The tool and footer doors keep advancing immediately; their
transport is the session's own, and losing it ends the session.

The deadline overrun message named one consequence for every op and
was wrong for flush. It says what each op's unfinished work does.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01PZTxadnYDr7Eh3XuzZzc9y
EOF
```

---

## Self-review

**Spec coverage.** The spec's "one report per problem" rule holds on both doors: the acknowledged hook path and the immediate tool path each record what they showed, and the interleavings section shows the overlap cases produce repeats, not losses, which is the direction the spec's "A footer consumes" paragraph accepts as the lesser cost. The lock order and the `op_deadline_ms` deadline are untouched. The Protocol section is amended in Task 3 Step 5, which is the one place the plan changes the spec rather than implementing it.

**Placeholder scan.** Every code step carries the code. The one instruction that names a location rather than quoting surrounding lines (Task 3 Step 1, the hang-up check in the test `serve_connection`) names the two statements it sits between, because the concurrent doctor work reshaped that loop after the line numbers were taken.

**Type consistency.** `stage` returns `(FlushReport, Option<u64>)` in Task 1 and is consumed as such in Task 2's `flush_now`. `flush_for_hook` returns `(Option<String>, Option<u64>)` and the handler destructures it that way. `send_and_acknowledge` returns `Result<Vec<Response>>`, which both `flush_from_owner` (`.remove(0)`) and the hook (`.into_iter().nth(1)` / `.next()`) consume. `Request::Ack { session: String, token: u64 }` is spelled the same in the protocol, `acknowledgement_for`, the handler arm, `op_name`, `answer`, and every test.

## Unresolved questions

1. **Resolved: Claude Code does kill an in-flight hook on a turn interrupt.** Verified by reading the installed Claude Code 2.1.263 binary, not assumed. On an interrupt the host sends `SIGTERM` to the hook's process group, then `SIGKILL` about five seconds later, and discards output written after the interrupt; the session goes on without the answer. The strings read were "sending SIGTERM to the hook's process group", "still running after SIGTERM, sending SIGKILL to the hook's process group", and a message about stopping waiting for a hook and going on without its answer. The same discard applies when the host's own hook timeout expires (default 120 s, overridable per hook). `PostToolBatch` and `UserPromptSubmit` each have a cancellation path. So interrupting a turn while a `PostToolBatch` hook is in flight kills it, and today that loses the diagnostics of the batch that was just interrupted, permanently and silently. That is the everyday trigger, and it is what "Where it can still lose" above is measured against.

2. **Resolved: `Response::Ack` does not report what it committed.** The client cannot act on the difference, so the field would exist for nothing it reads; left out under YAGNI. If a log line or `mcpls hook doctor` ever needs to tell a stale acknowledgement from a live one, it is one added field on a response the client already reads.

3. **Resolved: a stage with no updates does replace an older pending.** Case 8's reasoning stands: committing a report the client never saw is the exact failure this work removes, and leaving the older pending committable would let a late acknowledgement do that. `stage` removes it, as written in Task 1.

4. **Resolved: the muted-file no-op acknowledgement is accepted, not special-cased.** A branch in `diff` costs more than an occasional pointless round trip on a 1500 ms budget. The implementation stays as written in Task 1; the `token: None` assertion in the delivery tests does not cover the muted case, and no test is added for it.
