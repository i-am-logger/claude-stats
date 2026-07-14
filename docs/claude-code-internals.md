# Claude Code internals: sessions, agents, teams, workflows — on-disk reference

Reference for claude-stats scanner development. Sources: extracted strings from the Claude Code bundle (v2.1.205, `GIT_SHA 4cf2699a`; minified identifiers cited in brackets for cross-reference against future extractions) plus live-state verification of `~/.claude` written by v2.1.198–2.1.205. Facts marked **[live]** were confirmed on disk; facts marked **UNCERTAIN** were not. §7.1's `workflowProgress`/task-output findings were added later, verified against v2.1.207 (binary string search + a real polled 2-phase probe run).

---

## 1. Directory map (`~/.claude/`)

```
~/.claude/
├── sessions/<pid>.json              # live-session registry (THE liveness source)
├── projects/<proj>/                 # <proj> = cwd with '/' → '-'
│   ├── <sessionId>.jsonl            # parent transcript
│   └── <sessionId>/                 # per-session sibling dir
│       ├── subagents/
│       │   ├── agent-<agentId>.jsonl        # Task-tool subagents AND in-process teammates
│       │   ├── agent-<agentId>.meta.json    # sidecar metadata
│       │   └── workflows/<runId>/           # per workflow run
│       │       ├── journal.jsonl            # started/result cache journal
│       │       └── agent-<id>.jsonl/.meta.json
│       ├── workflows/
│       │   ├── <runId>.json                 # run snapshot, written on completion
│       │   └── scripts/<slug>-<runId>.js    # persisted workflow script
│       ├── tasks/<taskId>.output            # background stdout/progress streams
│       └── tool-results/*.txt
├── teams/<teamName>/
│   ├── config.json                  # roster: lead + members
│   └── inboxes/<memberName>.json    # JSON-array mailboxes
├── tasks/<listId>/                  # shared TaskCreate/TaskUpdate lists
│   ├── <n>.json  .highwatermark  .lock
├── jobs/<short8>/  (state.json, adopt.json)   # bg-job/daemon supervisor state
├── daemon.json  daemon.log  daemon.lock  daemon/roster.json
└── scheduled_tasks.json             # durable cron
```

Note: per-session temp outputs also live under the project temp dir, e.g. `/tmp/claude-<uid>/<proj>/<sessionId>/tasks/<taskId>.output` — the `outputFile` in AgentOutput points there. **[live]**

---

## 2. Parent session JSONL (`projects/<proj>/<sessionId>.jsonl`)

### 2.1 Entry taxonomy (authoritative, from the routing map `Qwp`)

- Conversation (`dedup-transcript` policy): `user`, `assistant`, `system`, `attachment`, `progress`
- Always-kept metadata: `summary`, `custom-title`, `ended-by-model`, `ai-title`, `last-prompt`, `tag`, `relocated`, `agent-name`, `agent-color`, `agent-setting`, `pr-link`, `frame-link`, `bridge-session`, `file-history-snapshot`, `attribution-snapshot`, `mode`, `permission-mode`, `isolation-latch`, `worktree-state`, `queue-operation`, `marble-origami-commit/-snapshot/-reset`
- Routed-by-agent: `content-replacement`, `fork-context-ref`

Live census confirms `assistant, user, queue-operation, permission-mode, mode, last-prompt, ai-title, system, pr-link, file-history-snapshot`. **[live]**

### 2.2 Per-entry stamps

Every user/assistant/system entry is stamped at append time (`insertMessageChain`) with: `parentUuid, logicalParentUuid, isSidechain, teamName, agentName, promptId (user only), agentId, sessionKind ("interactive"|"bg"|"daemon"|"daemon-worker"), userType, entrypoint, cwd, sessionId, version, gitBranch, slug`.

- `gitBranch` is **per-entry and live**: cached branch invalidated by an fs watcher on `.git/HEAD`/`config`; changes on branch switch mid-session. Absent on git failure. **[live]**
- `cwd` per entry from the process cwd (can drift from the project dir key after `cd`).

### 2.3 System subtypes

`compact_boundary` (`{subtype, compactMetadata:{trigger, preTokens, ...}}`), `microcompact_boundary`, `read_divider`, `local_command`; live-observed also `turn_duration`, `stop_hook_summary`, `away_summary`.

### 2.4 `progress` entries are dead

