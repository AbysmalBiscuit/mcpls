# Diagnostics delivery: `[diagnostics]`

Sets which diagnostics reach the agent, how many, and when a workspace counts as settled. A server's own `diagnostics_severity` overrides `severity` for that server.

| Field | Type | Default | Notes |
|---|---|---|---|
| `severity` | `"off"` \| `"error"` \| `"warning"` \| `"information"` \| `"hint"` | `"warning"` | The least severe diagnostic worth delivering. A diagnostic with no severity clears every floor but `"off"`. |
| `max_per_file` | integer | `10` | Most diagnostics delivered for one file in one flush. `0` disables the limit. |
| `max_total` | integer | `50` | Most diagnostics delivered in one flush across every file and server, as a context budget. `0` disables the limit. |
| `settle_quiet_ms` | integer (ms) | `1000` | How long the language servers must report no work before their view of the workspace counts as complete. |
| `settle_deadline_ms` | integer (ms) | `300000` | How long to wait for that quiet before baselining anyway, bounding a server that never finishes. |

## Write-tool footers

With `footer = true`, the tools that write files append the diagnostics their edit produced to their own result.

| Field | Type | Default | Notes |
|---|---|---|---|
| `footer` | boolean | `false` | Turns footers on. |
| `footer_grace_ms` | integer (ms) | `250` | Wait before looking for quiet. A check any sooner can land before the server starts rechecking and report the state from before the edit. |
| `footer_quiet_ms` | integer (ms) | `200` | How long nothing may be outstanding before the footer counts the check as done. |
| `footer_wait_ms` | integer (ms) | `15000` | Total wait before the footer reports what it has. The wait ends on quiet, not on the timer, so a fast workspace still returns in about a second. Lower it only if a slow build should report stale results sooner. |

## `[diagnostics.hooks]`

How the agent plugin's hooks reach the running backend.

| Field | Type | Default | Notes |
|---|---|---|---|
| `enabled` | boolean | `true` | Whether the hook listener binds. With it off, nothing injects diagnostics between turns, and an edit alone no longer starts a language server for a language that has none running; a tool call still does. The backend's watcher does not depend on this, so changes on disk still reach the servers that are running. |
| `op_deadline_ms` | integer (ms) | `1500` | How long one hook operation may run before it answers anyway, so a stuck backend never blocks the agent. Work already started may continue, but delivery is not guaranteed. |
| `sweep_quiet_ms` | integer (ms) | `500` | How long pending saves must be quiet before the sweep runs. Each save restarts rust-analyzer's check, so sweeping mid-burst yields cancelled checks and no diagnostics. |
