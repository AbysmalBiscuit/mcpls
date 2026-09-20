# Piloted harness tests

A **piloted harness test** drives a real host agent from another agent. The **driver** builds a rig, starts a **subject** (Codex or Claude Code) in a terminal pane, sends it prompts, and reads the evidence the rig captured. It answers the questions a unit or end-to-end suite cannot: what a host actually puts in a hook payload, what identity its MCP calls carry, whether two doors of mcpls meet on one record.

Everything below is the Codex instance of the method. The shape holds for Claude Code; the rig files change.

## When to reach for this

The Verification section of a design document lists gates that no test in `cargo nextest` covers, because the behaviour under test belongs to the host. Those are the candidates:

- The frontend and the hook agreeing on one endpoint.
- The shape and identity fields of a real hook payload.
- `_meta` on real MCP tool calls, and whether a subagent's differs from its root's.
- Per-agent diagnostic attribution across the hook door and the MCP door.
- How the host's approval policy treats a tool's annotations.

A behaviour reproducible from a synthetic payload belongs in `crates/mcpls-core/src/bridge/delivery_tests.rs` or `crates/mcpls-core/tests/ra_e2e.rs` instead. Reach for a pilot when the host itself is the thing you distrust.

## The rig

Four pieces, all under the session scratchpad so the daily-driver configuration stays untouched. Write them with literal absolute paths: devkit's write harness refuses a path it cannot resolve from the command text.

**A throwaway checkout.** A crate small enough that rust-analyzer indexes it in seconds, with one file per agent you plan to attribute. `git init` is enough; mcpls resolves the checkout root from `.git`, and no commit is needed.

```bash
git init -q "$SP/testproj"
```

**An isolated host home.** Codex reads `CODEX_HOME` and falls back to `~/.codex`, so pointing it at a scratch directory gives a config the test owns. Symlink `auth.json` from the real home to stay authenticated.

```toml
model = "gpt-6-astra"
model_reasoning_effort = "low"
approval_policy = "never"
sandbox_mode = "workspace-write"

[features]
hooks = true
multi_agent_v2 = true

[agents]
enabled = true

[mcp_servers.mcpls]
command = "bash"
args = ["/abs/path/mcp-wrap.sh"]
enabled = true

[hooks]

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "bash /abs/path/hook-wrap.sh"
timeout = 30

[[hooks.PostToolUse]]
matcher = "^apply_patch$"
[[hooks.PostToolUse.hooks]]
type = "command"
command = "bash /abs/path/hook-wrap.sh"
timeout = 30
```

**A hook wrapper.** Captures the payload, the environment the hook ran in, and mcpls's own output, then passes the real payload through on stdin so the test stays faithful.

```bash
#!/usr/bin/env bash
LOGDIR=/abs/path/hooklog
payload=$(cat)
ts=$(date +%s%N)
printf '%s' "$payload" > "$LOGDIR/$ts.payload.json"
echo "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-<unset>} PWD=$PWD" > "$LOGDIR/$ts.env.txt"
printf '%s' "$payload" | mcpls hook --host codex > "$LOGDIR/$ts.stdout.json" 2> "$LOGDIR/$ts.stderr.txt"
echo "exit=$?" >> "$LOGDIR/$ts.env.txt"
cat "$LOGDIR/$ts.stdout.json"
```

**An MCP wrapper.** Tees the JSON-RPC both ways around the real binary. This is what makes `_meta` visible.

```bash
#!/usr/bin/env bash
LOG=/abs/path/mcplog
mkdir -p "$LOG"
exec tee -a "$LOG/in.jsonl" | mcpls | tee -a "$LOG/out.jsonl"
```

## Procedure

1. **Build the rig and record the versions.** Write the four pieces, then capture `mcpls --version`, `codex --version`, and `mcpls hook doctor` in the throwaway checkout. Host behaviour is version specific, so a result without its versions cannot be read later. Done when the doctor reports no backend for that root, which is the clean baseline every run starts from.

2. **Open a pane and start the subject.** Split a sibling pane in the driver's own tab, keep focus where it is, export `CODEX_HOME` and `MCPLS_NO_BOOTSTRAP=1` into the pane's shell, then start the agent.

   ```bash
   herdr pane split --current --direction right --cwd "$SP/testproj" --no-focus
   herdr pane run <pane-id> "export CODEX_HOME=... MCPLS_NO_BOOTSTRAP=1"
   herdr agent start <name> --kind codex --pane <pane-id>
   ```

   Done when `herdr agent get <name>` reports `idle` and the doctor now names a backend pid.