`data.type` ∈ `{bash_progress, powershell_progress, mcp_progress, repl_tool_call}` are UI-ephemeral; **zero** `{"type":"progress"}` lines exist in any recent live JSONL (parent or subagent). Subagent activity is NOT in the parent JSONL — it lives in `subagents/` transcripts. `agent_progress` events exist only in-memory / in `.output` files.

### 2.5 `queue-operation`

`{type:"queue-operation", operation:"enqueue"|"dequeue"|"remove", timestamp, sessionId, content? (enqueue only)}` — exhaustive operation list. A trailing unmatched `enqueue` = prompt queued. **[live: 70/38/32 counts]**

### 2.6 Agent lifecycle markers in the parent JSONL

- **Sync agent completes**: the Agent `tool_result` itself; `toolUseResult.status == "completed"` with usage/toolStats. No other persisted trace during the run.
- **Background agent launch**: user row with `toolUseResult = {status:"async_launched", agentId, isAsync:true, description, prompt, resolvedModel, outputFile, canReadOutputFile}`. **[live, exact key set]**
- **Workflow launch**: user row with `toolUseResult = {status:"async_launched", taskType:"local_workflow", taskId, runId, workflowName, scriptPath, transcriptDir, summary}` — no `agentId`. **[live, exact key set]**
- **Teammate spawn**: `toolUseResult = {status:"teammate_spawned", prompt, teammate_id, agent_id ("<name>@<team>"), agent_type, model, name, color, tmux_session_name, tmux_window_name, tmux_pane_id (all "in-process" for in-process), team_name, is_splitpane, plan_mode_required}`. **[live]**
- **Background/monitor/workflow terminal**: a `queue-operation` enqueue whose content is a `<task-notification>` block, plus a user message with the same text:
  ```
  <task-notification>
  <task-id>…</task-id> <tool-use-id>toolu_…</tool-use-id>
  <output-file>…/tasks/<id>.output</output-file>
  <status>completed</status> <summary>Agent "…" finished</summary> …
  ```
  Status reflects registry terminal statuses `completed|failed|killed` (summaries: "completed (exit code N)"/"failed"/"was stopped"). Notification is suppressed for `quietlyParked` tasks and if an equivalent queue command is already pending; crashes produce none (orphan summaries handle >20 orphans within 48h).
- **Teammate messages**: every teammate→lead delivery is a user message with text `<teammate-message teammate_id="<name>" color? summary?>…</teammate-message>` (tag constant `Pq`). Protocol frames ride as raw JSON inside the tag, e.g. `{"type":"idle_notification","from":…,"idleReason":"available"}`. **[live]**
- **Teammate end**: NO dedicated JSONL event. Lead receives a system-authored teammate-message containing `{"type":"teammate_terminated","message":"X has shut down. N task(s) were unassigned: …"}`. Team-file member removal leaves no JSONL trace.

---

## 3. Live-session registry (`~/.claude/sessions/<pid>.json`)

Written once at startup (dir mode 0700), merged-updated on transitions, `unlink`ed in the exit handler (survives crashes → must validate).

Fields: `pid, sessionId, cwd, startedAt, procStart (string: /proc/<pid>/stat starttime ticks), version, peerProtocol, kind ("interactive"|"bg"|"daemon"|"daemon-worker"), entrypoint, name, nameSource, status, updatedAt, statusUpdatedAt`; optional `messagingSocketPath, jobId, bridgeSessionId, logPath, tempo ("active"|"idle"|"blocked"), needs, waitingFor, tmux`. **[live]**

- `status` enum: `["busy","shell","idle","waiting"]`; `waitingFor`: `"permission prompt"|"worker request"|"sandbox request"|"dialog open"|"input needed"`.
- `sessionId` is **rebound on /resume or fork** — the file tracks the process's current session.
- Updates are event-driven (status transitions), **not** a heartbeat. Do not age files out by `updatedAt`.
- **Canonical liveness check** (what the CLI itself does, `Bnt`/`y2`/`lC`): (a) `kill(pid,0)` succeeds AND (b) stored `procStart` equals the 22nd field of `/proc/<pid>/stat` (parse text after the last `)`; field 22 overall). Missing `procStart` ⇒ lenient alive-if-pid-alive. Secondary: ping `messagingSocketPath` UDS (250ms timeout, EBUSY ⇒ alive). The same pid+procStart pattern recurs in `daemon/roster.json`, `jobs/*/adopt.json`, `.in_use/<pid>` markers.

---

## 4. Agent tool, ids, registry types

### 4.1 Input/Output

Input (`AgentInput`): `description, prompt, subagent_type?, model? ("sonnet"|"opus"|"haiku"|"fable")`, `run_in_background?` (**default true**), `name?` (makes it a SendMessage-addressable teammate), `team_name?` (**deprecated; ignored — one implicit team per session**), `mode?, isolation? ("worktree"|"remote")`.

Output statuses: `"completed"` (sync, with usage/toolStats/totalTokens), `"async_launched"` (background local, with `outputFile`), `"remote_launched"`, and runtime `"teammate_spawned"`.

Restrictions: in-process teammates cannot spawn background agents; teammates cannot spawn teammates (`name`/`mode` unavailable); SDK/non-interactive: only synchronous subagents. Depth: `spawnDepth` = parent depth + 1, hard cap **5**.

### 4.2 Agent id formation (`TO`)

`name` sanitized to `[\w-]{1,63}`; `hex = randomBytes(8).hex` (16 chars); id = `a<name>-<hex>` or `a<hex>`. Validation regex: `/^a(?:[\w-]{1,63}-)?[0-9a-f]{16}$/`. Transcript filename: `agent-<id>.jsonl`. **[live: `agent-a043be948adef5e7e.jsonl`, `agent-aconstitutive-extender-2fa621aa89ce1eb9.jsonl`]**

Non-agent task ids: prefix letter + 8 base36 chars. Prefix map (exhaustive registry `type` list): `local_bash:"b"`, `local_agent:"a"`, `remote_agent:"r"`, `in_process_teammate:"t"`, `local_workflow:"w"`, `monitor_mcp:"m"`, `monitor_ws:"s"`, `mcp_task:"k"`, `dream:"d"`.

`taskKind` (transcript meta field) has exactly **one** literal value in the whole binary: `"in_process_teammate"`. Plain subagents have no `taskKind`.

### 4.3 Subagent transcripts and `.meta.json`

- Path: `<proj>/<sessionId>/subagents/agent-<id>.jsonl`; workflow agents redirected to `subagents/workflows/<runId>/`.
- Writer: the **parent process's** SessionMirror; sidechain entries (`isSidechain:true` + `agentId`) go to the agent file; flush on a **100 ms** timer (transcript mtime can lag writes by up to ~100 ms).
- `.meta.json` (`agent-<id>.meta.json`, atomic write, mode 0600): exhaustive field set `agentType, isFork, worktreePath, worktreeBranch, cwd, spawnMode, description, name, toolUseId, parentAgentId, stoppedByUser (bool), spawnDepth (int), taskKind, teamName, color, planModeRequired, customAgentType, model, permissionMode, isObserver`. Written at spawn; `stoppedByUser:true` added when the user stops the agent, stripped on resume. **[live examples: plain `{agentType, description, name, toolUseId}`; teammate adds `taskKind:"in_process_teammate", teamName, color, model, permissionMode, spawnDepth}`]**

---

## 5. Teams

### 5.1 `teams/<teamName>/config.json`

`{name, createdAt, leadAgentId, leadSessionId, members:[…]}`. Lead member: `{agentId:"team-lead@<team>", name:"team-lead", agentType:"team-lead", joinedAt, tmuxPaneId:"leader", cwd, subscriptions:[], backendType:"in-process"}`. Teammate members add `model, color, prompt, planModeRequired, agentType`, plus `isActive?` (toggled by `setMemberActive` on idle/active), `mode?`, `worktreePath?`. AgentId = `<name>@<teamName>`. Default team name for the implicit session team: `session-<first 8 hex of sessionId>`. **[live: 19-member config, lead keys `agentId,agentType,backendType,cwd,joinedAt,name,subscriptions,tmuxPaneId`]**

All writes go through a `.lock` (proper-lockfile, 10 retries).

`backendType`: `"in-process"` (default; teammate runs inside the lead's process → shares the lead's PID), `"tmux"`, `"iterm2"` (pane-backed: separate CLI process spawned with `--agent-id --agent-name --team-name …`).

### 5.2 Member lifecycle

Added: `reserveTeammateIdentity` at spawn (`joinedAt: Date.now()`); re-added on resume (`joinedAt` **resets** to now). Removed (all **best-effort** — stale members survive crashes):
- spawn-failure rollback;
- kill / in-process `idle_timeout` exit (`removeMemberByAgentId`);
- **graceful shutdown**: the lead's InboxPoller removes the member when a `shutdown_approved` frame arrives (`removeTeammateFromTeamFile`).