3. **Drive one prompt per run.** Write the prompt as a numbered list with an explicit `sleep` before each diagnostics read, so rust-analyzer has published, and end with an instruction to print the raw tool responses behind labels the driver can grep (`ROOT>>>`, `SUBAGENT>>>`). Prompting for a verbatim paste beats asking the agent to summarise: a summary loses the field you came for. Done when `herdr agent prompt --wait` returns and the labelled blocks are in the pane.

4. **Read the evidence, not the agent's account.** See the table below. Done when every claim in the result traces to a captured file or a doctor line.

5. **Reset before the next run.** Rewrite the source files to their clean state, wait for the language server to settle, quit the subject with two `ctrl+c`, and start a fresh one. A fresh subject means a fresh session id, which is what makes the next run independent. Done when the sources are clean and `herdr agent start` has returned for the new name.

## Evidence sources

| Source | Proves |
|---|---|
| `mcpls hook doctor` in the checkout | Which endpoint each door reached, the backend pid and version, attached sessions, which language servers are up, watcher coverage |
| `hooklog/*.payload.json` | The host's real payload shape: `hook_event_name`, `cwd`, `session_id`, `agent_id`, and the `apply_patch` envelope in `tool_input.command` |
| `hooklog/*.env.txt` | The environment the hook process actually had, and mcpls's exit status |
| `mcplog/in.jsonl` | `_meta.threadId` and `x-codex-turn-metadata` per tool call, and whether root and subagent share one connection |
| The pane transcript | The tool responses the agent received, which is the behaviour under test |

An empty hook stdout proves nothing on its own. Payload and socket failures both exit cleanly without output, so read `hooks seen` in the doctor to confirm the hook reached the backend.

## One-variable runs

A pilot run is expensive and its subject is nondeterministic, so spend the runs on isolating a cause rather than on re-observing a failure. Hold everything fixed, move one thing, and record the result as a row. The attribution bug found by this method took five runs:

| Root called a tool before editing | Subagent read diagnostics | Root received its own file |
|---|---|---|
| no | no subagent at all | yes |
| no | subagent wrote, never read | yes |
| yes | yes | yes |
| no | yes | no |
| no | yes | no |

Two rows repeat the failing combination on purpose: a nondeterministic subject earns one confirmation before the row counts.

A table isolates a correlate, not a cause. The first column above is a proxy for something it does not name: any tool call starts the language server, so a run that warms up has a settled server before the edit and a run that does not takes its first snapshot mid-run. That window held the bug. The second column tracked the failure across all five runs and its mechanism was never established. Confirm a row against the code before writing it up as a diagnosis; instrumenting the suspect path and rerunning the failing row costs one more run and settles it.

## Gotchas

- **Codex re-asks for hook trust whenever the config changes.** The subject sits at a dialog and `herdr agent prompt --wait` returns `agent_prompt_stalled` with status `idle`. Read the pane, answer the dialog with `herdr agent send-keys`, and say in the result that you answered it.
- **A tool's annotations decide whether a headless run may call it.** Codex reads `readOnlyHint` as a claim about modifying the environment and denies a tool that claims otherwise under `approval_policy = "never"`. `default_tools_approval_mode = "approve"` on the server entry unblocks the run, but read the annotation before reaching for it: the denial may be the finding.
- **Subagents come from `features.multi_agent_v2`.** The config above sets `[agents] enabled` beside it, though Codex's own schema says that one defaults to true and that the feature flag takes precedence, so the second line may be redundant. Untested separately.
- **A `CODEX_HOME` under `/tmp` warns about PATH aliases.** Codex refuses to create helper binaries in a temporary directory and proceeds anyway. Harmless.
- **devkit intercepts `git commit` and blocks computed write paths.** In the scratch checkout, skip the commit. For edits to rig files, write the whole file with a heredoc at a literal path rather than reaching for `sed -i` or a script that builds the path.
- **The backend outlives the subject** by its idle timeout, so a run that starts immediately after the previous subject exits may attach to the old backend. Either wait it out or read the pid from the doctor and say which backend served the run.