Whole-team: at lead process exit, session-created teams are `rm -rf`'d (`cleanupSessionTeams`) — best-effort; stale team dirs persist after crashes. **[live: many stale `session-*` dirs]**

Therefore: **absence from a readable config = terminated (shutdown or kill)**; presence ≠ alive; missing team dir = cleaned up or never existed.

### 5.3 Inboxes (`teams/<team>/inboxes/<name>.json`)

JSON **array**, appended under `<path>.lock`. Message: `{type?, from, text, timestamp, color?, summary?, read?, msgV:1, msg_id}`; missing `type` ⇒ `"message"`. Protocol frames (as JSON in `text`): `idle_notification {idleReason:"available"|"interrupted"|"failed", summary?, completedTaskId?, completedStatus?, failureReason?}`, `shutdown_request {requestId, reason?}`, `shutdown_approved {requestId, paneId?, backendType?}`, `shutdown_rejected`, `teammate_terminated {message}`, `task_assignment`, `task_completed`, `plan_approval_*`, `permission_*`, `mode_set_request`.

`idle_notification` producer is always the **teammate**, addressed to `team-lead`: pane teammates via a registered Stop hook; in-process teammates after each turn. The lead's InboxPoller injects mail into the conversation **only when the lead is idle**; otherwise messages queue in memory — parent-JSONL delivery of idle notifications can lag minutes or be lost on crash. The inbox file itself is immediate.

### 5.4 Shutdown protocol

lead `shutdown_request` → teammate model approves via SendMessage `shutdown_response` → teammate writes `shutdown_approved` to lead inbox and exits (in-process: aborts own controller; pane: graceful exit) → lead InboxPoller kills the pane if any, removes the member from config.json, unassigns the member's incomplete shared tasks (`owner:undefined, status:"pending"`), marks the registry task `completed` with `evictAfter = now+3000`, and enqueues a `teammate_terminated` frame.

### 5.5 Roster UI semantics (the `● main` / `○ name` picker)

- Rows come from the in-memory task registry (types `local_agent`/`in_process_teammate`), **not** the team file.
- `●` vs `○` is a **view cursor** (which conversation is displayed), not a run state.
- Terminal rows (`completed|failed|killed`) evict after `evictAfter`: **3 s** after shutdown-approved, **30 s** default (`eae=30000`), 0 on manual clear.
- **Idle teammates are never evicted** — they show state "idle" (no token text) and collapse into one `idle_summary` row when **more than 3** are idle.
- Elapsed = `now − (turnStartTime ?? startTime)` — **time in the current turn** (turnStartTime resets each turn); finished rows show `endTime − startTime`.
- Token chip = `latestInputTokens (input+cache_creation+cache_read of latest assistant msg) + cumulative output_tokens across the run`; `↓` merely means "recent tool activity recorded".

### 5.6 The idle/active flip — two mechanisms, one on disk

The roster's "idle" vs "working" state is **not** a single flag claude-stats can read directly. There are two independent implementations depending on backend, discovered via binary string search against v2.1.207 (`grep`-able JS identifiers cited in brackets):

- **In-process (default) teammates**: `tasks[id].isIdle`, a boolean on the **in-memory task registry** inside the running CLI process. Cleared (`isIdle:false`) at the start of every new turn (the `[inProcessRunner]` per-turn loop's first statement, alongside `status:"running"` and a fresh `turnStartTime`); set (`isIdle:true`) at turn end, which also fires `onIdleCallbacks` and sends the `idle_notification` mailbox message. **This flag is never persisted to disk** — there is no file claude-stats (an out-of-process scanner) can read it from, for the common case that covers every teammate observed live in this repo's sessions.
- **Out-of-process (tmux/iTerm2) teammates**: `member.isActive` in `teams/<team>/config.json` [`setMemberActive`], flipped `true` at the top of the teammate's own query-submit handler and `false` from a registered `Stop` hook (same moment the `idle_notification` fires) or on failure. This one *is* on disk, but only applies to the pane-backed backend, not in-process.

**Consequence for claude-stats**: since the authoritative in-process flag is unreadable, claude-stats' own approximation — the `idle_notification` mailbox message relayed into the parent JSONL — is the *only* signal available, and it has a real staleness problem: per §5.3, that relay only happens once the lead itself goes idle, so delivery can lag **minutes** behind the real transition. A teammate can go idle, get a new mailbox message, and resume actual work (writing new `user`/`assistant` turns to its own transcript) well before its *first* `idle_notification` is even delivered — live-verified with real timestamps: a 44-minute relay lag on one delivery, during which the teammate had already resumed and completed a whole new turn. **Fix applied**: `parse_teammate` (`subagents.rs`) no longer lets a `status.is_idle()==true` verdict override the state independently computed from the teammate's own transcript tail (`detect_state_from_tail`) — that tail read is the fresher, more reliable signal whenever the two disagree. The `is_idle()` mailbox-notification signal is now only trusted for the tight 30s roster-hide window (`TEAMMATE_IDLE_HIDE_SECS`) when the transcript tail *also* independently reads as idle.

Separately: because delivery only happens while the lead is idle, **several different teammates' notifications can batch into one relayed JSONL entry** as multiple concatenated `<teammate-message>` blocks (live-observed: 10 idle notifications from 5 teammates in one entry). Each block must be parsed independently — extracting `teammate_id`/`timestamp` from the whole concatenated string only ever finds the *first* block's values.

---

## 6. Shared task lists (`~/.claude/tasks/<listId>/`)

`listId = $CLAUDE_CODE_TASK_LIST_ID || teamName || sessionId`. One `<n>.json` per task: `{id, subject, description, activeForm?, owner?, status ∈ pending|in_progress|completed, blocks[], blockedBy[], metadata?}` + `.highwatermark` (ids never reused) + `.lock`. Linked to teams via `owner`; on teammate termination its incomplete tasks are unassigned. Dirs are precreated eagerly (many empty `session-*` dirs) and are historical after session end.

---

## 7. Workflows

### 7.1 Run lifecycle

- runId `wf_<uuid.slice(0,12)>`, validated `/^wf_[a-z0-9-]{6,}$/`. `resumeFromRunId` **reuses** the same runId/run dir/journal.
- Script persisted to `<proj>/<sessionId>/workflows/scripts/<slug>-<runId>.js` (slug: lowercase, `[^a-z0-9]+`→`-`).
- Run dir `<proj>/<sessionId>/subagents/workflows/<runId>/`: `journal.jsonl` + `agent-<id>.jsonl` transcripts.
- **Snapshot on completion**: `<proj>/<sessionId>/workflows/<runId>.json` with `{runId, timestamp, taskId, script, scriptPath, args, result, agentCount, logs, durationMs, error, summary, workflowName, title, status, startTime, phases, defaultModel, workflowProgress, totalTokens, totalToolCalls}`. Reader defaults `status ?? (error ? "failed" : "completed")`. The `/workflows` browser lists these — completed runs outlive the roster.
- Task `.output` file: `/tmp/claude-<uid>/<proj>/<sessionId>/tasks/<taskId>.output` (taskId comes from the launch event, §2.6). **[live, empirically confirmed]** Written *during* the run, not only on completion — polled at 1.5 s intervals against a live 2-agent/2-phase run, the file held complete `workflowProgress` data (both agents `state:"done"`) a full ~9 s before the task's terminal `<task-notification>` fired. Same top-level shape as the snapshot's `workflowProgress`/`totalTokens`/`agentCount`, but no `status`/`workflowName` keys; also carries `result`/`logs` (the agents' full output — can be tens of KB+, don't deserialize into a struct that keeps them). This is the only source for a still-running run's phase breakdown — the completion snapshot doesn't exist yet.
- `workflowProgress` (present in both the file above and the completion snapshot) is an array of two entry shapes:
  - `{type:"workflow_phase", index, title}` — one per `meta.phases` entry, in declaration order.
  - `{type:"workflow_agent", index, label, phaseIndex, phaseTitle, agentId, model, state, startedAt, queuedAt, attempt, lastToolName?, lastToolSummary?, lastProgressAt, tokens, toolCalls, durationMs, promptPreview, resultPreview}` — one per dispatched agent (including respawns — dedupe the way §7.2 dedupes journal keys if that matters to a consumer). `state` seen so far: `"done"`; other values (running/queued/error) presumed but not directly observed. Runs with no declared `meta.phases` have no `workflow_phase` entries and agents carry null `phaseIndex`/`phaseTitle`.

### 7.2 `journal.jsonl`

Exactly two entry types: `{type:"started", key, agentId}` (agent began) and `{type:"result", key, agentId, result}` (appended **only when result ≠ null** — failed and user-skipped agents leave a dangling `started` forever). Unparseable lines skipped. Cache keys are chained sha256 (`v2:<64hex>`) — cache hits are the longest unchanged prefix of `agent()` calls; a key with `started` but no `result` respawns live on resume. **Respawns (stall/throttle/user-retry, up to 5 attempts) append additional `started` lines with fresh agentIds for the same `key`** — count progress by `key`, not agentId.

### 7.3 Roster row (CC's own math)

Computed **in-memory from progress events, not the journal**: `done` = entries in state "done", `failedCount` = state "error", `total = max(agentCount, entriesSeen)`. Row text: `<name> — done/total agents done[ · N failed]` + elapsed + `↓ <tokens> tokens`. `totalTokens` = Σ over agents of `input+cache_creation+cache_read+output` of each agent's **latest** assistant message (all-in, cache reads included). Terminal rows linger 30 s (`evictAfter = now + 30000`). Name fallback chain: `workflowName ?? summary ?? description ?? "Dynamic workflow"`.

### 7.4 Workflow agents

agentType `workflow-subagent` (disallowed: SendUserMessage, Agent, Workflow). Spawn depth 1 from main; markers vs Task agents: `workflowRunId`/`workflowName`, `isBackgroundAgent:true`, transcript under the run dir. Concurrency: local semaphore `clamp(cpus−2, 2, 16)`; remote 50; per-agent stall timeout 180 s default; agent-call cap 1000; script cap 512 KB.

### 7.5 Adoption

Exit handoff records `{taskId, workflowRunId, scriptPath, scriptSha256, argsJson, transcriptDir…}`; adopted runs in a successor session **symlink** the new run dir to the original (after verifying `journal.jsonl` exists) — the same run can appear under two sessions; dedup by runId.

### 7.6 Loops

`/loop`/CronCreate use sentinel `<<autonomous-loop>>`; `ScheduleWakeup` uses `<<autonomous-loop-dynamic>>` (delay clamped [60, 3600] s). Session-scoped crons — they do **not** appear in the tasks/workflows roster and have no statusline indicator.

---

## 8. Liveness composition ("what is running right now")

No single file — three-part composition (this is what the internal ListAgents does):
1. **Sessions/processes**: `sessions/*.json` validated by pid+procStart — the only ground truth.
2. **Teammates**: `teams/<team>/config.json` members (∧ lead pid alive for in-process; `isActive` flag advisory; even ListAgents reports teammate `lastActive: undefined`).
3. **Task-tool subagents / workflow agents**: no registry — infer from `subagents/agent-*.jsonl` mtime + parent-JSONL launch/terminal markers, and for workflows from journal `started`-without-`result` (crash paths never write `result`).

## 9. Constants table

| Constant | Value | Meaning |
|---|---|---|
| `eae` | 30000 ms | default terminal-row eviction linger |
| `zzt` | 3000 ms | eviction after teammate shutdown_approved |
| `Ofa` | 3 | idle teammates collapse threshold (>3) |
| depth cap | 5 | subagent nesting limit |
| flush | 100 ms | transcript write flush interval |
| stall | 180000 ms | workflow per-agent stall timeout (default) |
| respawn | 5 | workflow agent respawn attempts |
| agent cap | 1000 | workflow agent-call cap |
| local concurrency | clamp(cpus−2, 2, 16) | workflow local agent semaphore |
| script cap | 524288 B | workflow script size |
| journal key | `v2:` prefix | chained sha256 cache key |

## 10. Known uncertainties

- Producer of the in-process runner's `idle_timeout` branch (may be legacy).
- Whether `sr` (workflow per-agent token carryover) accrues across turns within one attempt.
- Whether interrupted workflow agents ever get a `result` line (crash paths won't).
- `wf-\d+` agent-id form's emitter.
- The exact hollow-circle glyph (`Be.circle`) is platform-dependent.
- Which CC version dropped persisted `progress` entries (they are gone in 2.1.198+; the routing policy for them still exists).
- Full `workflow_agent.state` vocabulary in `workflowProgress` — only `"done"` has been directly observed; running/queued/error states are presumed to exist (the roster clearly distinguishes them, §7.3) but weren't captured live.
- Whether the task-output file is deleted/truncated after the run's completion snapshot is written, or left in place indefinitely.
