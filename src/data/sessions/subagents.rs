#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use super::activity::detect_state_from_tail;
use super::tail::{
    entry_timestamp, extract_tokens, is_assistant_usage, is_real_user_turn, parse_json_line,
    seek_tail, TeammateStatus,
};
use super::{ModelShort, PhaseProgress, SessionState, SubagentData};
use crate::fmt::truncate_str;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Hide workflow-run agents whose transcript hasn't been written for this
/// long. A crashed or killed run never writes its `result` journal lines, so
/// journal orphans alone would render forever. Live agents write at least
/// every ~60s; a single long model turn can stay silent for ~15min; dead runs
/// observed in practice are stale by many hours.
pub(crate) const AGENT_STALE_SECS: u64 = 1800;

/// How long a row that just went terminal lingers before disappearing —
/// Claude Code evicts terminal roster rows ~30s after completion for every
/// row kind (workflow runs, plain/sync agents, teammate shutdown/removal).
/// Idle (but not terminated) teammates are a different case and are never
/// evicted by this or any other timeout, matching Claude Code's own roster.
pub(crate) const RUN_DONE_LINGER_SECS: u64 = 30;

/// Scan subagent JSONL files and return data for agents that are still active.
///
/// `active_ids` is the set of background agent IDs that parent-JSONL
/// tracking shows as still in-progress; `completed_tool_ids` are `tool_use`
/// ids with a `tool_result` (finishing synchronous agents, joined via their
/// meta.json `toolUseId`). `teammates` maps named-teammate lifecycle
/// statuses observed in the parent JSONL. `workflow_names` maps workflow
/// `runId` to its human name, and `workflow_task_ids` maps `runId` to its
/// `taskId` (used to locate the run's live progress file) — both captured
/// from the parent JSONL's launch event, available before the run's
/// script/snapshot are. Workflow runs under `subagents/workflows/<run-id>/`
/// aggregate into a single row each.
#[allow(
    clippy::implicit_hasher,
    reason = "internal API, always called with std collections"
)]
pub fn scan_subagents(
    subagents_dir: &Path,
    active_ids: &HashSet<String>,
    completed_tool_ids: &HashSet<String>,
    teammates: &HashMap<String, TeammateStatus>,
    workflow_names: &HashMap<String, String>,
    workflow_task_ids: &HashMap<String, String>,
) -> Vec<SubagentData> {
    let mut agents = collect_agents(subagents_dir, active_ids, completed_tool_ids, teammates);

    agents.extend(scan_workflow_runs(
        subagents_dir,
        workflow_names,
        workflow_task_ids,
    ));

    dedup_teammates(&mut agents);

    // Chronological (oldest spawn first) so rows don't reshuffle between
    // scans; task title as a deterministic tiebreak.
    agents.sort_by(|a, b| {
        b.runtime_secs
            .unwrap_or(0)
            .cmp(&a.runtime_secs.unwrap_or(0))
            .then_with(|| a.task.cmp(&b.task))
    });
    agents
}

/// A respawned teammate leaves multiple transcripts carrying the same name —
/// keep only the most recently written one per name.
fn dedup_teammates(agents: &mut Vec<SubagentData>) {
    let mut newest: HashMap<String, u64> = HashMap::new();
    for agent in agents.iter() {
        if let Some(name) = &agent.name {
            let entry = newest.entry(name.clone()).or_insert(u64::MAX);
            *entry = (*entry).min(agent.last_write_age_secs);
        }
    }
    agents.retain(|a| match &a.name {
        Some(name) => newest.get(name) == Some(&a.last_write_age_secs),
        None => true,
    });
}

/// Collect active `agent-<id>.jsonl` transcripts directly inside `dir`.
fn collect_agents(
    dir: &Path,
    active_ids: &HashSet<String>,
    completed_tool_ids: &HashSet<String>,
    teammates: &HashMap<String, TeammateStatus>,
) -> Vec<SubagentData> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let teams_dir = default_teams_dir();

    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                return None;
            }
            let name = path.file_stem()?.to_str()?;
            // Extract agent ID from filename: "agent-a2a043d116fbd87f0" → "a2a043d116fbd87f0"
            let agent_id = name.strip_prefix("agent-")?;
            let meta = read_meta(&path);

            // Named teammates: meta taskKind is authoritative; the id shape
            // (a<name>-<hash>) is the fallback for meta-less transcripts.
            let is_teammate =
                meta.as_ref().and_then(|m| m.task_kind.as_deref()) == Some("in_process_teammate");
            let fallback_name = teammate_name_of(agent_id);
            if is_teammate || fallback_name.is_some_and(|n| teammates.contains_key(n)) {
                let tm_name = meta
                    .as_ref()
                    .and_then(|m| m.name.clone())
                    .or_else(|| fallback_name.map(String::from))?;
                let status = teammates.get(&tm_name).copied().unwrap_or_default();
                return parse_teammate(&path, &tm_name, &status, teams_dir.as_deref());
            }

            // Background agents tracked via async_launched/task-notification
            if active_ids.contains(agent_id) {
                return parse_subagent(&path).map(|(data, _)| data);
            }

            // Synchronous agents write no persisted progress entries — they
            // are active while their spawning tool_use has no tool_result in
            // the parent JSONL and they weren't stopped by the user.
            let meta = meta?;
            if meta.stopped_by_user == Some(true) {
                return None;
            }
            let tool_use_id = meta.tool_use_id.as_deref()?;
            let (data, _) = parse_subagent(&path)?;
            if completed_tool_ids.contains(tool_use_id) {
                // Finished — linger briefly like a workflow run, then
                // disappear, instead of vanishing the instant its
                // tool_result lands.
                return (data.last_write_age_secs <= RUN_DONE_LINGER_SECS).then_some(data);
            }
            // Crashed parents leave no completion marker — cap by staleness.
            (data.last_write_age_secs <= AGENT_STALE_SECS).then_some(data)
        })
        .collect()
}

/// Extract the teammate name from a transcript id of the form
/// `a<name>-<hash>` — e.g. `afix-load-docrefs-fb64396d7b46c88f` →
/// "fix-load-docrefs". Anonymous ids (`a2a043d116fbd87f0`, no dash) yield
/// `None`.
fn teammate_name_of(agent_id: &str) -> Option<&str> {
    let rest = agent_id.strip_prefix('a')?;
    let (name, hash) = rest.rsplit_once('-')?;
    (!name.is_empty() && !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(name)
}

fn parse_teammate(
    path: &Path,
    name: &str,
    status: &TeammateStatus,
    teams_dir: Option<&Path>,
) -> Option<SubagentData> {
    let (mut data, last_user_ts) = parse_subagent(path)?;

    // Any termination path (kill, graceful shutdown, spawn-failure rollback)
    // removes the member from the team config; normal never-shut-down
    // completion does not. Removals are best-effort — a crashed session
    // leaves stale members — so absence means terminated, but presence does
    // NOT mean alive.
    let membership = read_meta(path)
        .and_then(|m| m.team_name)
        .zip(teams_dir)
        .map_or(TeamMembership::Unknown, |(team, dir)| {
            team_membership(dir, &team, name)
        });

    // An explicit "X has shut down" frame or disappearing from the team file
    // are both real termination signals — linger briefly like a finished
    // workflow run (Claude Code evicts terminal roster rows after the same
    // window), then disappear, instead of vanishing the instant termination
    // is detected.
    if status.terminated || matches!(membership, TeamMembership::Absent) {
        return (data.last_write_age_secs <= RUN_DONE_LINGER_SECS).then_some(data);
    }

    if status.is_idle() && data.state == SessionState::Idle {
        // Idle means "available", not terminated. Real Claude Code never
        // evicts idle teammates from its roster — only terminal rows linger-
        // then-disappear — so an idle teammate whose transcript is still on
        // disk keeps showing indefinitely, same as CC's own picker.
    } else if data.last_write_age_secs > AGENT_STALE_SECS {
        // Either genuinely active (never sent an idle notification, or the
        // transcript tail shows fresh work — see below), or a stale/absent
        // idle signal and the transcript has been quiet far longer than any
        // real model turn — abandoned or killed.
        return None;
    }
    // Deliberately NOT forcing `data.state = Idle` when `status.is_idle()` is
    // true but the transcript tail (`detect_state_from_tail`, already
    // computed into `data.state` by `parse_subagent` above) disagrees:
    // `idle_notification` delivery into the parent JSONL can lag minutes
    // behind (it only lands once the lead itself goes idle), so a teammate
    // that resumed via a new mailbox message and is actively mid-turn right
    // now can still be carrying a stale `is_idle()==true` latch — the
    // transcript tail is the fresher, more reliable signal in that case.
    // The meta.json name wins, but the tracker name fills any gap.
    data.name.get_or_insert_with(|| name.to_string());
    // Claude Code's roster shows time-in-current-turn: elapsed since the last
    // user entry in the teammate transcript (a mailbox message starts a new
    // turn). Spawn-time joinedAt is the fallback.
    let turn_start = last_user_ts.or(match membership {
        TeamMembership::Member(joined) => joined,
        _ => None,
    });
    if let Some(start) = turn_start {
        data.runtime_secs = Some(
            (chrono::Utc::now() - start)
                .num_seconds()
                .max(0)
                .cast_unsigned(),
        );
    }
    Some(data)
}

/// Aggregate each workflow run under `subagents/workflows/<run-id>/` into a
/// single roster row: run name, "done/total agents" progress, summed tokens,
/// and runtime — mirroring how Claude Code presents workflow runs.
///
/// Workflow runs don't emit `agent_progress` entries in the parent session
/// JSONL; each run journals into `<run-id>/journal.jsonl` with one
/// `{"type":"started","agentId":...}` line per launched agent and a matching
/// `{"type":"result",...}` line when it completes.
fn scan_workflow_runs(
    subagents_dir: &Path,
    workflow_names: &HashMap<String, String>,
    workflow_task_ids: &HashMap<String, String>,
) -> Vec<SubagentData> {
    let Ok(runs) = fs::read_dir(subagents_dir.join("workflows")) else {
        return Vec::new();
    };
    // Run scripts are saved as <session-dir>/workflows/scripts/<name>-<run-id>.js
    let scripts_dir = subagents_dir
        .parent()
        .map(|session| session.join("workflows").join("scripts"));

    runs.flatten()
        .filter_map(|run| {
            let run_dir = run.path();
            if !run_dir.is_dir() {
                return None;
            }
            // Journals accumulate (one dir per run, results can be large) —
            // skip runs where nothing has been written recently instead of
            // re-parsing dead journals every scan.
            if !dir_recently_written(&run_dir, AGENT_STALE_SECS) {
                return None;
            }
            parse_workflow_run(
                &run_dir,
                scripts_dir.as_deref(),
                subagents_dir,
                workflow_names,
                workflow_task_ids,
            )
        })
        .collect()
}

/// One entry of a workflow run's `workflowProgress` array — Claude Code's own
/// phase-tracking data, present both in the completion snapshot and in the
/// live task-output file. Two entry shapes share this type: `workflow_phase`
/// (`title`, declaration order) and `workflow_agent` (`phaseTitle`, `state`).
/// Unrecognized/absent fields are left `None` rather than erroring — this is
/// live telemetry, not a stable public API.
#[derive(Debug, serde::Deserialize)]
struct ProgressEntry {
    #[serde(rename = "type")]
    kind: String,
    title: Option<String>,
    #[serde(rename = "phaseTitle")]
    phase_title: Option<String>,
    state: Option<String>,
    #[serde(rename = "lastToolName")]
    last_tool_name: Option<String>,
    #[serde(rename = "lastToolSummary")]
    last_tool_summary: Option<String>,
}

/// Reduce a run's raw `workflowProgress` entries to per-phase done/total
/// counts, in the phases' declaration order. Agents whose `phaseTitle`
/// doesn't match a declared phase (or is absent — runs with no `meta.phases`)
/// are dropped: callers treat an empty result as "no phase breakdown",
/// falling back to the flat `done/total agents done` display.
fn derive_phases(entries: &[ProgressEntry]) -> Vec<PhaseProgress> {
    let mut phases: Vec<PhaseProgress> = entries
        .iter()
        .filter(|e| e.kind == "workflow_phase")
        .filter_map(|e| e.title.clone())
        .map(|title| PhaseProgress {
            title,
            done: 0,
            total: 0,
            current_tool: None,
        })
        .collect();
    for entry in entries.iter().filter(|e| e.kind == "workflow_agent") {
        let Some(phase_title) = &entry.phase_title else {
            continue;
        };
        if let Some(phase) = phases.iter_mut().find(|p| &p.title == phase_title) {
            phase.total += 1;
            if entry.state.as_deref() == Some("done") {
                phase.done += 1;
            } else if phase.current_tool.is_none() {
                phase.current_tool = entry
                    .last_tool_summary
                    .clone()
                    .or_else(|| entry.last_tool_name.clone());
            }
        }
    }
    phases
}

/// Run snapshot written to `<session>/workflows/<run-id>.json` when a
/// workflow finishes — the exact terminal signal (journals can't record
/// failures: `result` lines are only written for non-null results).
#[derive(Debug, serde::Deserialize)]
struct RunSnapshot {
    status: Option<String>,
    #[serde(rename = "workflowName")]
    workflow_name: Option<String>,
    #[serde(rename = "totalTokens")]
    total_tokens: Option<u64>,
    #[serde(rename = "workflowProgress", default)]
    workflow_progress: Vec<ProgressEntry>,
    /// Wall-clock run duration, captured once at completion — the
    /// authoritative "how long did this actually take", as opposed to
    /// `now() - started_at` which keeps ticking for as long as the finished
    /// row lingers on the roster.
    #[serde(rename = "durationMs")]
    duration_ms: Option<u64>,
}

/// Live progress file Claude Code polls background/workflow tasks through
/// (`AgentOutput.outputFile`), updated incrementally while a workflow runs —
/// unlike the completion snapshot, this exists and is current *during* the
/// run. Only `workflowProgress` is read; the file also carries `result`/
/// `logs` (the agents' full output, can be very large) which are left
/// undeclared here so serde discards them without allocating.
#[derive(Debug, serde::Deserialize)]
struct TaskOutputProgress {
    #[serde(rename = "workflowProgress", default)]
    workflow_progress: Vec<ProgressEntry>,
}

/// Find `run_id`'s live task-output file under
/// `<tmp_dir>/claude-*/<proj>/<session_id>/tasks/<task_id>.output`. `tmp_dir`
/// is injected (production passes `std::env::temp_dir()`) so this stays
/// testable without touching the real `/tmp`.
fn find_task_output(
    tmp_dir: &Path,
    proj: &str,
    session_id: &str,
    task_id: &str,
) -> Option<PathBuf> {
    let entries = fs::read_dir(tmp_dir).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("claude-"))
        })
        .find_map(|root| {
            let candidate = root
                .join(proj)
                .join(session_id)
                .join("tasks")
                .join(format!("{task_id}.output"));
            candidate.is_file().then_some(candidate)
        })
}

/// Derive `(proj, sessionId)` from a session's `subagents/` dir path
/// (`.../<proj>/<sessionId>/subagents`), for locating that session's temp
/// dir (`/tmp/claude-<uid>/<proj>/<sessionId>/`).
fn proj_and_session(subagents_dir: &Path) -> Option<(&str, &str)> {
    let session_dir = subagents_dir.parent()?;
    let session_id = session_dir.file_name()?.to_str()?;
    let proj = session_dir.parent()?.file_name()?.to_str()?;
    Some((proj, session_id))
}

/// Read a live run's phase progress from its task-output file, given the
/// session's `subagents/` dir and the run's `runId`. Empty if the run's
/// `taskId` was never observed (e.g. this session's launch event predates
/// `taskId` being present), the temp file can't be located, or it doesn't
/// parse — this is best-effort live telemetry, not the source of truth (the
/// completion snapshot is, once a run finishes).
fn read_live_phases(
    tmp_dir: &Path,
    subagents_dir: &Path,
    workflow_task_ids: &HashMap<String, String>,
    run_id: &str,
) -> Vec<PhaseProgress> {
    let Some(task_id) = workflow_task_ids.get(run_id) else {
        return Vec::new();
    };
    let Some((proj, session_id)) = proj_and_session(subagents_dir) else {
        return Vec::new();
    };
    let Some(output_path) = find_task_output(tmp_dir, proj, session_id, task_id) else {
        return Vec::new();
    };
    let Ok(data) = fs::read_to_string(output_path) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<TaskOutputProgress>(&data) else {
        return Vec::new();
    };
    derive_phases(&parsed.workflow_progress)
}

fn parse_workflow_run(
    run_dir: &Path,
    scripts_dir: Option<&Path>,
    subagents_dir: &Path,
    workflow_names: &HashMap<String, String>,
    workflow_task_ids: &HashMap<String, String>,
) -> Option<SubagentData> {
    let journal = fs::File::open(run_dir.join("journal.jsonl")).ok()?;
    // Count by cache KEY, not agentId: stall/throttle respawns append extra
    // started lines with new agentIds for the same key.
    let mut started: HashSet<String> = HashSet::new();
    let mut finished: HashSet<String> = HashSet::new();
    for line in BufReader::new(journal).lines().map_while(Result::ok) {
        let Some(val) = parse_json_line(&line) else {
            continue;
        };
        let Some(key) = val
            .get("key")
            .or_else(|| val.get("agentId"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        match val.get("type").and_then(|v| v.as_str()) {
            Some("started") => {
                started.insert(key.to_string());
            }
            Some("result") => {
                finished.insert(key.to_string());
            }
            _ => {}
        }
    }
    if started.is_empty() {
        return None;
    }
    let total = started.len() as u32;
    let done = started.iter().filter(|key| finished.contains(*key)).count() as u32;

    // The completion snapshot is authoritative for terminal runs (it also
    // catches failed runs whose journal keys dangle as started forever).
    let run_id = run_dir.file_name().and_then(|n| n.to_str())?;
    let snapshot_path = run_dir
        .parent() // .../subagents/workflows
        .and_then(Path::parent) // .../subagents
        .and_then(Path::parent) // <session dir>
        .map(|session| session.join("workflows").join(format!("{run_id}.json")));
    let snapshot = snapshot_path.as_ref().and_then(|p| read_run_snapshot(p));
    let snapshot_age = snapshot_path.as_ref().and_then(|p| file_age_secs(p));

    let run_stats = aggregate_run_transcripts(run_dir);
    let (total_tokens, started_at, last_write_age_secs) = run_stats;

    let terminal = snapshot.is_some() || done >= total;
    if terminal {
        // A finished run lingers briefly (Claude Code evicts terminal rows
        // after 30s), anchored to the snapshot when it exists.
        let terminal_age = snapshot_age.unwrap_or(last_write_age_secs);
        if terminal_age > RUN_DONE_LINGER_SECS {
            return None;
        }
    }

    // A terminal run's elapsed time is frozen at its own recorded duration —
    // otherwise it keeps ticking wall-clock for the whole linger window
    // after the run already finished, which reads as still-running.
    let runtime_secs = match snapshot.as_ref().and_then(|s| s.duration_ms) {
        Some(duration_ms) => Some(duration_ms / 1000),
        None => started_at.map(|t| {
            (chrono::Utc::now() - t)
                .num_seconds()
                .max(0)
                .cast_unsigned()
        }),
    };

    let snapshot_name = snapshot.as_ref().and_then(|s| s.workflow_name.clone());
    let snapshot_tokens = snapshot.as_ref().and_then(|s| s.total_tokens);
    let failed = snapshot
        .as_ref()
        .and_then(|s| s.status.as_deref())
        .is_some_and(|s| s == "failed");

    // The completion snapshot's phase breakdown is authoritative once it
    // exists; a still-running workflow has no snapshot yet, so fall back to
    // its live task-output file. A terminal run without snapshot phase data
    // (e.g. no `meta.phases` declared) has none to fall back to — it's about
    // to evict anyway.
    let snapshot_phases = snapshot
        .as_ref()
        .map(|s| derive_phases(&s.workflow_progress))
        .unwrap_or_default();
    let phases = if !snapshot_phases.is_empty() {
        snapshot_phases
    } else if terminal {
        Vec::new()
    } else {
        read_live_phases(
            &std::env::temp_dir(),
            subagents_dir,
            workflow_task_ids,
            run_id,
        )
    };

    Some(SubagentData {
        task: if failed {
            "failed".to_string()
        } else {
            String::new()
        },
        name: Some(
            snapshot_name
                .or_else(|| workflow_names.get(run_id).cloned())
                .unwrap_or_else(|| workflow_run_name(run_dir, scripts_dir)),
        ),
        model: ModelShort::Unknown,
        context_tokens: snapshot_tokens.unwrap_or(total_tokens),
        runtime_secs,
        last_write_age_secs,
        state: if terminal {
            SessionState::Idle
        } else {
            SessionState::Working
        },
        progress: Some((done, total)),
        phases,
    })
}

/// Aggregate a run's agent transcripts: summed tokens (context + output,
/// matching Claude Code's `totalTokens`), earliest start, freshest write age.
fn aggregate_run_transcripts(run_dir: &Path) -> (u64, Option<chrono::DateTime<chrono::Utc>>, u64) {
    let mut total_tokens: u64 = 0;
    let mut started_at: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut last_write_age_secs = u64::MAX;
    if let Ok(entries) = fs::read_dir(run_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_agent_transcript = path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agent-"));
            if !is_agent_transcript {
                continue;
            }
            let Ok(file) = fs::File::open(&path) else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            let age = metadata
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map_or(0, |d| d.as_secs());
            last_write_age_secs = last_write_age_secs.min(age);
            let (_, first_ts) = read_subagent_head(&file);
            started_at = match (started_at, first_ts) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let stats = read_subagent_usage(&file, metadata.len());
            total_tokens += stats.context_tokens + stats.last_output_tokens;
        }
    }
    if last_write_age_secs == u64::MAX {
        last_write_age_secs = 0;
    }
    (total_tokens, started_at, last_write_age_secs)
}

fn read_run_snapshot(path: &Path) -> Option<RunSnapshot> {
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Seconds since `path` was last modified; `None` if unreadable. A future
/// mtime (clock skew) counts as just written.
fn file_age_secs(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.elapsed().map_or(0, |age| age.as_secs()))
}

/// Last-resort name fallback: reconstruct a workflow run's human name from
/// its persisted script filename (`<session>/workflows/scripts/<name>-<run-id>.js`).
/// Only reached when neither the completion snapshot nor the parent JSONL's
/// launch event (`workflow_names`) had a name yet. Falls back further to the
/// raw run-dir name (`wf_...`).
fn workflow_run_name(run_dir: &Path, scripts_dir: Option<&Path>) -> String {
    let run_id = run_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let suffix = format!("-{run_id}.js");
    if let Some(entries) = scripts_dir.and_then(|d| fs::read_dir(d).ok()) {
        for entry in entries.flatten() {
            if let Some(file_name) = entry.file_name().to_str() {
                if let Some(name) = file_name.strip_suffix(&suffix) {
                    if !name.is_empty() {
                        return name.to_string();
                    }
                }
            }
        }
    }
    run_id.to_string()
}

/// Whether any file directly inside `dir` was modified within `max_age_secs`.
/// A future mtime (clock skew) counts as recent.
fn dir_recently_written(dir: &Path, max_age_secs: u64) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .is_some_and(|t| {
                t.elapsed()
                    .map_or(true, |age| age.as_secs() <= max_age_secs)
            })
    })
}

/// Sidecar metadata Claude Code writes next to agent transcripts
/// (`agent-<id>.meta.json`). `taskKind == "in_process_teammate"` is the
/// authoritative teammate discriminator; `toolUseId` joins a synchronous
/// agent to its spawning `tool_use` in the parent JSONL.
#[derive(Debug, serde::Deserialize)]
struct AgentMeta {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "teamName")]
    team_name: Option<String>,
    #[serde(rename = "taskKind")]
    task_kind: Option<String>,
    #[serde(rename = "toolUseId")]
    tool_use_id: Option<String>,
    #[serde(rename = "stoppedByUser")]
    stopped_by_user: Option<bool>,
}

/// Persistent team state written by Claude Code
/// (`~/.claude/teams/<team>/config.json`). Members accumulate — presence
/// does NOT mean alive — but `joinedAt` is the roster runtime Claude Code
/// itself displays.
#[derive(Debug, serde::Deserialize)]
struct TeamConfig {
    #[serde(default)]
    members: Vec<TeamMember>,
}

#[derive(Debug, serde::Deserialize)]
struct TeamMember {
    name: String,
    #[serde(rename = "joinedAt")]
    joined_at: Option<i64>,
}

fn default_teams_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude").join("teams"))
}

/// A teammate's standing in its team config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeamMembership {
    /// Config readable and the member is present (with `joinedAt` if known).
    /// Presence does NOT mean alive — removals are best-effort.
    Member(Option<chrono::DateTime<chrono::Utc>>),
    /// Config readable but the member is absent — terminated (kill, graceful
    /// shutdown, and spawn-failure rollback all remove the member).
    Absent,
    /// Config missing or unreadable — no signal (session-end cleanup removes
    /// the whole team dir).
    Unknown,
}

fn team_membership(teams_dir: &Path, team: &str, name: &str) -> TeamMembership {
    let Ok(data) = fs::read_to_string(teams_dir.join(team).join("config.json")) else {
        return TeamMembership::Unknown;
    };
    let Ok(config) = serde_json::from_str::<TeamConfig>(&data) else {
        return TeamMembership::Unknown;
    };
    match config.members.into_iter().find(|m| m.name == name) {
        Some(member) => TeamMembership::Member(
            member
                .joined_at
                .and_then(chrono::DateTime::from_timestamp_millis),
        ),
        None => TeamMembership::Absent,
    }
}

fn read_meta(jsonl_path: &Path) -> Option<AgentMeta> {
    let meta_path = jsonl_path.with_extension("meta.json");
    let data = fs::read_to_string(meta_path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Parse an agent transcript. The second value is the timestamp of the last
/// user entry in the tail window — a teammate's current turn start.
fn parse_subagent(path: &Path) -> Option<(SubagentData, Option<chrono::DateTime<chrono::Utc>>)> {
    let file = fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let file_len = metadata.len();
    if file_len == 0 {
        return None;
    }
    // elapsed() errors when mtime is in the future (clock skew) — treat as
    // just written.
    let last_write_age_secs = metadata
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map_or(0, |d| d.as_secs());

    let (task, started_at) = read_subagent_head(&file);
    let runtime_secs = started_at.map(|t| {
        (chrono::Utc::now() - t)
            .num_seconds()
            .max(0)
            .cast_unsigned()
    });

    let meta = read_meta(path);
    let (name, description) = meta.map_or((None, None), |m| (m.name, m.description));
    // The meta description is the curated task title; the first-user-message
    // heuristic is the fallback (it may be the raw prompt).
    let task = description.map_or(task, |d| truncate_str(d.trim(), 60));

    let tail_stats = read_subagent_usage(&file, file_len);

    let state = detect_state_from_tail(&file, file_len);

    Some((
        SubagentData {
            task,
            name,
            model: tail_stats.model,
            context_tokens: tail_stats.context_tokens,
            runtime_secs,
            last_write_age_secs,
            state,
            progress: None,
            phases: Vec::new(),
        },
        tail_stats.last_user_ts,
    ))
}

/// Read the agent's task title and start timestamp from the first lines of
/// its transcript.
fn read_subagent_head(file: &fs::File) -> (String, Option<chrono::DateTime<chrono::Utc>>) {
    let mut file = file;
    if file.seek(SeekFrom::Start(0)).is_err() {
        return (String::new(), None);
    }
    let reader = BufReader::new(&mut file);

    let mut task = String::new();
    let mut started_at = None;
    for line in reader.lines().take(3).map_while(Result::ok) {
        let Some(val) = parse_json_line(&line) else {
            continue;
        };
        if started_at.is_none() {
            started_at = entry_timestamp(&val);
        }
        if !task.is_empty() {
            continue;
        }
        if val.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let text = match val.pointer("/message/content") {
            Some(serde_json::Value::String(s)) => Some(s.as_str()),
            Some(serde_json::Value::Array(arr)) => arr.iter().find_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            }),
            Some(other) => {
                tracing::warn!(
                    content_type = other.to_string(),
                    "unexpected content type in subagent task"
                );
                None
            }
            None => None,
        };
        if let Some(s) = text {
            let trimmed = s.trim().lines().next().unwrap_or("").trim();
            let cleaned = trimmed
                .trim_start_matches("## Task:")
                .trim_start_matches('#')
                .trim_start_matches("Task:")
                .trim();
            task = truncate_str(cleaned, 60);
        }
    }
    (task, started_at)
}

/// Usage stats scanned from a transcript's tail window.
#[derive(Debug, Default)]
struct TailStats {
    model: ModelShort,
    /// Context size from the latest assistant usage (input + cache fields).
    context_tokens: u64,
    /// `output_tokens` of the latest assistant usage.
    last_output_tokens: u64,
    /// Timestamp of the last user entry — a teammate's current turn start.
    last_user_ts: Option<chrono::DateTime<chrono::Utc>>,
}

fn read_subagent_usage(file: &fs::File, file_len: u64) -> TailStats {
    let seek_pos = file_len.saturating_sub(super::RECENT_TAIL_BYTES);
    let Some(reader) = seek_tail(file, seek_pos) else {
        return TailStats::default();
    };

    let mut stats = TailStats::default();

    for line in reader.lines().map_while(Result::ok) {
        let Some(val) = parse_json_line(&line) else {
            continue;
        };
        if is_real_user_turn(&val) {
            stats.last_user_ts = entry_timestamp(&val).or(stats.last_user_ts);
        }
        if !is_assistant_usage(&val) {
            continue;
        }
        if let Some(usage) = val.pointer("/message/usage") {
            let total = extract_tokens(usage);
            if total > 0 {
                stats.context_tokens = total;
                stats.last_output_tokens = usage
                    .get("output_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
            }
        }
        if let Some(m) = val.pointer("/message/model").and_then(|v| v.as_str()) {
            stats.model = parse_model(m);
        }
    }

    stats
}

fn parse_model(model: &str) -> ModelShort {
    if model.contains("fable") {
        ModelShort::Fable
    } else if model.contains("mythos") {
        ModelShort::Mythos
    } else if model.contains("opus") {
        ModelShort::Opus
    } else if model.contains("sonnet") {
        ModelShort::Sonnet
    } else if model.contains("haiku") {
        ModelShort::Haiku
    } else {
        ModelShort::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::sessions::testutil::write_agent_file;

    #[test]
    fn parse_model_fable() {
        assert_eq!(parse_model("claude-fable-5"), ModelShort::Fable);
    }

    #[test]
    fn parse_model_mythos() {
        assert_eq!(parse_model("claude-mythos-5"), ModelShort::Mythos);
    }

    #[test]
    fn parse_model_opus() {
        assert_eq!(parse_model("claude-opus-4-20250514"), ModelShort::Opus);
    }

    #[test]
    fn parse_model_sonnet() {
        assert_eq!(parse_model("claude-sonnet-4-20250514"), ModelShort::Sonnet);
    }

    #[test]
    fn parse_model_haiku() {
        assert_eq!(parse_model("claude-haiku-4-20250514"), ModelShort::Haiku);
    }

    #[test]
    fn parse_model_unknown() {
        assert_eq!(parse_model("gpt-4o-2025"), ModelShort::Unknown);
    }

    #[test]
    fn parse_model_no_dash() {
        assert_eq!(parse_model("custom"), ModelShort::Unknown);
    }

    // ── scan_subagents tests ────────────────────────────────────

    #[test]
    fn scan_subagents_filters_by_active_ids() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(dir.path(), "aaa", "Task A", 1000);
        write_agent_file(dir.path(), "bbb", "Task B", 1000);
        write_agent_file(dir.path(), "ccc", "Task C", 1000);

        let active: HashSet<String> = ["aaa", "ccc"].iter().map(|s| (*s).to_string()).collect();
        let agents = scan_subagents(
            dir.path(),
            &active,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 2);
        let tasks: Vec<&str> = agents.iter().map(|a| a.task.as_str()).collect();
        assert!(tasks.contains(&"Task A"));
        assert!(tasks.contains(&"Task C"));
        assert!(!tasks.contains(&"Task B"));
    }

    #[test]
    fn scan_subagents_empty_active_ids() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(dir.path(), "aaa", "Task A", 1000);

        let active: HashSet<String> = HashSet::new();
        let agents = scan_subagents(
            dir.path(),
            &active,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_subagents_nonexistent_dir() {
        let active = HashSet::from(["aaa".to_string()]);
        let agents = scan_subagents(
            Path::new("/nonexistent/dir/subagents"),
            &active,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_subagents_skips_compact_prefix() {
        let dir = tempfile::tempdir().unwrap();
        // "acompact-xyz.jsonl" should not match strip_prefix("agent-")
        let path = dir.path().join("acompact-xyz.jsonl");
        fs::write(&path, r#"{"type":"user","message":{"content":"hi"}}"#).unwrap();

        let active = HashSet::from(["xyz".to_string()]);
        let agents = scan_subagents(
            dir.path(),
            &active,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    fn set_mtime_ago(path: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(t)).unwrap();
    }

    /// Create a workflow run dir with a journal (2 started, 1 finished) and
    /// two agent transcripts.
    fn write_workflow_run(subagents_dir: &Path, run_id: &str) -> PathBuf {
        let run_dir = subagents_dir.join("workflows").join(run_id);
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("journal.jsonl"),
            concat!(
                "{\"type\":\"started\",\"key\":\"v2:k1\",\"agentId\":\"wf1\"}\n",
                "{\"type\":\"result\",\"key\":\"v2:k1\",\"agentId\":\"wf1\",\"result\":\"ok\"}\n",
                "{\"type\":\"started\",\"key\":\"v2:k2\",\"agentId\":\"wf2\"}\n",
            ),
        )
        .unwrap();
        write_agent_file(&run_dir, "wf1", "First workflow agent", 2_000);
        write_agent_file(&run_dir, "wf2", "Second workflow agent", 3_000);
        run_dir
    }

    #[test]
    fn scan_subagents_aggregates_workflow_run() {
        let dir = tempfile::tempdir().unwrap();
        write_workflow_run(dir.path(), "wf_abc123");

        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        let run = &agents[0];
        assert_eq!(run.progress, Some((1, 2)));
        // Falls back to the run id when no script maps the name
        assert_eq!(run.name.as_deref(), Some("wf_abc123"));
        // Tokens are summed across the run's agents
        assert_eq!(run.context_tokens, 5_000);
        assert_eq!(run.state, SessionState::Working);
    }

    #[test]
    fn scan_subagents_workflow_run_named_from_script() {
        let root = tempfile::tempdir().unwrap();
        let subagents_dir = root.path().join("subagents");
        write_workflow_run(&subagents_dir, "wf_abc123");
        let scripts = root.path().join("workflows").join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("my-flow-wf_abc123.js"), "export const meta").unwrap();

        let agents = scan_subagents(
            &subagents_dir,
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name.as_deref(), Some("my-flow"));
    }

    #[test]
    fn scan_subagents_workflow_run_uses_launch_event_name_over_run_id() {
        let dir = tempfile::tempdir().unwrap();
        write_workflow_run(dir.path(), "wf_abc123");
        // No completion snapshot, no persisted script — only the launch
        // event's name is available. Should NOT fall back to the raw run id.
        let workflow_names: HashMap<String, String> =
            [("wf_abc123".to_string(), "review-changes".to_string())].into();

        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &workflow_names,
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name.as_deref(), Some("review-changes"));
    }

    #[test]
    fn scan_subagents_workflow_run_launch_event_wins_over_script_slug() {
        // The launch event's `workflowName` is a direct structured field
        // parsed straight from the parent JSONL — more authoritative than
        // reverse-engineering a name from the persisted script's filename
        // (which can race or fail to match, the exact gap this fixes).
        let root = tempfile::tempdir().unwrap();
        let subagents_dir = root.path().join("subagents");
        write_workflow_run(&subagents_dir, "wf_abc123");
        let scripts = root.path().join("workflows").join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("my-flow-wf_abc123.js"), "export const meta").unwrap();
        let workflow_names: HashMap<String, String> =
            [("wf_abc123".to_string(), "review-changes".to_string())].into();

        let agents = scan_subagents(
            &subagents_dir,
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &workflow_names,
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name.as_deref(), Some("review-changes"));
    }

    #[test]
    fn scan_subagents_merges_direct_agents_and_workflow_runs() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(dir.path(), "direct1", "Direct agent", 1_000);
        write_workflow_run(dir.path(), "wf_abc123");

        let active = HashSet::from(["direct1".to_string()]);
        let agents = scan_subagents(
            dir.path(),
            &active,
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 2);
        assert_eq!(
            agents.iter().filter(|a| a.progress.is_some()).count(),
            1,
            "exactly one aggregated workflow row"
        );
    }

    #[test]
    fn scan_subagents_excludes_stale_workflow_runs() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = write_workflow_run(dir.path(), "wf_dead");
        for entry in fs::read_dir(&run_dir).unwrap().flatten() {
            set_mtime_ago(&entry.path(), AGENT_STALE_SECS + 60);
        }

        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_subagents_finished_workflow_run_lingers_then_hides() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = write_workflow_run(dir.path(), "wf_done");
        // Balance the journal: both agents finished
        fs::write(
            run_dir.join("journal.jsonl"),
            concat!(
                "{\"type\":\"started\",\"key\":\"v2:k1\",\"agentId\":\"wf1\"}\n",
                "{\"type\":\"result\",\"key\":\"v2:k1\",\"agentId\":\"wf1\",\"result\":\"ok\"}\n",
                "{\"type\":\"started\",\"key\":\"v2:k2\",\"agentId\":\"wf2\"}\n",
                "{\"type\":\"result\",\"key\":\"v2:k2\",\"agentId\":\"wf2\",\"result\":\"ok\"}\n",
            ),
        )
        .unwrap();

        // Fresh: lingers with full progress and Idle state
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].progress, Some((2, 2)));
        assert_eq!(agents[0].state, SessionState::Idle);

        // Quiet past the linger window: hidden
        for entry in fs::read_dir(&run_dir).unwrap().flatten() {
            set_mtime_ago(&entry.path(), RUN_DONE_LINGER_SECS + 60);
        }
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    // ── workflow phases ──────────────────────────────────────────

    #[test]
    fn derive_phases_groups_agents_by_phase_title_in_declaration_order() {
        let entries: Vec<ProgressEntry> = serde_json::from_str(
            r#"[
                {"type":"workflow_phase","index":1,"title":"Find"},
                {"type":"workflow_phase","index":2,"title":"Verify"},
                {"type":"workflow_agent","index":1,"phaseTitle":"Find","state":"done"},
                {"type":"workflow_agent","index":2,"phaseTitle":"Find","state":"done"},
                {"type":"workflow_agent","index":3,"phaseTitle":"Verify","state":"running"}
            ]"#,
        )
        .unwrap();
        let phases = derive_phases(&entries);
        assert_eq!(
            phases,
            vec![
                PhaseProgress {
                    title: "Find".into(),
                    done: 2,
                    total: 2,
                    current_tool: None,
                },
                PhaseProgress {
                    title: "Verify".into(),
                    done: 0,
                    total: 1,
                    current_tool: None,
                },
            ]
        );
    }

    #[test]
    fn derive_phases_current_tool_from_running_agent() {
        let entries: Vec<ProgressEntry> = serde_json::from_str(
            r#"[
                {"type":"workflow_phase","index":1,"title":"Build"},
                {"type":"workflow_agent","index":1,"phaseTitle":"Build","state":"running","lastToolName":"Bash","lastToolSummary":"cargo test --workspace"}
            ]"#,
        )
        .unwrap();
        let phases = derive_phases(&entries);
        assert_eq!(
            phases[0].current_tool.as_deref(),
            Some("cargo test --workspace")
        );
    }

    #[test]
    fn derive_phases_current_tool_falls_back_to_tool_name() {
        let entries: Vec<ProgressEntry> = serde_json::from_str(
            r#"[
                {"type":"workflow_phase","index":1,"title":"Build"},
                {"type":"workflow_agent","index":1,"phaseTitle":"Build","state":"running","lastToolName":"Bash"}
            ]"#,
        )
        .unwrap();
        let phases = derive_phases(&entries);
        assert_eq!(phases[0].current_tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn derive_phases_no_current_tool_when_agent_done() {
        let entries: Vec<ProgressEntry> = serde_json::from_str(
            r#"[
                {"type":"workflow_phase","index":1,"title":"Build"},
                {"type":"workflow_agent","index":1,"phaseTitle":"Build","state":"done","lastToolName":"Bash","lastToolSummary":"cargo test"}
            ]"#,
        )
        .unwrap();
        let phases = derive_phases(&entries);
        assert!(phases[0].current_tool.is_none());
    }

    #[test]
    fn derive_phases_ignores_agents_with_no_declared_phase() {
        // No `workflow_phase` entries at all — a run whose script declares
        // no `meta.phases`. Agents carry no `phaseTitle` to match against.
        let entries: Vec<ProgressEntry> =
            serde_json::from_str(r#"[{"type":"workflow_agent","index":1,"state":"done"}]"#)
                .unwrap();
        assert!(derive_phases(&entries).is_empty());
    }

    #[test]
    fn find_task_output_locates_file_under_claude_prefixed_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_root = tmp.path().join("claude-1000");
        let task_dir = claude_root.join("my-proj").join("sess-1").join("tasks");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("w1.output"), "{}").unwrap();

        let found = find_task_output(tmp.path(), "my-proj", "sess-1", "w1");
        assert_eq!(found, Some(task_dir.join("w1.output")));
    }

    #[test]
    fn find_task_output_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("claude-1000")).unwrap();
        assert_eq!(
            find_task_output(tmp.path(), "my-proj", "sess-1", "w1"),
            None
        );
    }

    #[test]
    fn scan_subagents_workflow_run_phases_from_terminal_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let subagents_dir = root.path().join("subagents");
        let run_dir = write_workflow_run(&subagents_dir, "wf_abc123");
        // Balance the journal so the run is terminal.
        fs::write(
            run_dir.join("journal.jsonl"),
            concat!(
                "{\"type\":\"started\",\"key\":\"v2:k1\",\"agentId\":\"wf1\"}\n",
                "{\"type\":\"result\",\"key\":\"v2:k1\",\"agentId\":\"wf1\",\"result\":\"ok\"}\n",
                "{\"type\":\"started\",\"key\":\"v2:k2\",\"agentId\":\"wf2\"}\n",
                "{\"type\":\"result\",\"key\":\"v2:k2\",\"agentId\":\"wf2\",\"result\":\"ok\"}\n",
            ),
        )
        .unwrap();
        let workflows_dir = root.path().join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        fs::write(
            workflows_dir.join("wf_abc123.json"),
            r#"{
                "status": "completed",
                "workflowName": "review-changes",
                "totalTokens": 5000,
                "workflowProgress": [
                    {"type":"workflow_phase","index":1,"title":"Find"},
                    {"type":"workflow_phase","index":2,"title":"Verify"},
                    {"type":"workflow_agent","index":1,"phaseTitle":"Find","state":"done"},
                    {"type":"workflow_agent","index":2,"phaseTitle":"Verify","state":"done"}
                ]
            }"#,
        )
        .unwrap();

        let agents = scan_subagents(
            &subagents_dir,
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(
            agents[0].phases,
            vec![
                PhaseProgress {
                    title: "Find".into(),
                    done: 1,
                    total: 1,
                    current_tool: None,
                },
                PhaseProgress {
                    title: "Verify".into(),
                    done: 1,
                    total: 1,
                    current_tool: None,
                },
            ]
        );
    }

    #[test]
    fn scan_subagents_workflow_run_phases_from_live_output() {
        // No completion snapshot — the run is still in progress (write_workflow_run's
        // journal has wf1 done, wf2 only started). Phase data must come from the
        // live task-output file under /tmp/claude-*/<proj>/<sessionId>/tasks/<taskId>.output.
        let root = tempfile::tempdir().unwrap();
        let subagents_dir = root
            .path()
            .join("live-proj")
            .join("live-session")
            .join("subagents");
        write_workflow_run(&subagents_dir, "wf_abc123");

        let claude_tmp = tempfile::Builder::new()
            .prefix("claude-")
            .tempdir()
            .unwrap();
        let task_dir = claude_tmp
            .path()
            .join("live-proj")
            .join("live-session")
            .join("tasks");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(
            task_dir.join("w-live-task.output"),
            r#"{
                "workflowProgress": [
                    {"type":"workflow_phase","index":1,"title":"Design"},
                    {"type":"workflow_phase","index":2,"title":"Review"},
                    {"type":"workflow_agent","index":1,"phaseTitle":"Design","state":"done"},
                    {"type":"workflow_agent","index":2,"phaseTitle":"Review","state":"running"}
                ]
            }"#,
        )
        .unwrap();
        let workflow_task_ids: HashMap<String, String> =
            [("wf_abc123".to_string(), "w-live-task".to_string())].into();

        let agents = scan_subagents(
            &subagents_dir,
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &workflow_task_ids,
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state, SessionState::Working);
        assert_eq!(
            agents[0].phases,
            vec![
                PhaseProgress {
                    title: "Design".into(),
                    done: 1,
                    total: 1,
                    current_tool: None,
                },
                PhaseProgress {
                    title: "Review".into(),
                    done: 0,
                    total: 1,
                    current_tool: None,
                },
            ]
        );
    }

    #[test]
    fn scan_subagents_live_workflow_without_task_id_has_no_phases() {
        // No entry in workflow_task_ids for this run_id at all — the launch
        // event predates `taskId` or wasn't observed yet.
        let dir = tempfile::tempdir().unwrap();
        write_workflow_run(dir.path(), "wf_abc123");

        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert!(agents[0].phases.is_empty());
    }

    // ── teammates ────────────────────────────────────────────────

    fn spawned_status() -> TeammateStatus {
        TeammateStatus {
            spawned_at: Some(chrono::Utc::now()),
            last_idle_at: None,
            terminated: false,
        }
    }

    fn idle_status() -> TeammateStatus {
        let now = chrono::Utc::now();
        TeammateStatus {
            spawned_at: Some(now - chrono::TimeDelta::seconds(600)),
            last_idle_at: Some(now),
            terminated: false,
        }
    }

    #[test]
    fn scan_subagents_finds_named_teammate() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(
            dir.path(),
            "amy-fixer-782b353b34c66890",
            "Fix things",
            3_000,
        );

        let teammates = HashMap::from([("my-fixer".to_string(), spawned_status())]);
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &teammates,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name.as_deref(), Some("my-fixer"));
    }

    #[test]
    fn scan_subagents_idle_teammate_fresh_shows_idle() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(
            dir.path(),
            "amy-fixer-782b353b34c66890",
            "Fix things",
            3_000,
        );

        let teammates = HashMap::from([("my-fixer".to_string(), idle_status())]);
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &teammates,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state, SessionState::Idle);
    }

    #[test]
    fn scan_subagents_idle_teammate_resumed_shows_working() {
        // The idle_notification latch (`status.is_idle()`) can be stale:
        // delivery into the parent JSONL only happens once the lead itself
        // goes idle, so a teammate that resumed via a new mailbox message
        // and is actively mid-turn right now can still carry a
        // `is_idle()==true` status. The teammate's OWN transcript tail is
        // the fresher signal and must win — not get forcibly overwritten.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-amy-fixer-782b353b34c66890.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"user\",\"message\":{\"content\":\"new task\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}],\"stop_reason\":\"tool_use\"}}\n",
            ),
        )
        .unwrap();

        let teammates = HashMap::from([("my-fixer".to_string(), idle_status())]);
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &teammates,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state, SessionState::Working);
    }

    #[test]
    fn scan_subagents_idle_teammate_never_evicted_for_staleness() {
        // Real Claude Code never evicts idle teammates from its roster —
        // only terminal rows linger-then-disappear. An idle teammate whose
        // transcript has gone quiet for a long time must still show, not
        // vanish, as long as it's genuinely idle (not stale-and-abandoned:
        // AGENT_STALE_SECS still caps the never-went-idle case elsewhere).
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(
            dir.path(),
            "amy-fixer-782b353b34c66890",
            "Fix things",
            3_000,
        );
        set_mtime_ago(
            &dir.path().join("agent-amy-fixer-782b353b34c66890.jsonl"),
            3_600,
        );

        let teammates = HashMap::from([("my-fixer".to_string(), idle_status())]);
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &teammates,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state, SessionState::Idle);
    }

    #[test]
    fn scan_subagents_abandoned_teammate_hidden() {
        // Spawned, never went idle, transcript quiet for far longer than any
        // model turn — a zombie left by a killed session turn.
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(
            dir.path(),
            "amy-fixer-782b353b34c66890",
            "Fix things",
            3_000,
        );
        set_mtime_ago(
            &dir.path().join("agent-amy-fixer-782b353b34c66890.jsonl"),
            AGENT_STALE_SECS + 60,
        );

        let teammates = HashMap::from([("my-fixer".to_string(), spawned_status())]);
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &teammates,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_subagents_respawned_teammate_dedups_to_newest() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(
            dir.path(),
            "amy-fixer-782b353b34c66890",
            "First spawn",
            1_000,
        );
        write_agent_file(
            dir.path(),
            "amy-fixer-99aa353b34c66890",
            "Second spawn",
            2_000,
        );
        set_mtime_ago(
            &dir.path().join("agent-amy-fixer-782b353b34c66890.jsonl"),
            300,
        );

        let teammates = HashMap::from([("my-fixer".to_string(), spawned_status())]);
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &teammates,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].task, "Second spawn");
    }

    #[test]
    fn scan_subagents_teammate_meta_name_and_description_win() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(
            dir.path(),
            "amy-fixer-782b353b34c66890",
            "Raw prompt",
            1_000,
        );
        fs::write(
            dir.path().join("agent-amy-fixer-782b353b34c66890.meta.json"),
            r#"{"agentType":"my-fixer","description":"Curated task title","name":"my-fixer","taskKind":"in_process_teammate","model":"claude-opus-4-8"}"#,
        )
        .unwrap();

        let teammates = HashMap::from([("my-fixer".to_string(), spawned_status())]);
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &teammates,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name.as_deref(), Some("my-fixer"));
        assert_eq!(agents[0].task, "Curated task title");
    }

    // ── synchronous agents (meta toolUseId join) ─────────────────

    #[test]
    fn scan_subagents_sync_agent_active_until_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(dir.path(), "sync1", "Sync agent", 1_000);
        fs::write(
            dir.path().join("agent-sync1.meta.json"),
            r#"{"agentType":"general-purpose","toolUseId":"toolu_123"}"#,
        )
        .unwrap();

        // No tool_result yet → visible
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].task, "Sync agent");

        // tool_result arrived → finished, but lingers briefly first (same
        // window a workflow run gets) instead of vanishing immediately.
        let completed = HashSet::from(["toolu_123".to_string()]);
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &completed,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);

        // Quiet past the linger window: hidden.
        set_mtime_ago(
            &dir.path().join("agent-sync1.jsonl"),
            RUN_DONE_LINGER_SECS + 1,
        );
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &completed,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_subagents_sync_agent_stopped_by_user_hidden() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(dir.path(), "sync1", "Sync agent", 1_000);
        fs::write(
            dir.path().join("agent-sync1.meta.json"),
            r#"{"agentType":"general-purpose","toolUseId":"toolu_123","stoppedByUser":true}"#,
        )
        .unwrap();

        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_subagents_terminated_teammate_lingers_then_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("agent-amy-fixer-782b353b34c66890.jsonl");
        write_agent_file(
            dir.path(),
            "amy-fixer-782b353b34c66890",
            "Fix things",
            3_000,
        );

        let status = TeammateStatus {
            spawned_at: Some(chrono::Utc::now()),
            last_idle_at: None,
            terminated: true,
        };
        let teammates = HashMap::from([("my-fixer".to_string(), status)]);

        // Fresh: lingers, same as a just-finished workflow run.
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &teammates,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);

        // Quiet past the linger window: hidden.
        set_mtime_ago(&transcript_path, RUN_DONE_LINGER_SECS + 1);
        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &teammates,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    // ── workflow snapshots and respawn counting ──────────────────

    #[test]
    fn workflow_run_counts_by_key_across_respawns() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("workflows").join("wf_retry");
        fs::create_dir_all(&run_dir).unwrap();
        // The same cache key started twice (respawn) then finished once
        fs::write(
            run_dir.join("journal.jsonl"),
            concat!(
                "{\"type\":\"started\",\"key\":\"v2:k1\",\"agentId\":\"a1\"}\n",
                "{\"type\":\"started\",\"key\":\"v2:k1\",\"agentId\":\"a2\"}\n",
                "{\"type\":\"result\",\"key\":\"v2:k1\",\"agentId\":\"a2\",\"result\":\"ok\"}\n",
            ),
        )
        .unwrap();
        write_agent_file(&run_dir, "a2", "Retried agent", 1_000);

        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].progress, Some((1, 1)));
    }

    #[test]
    fn workflow_run_snapshot_is_authoritative() {
        let root = tempfile::tempdir().unwrap();
        let subagents_dir = root.path().join("subagents");
        let run_dir = subagents_dir.join("workflows").join("wf_failed");
        fs::create_dir_all(&run_dir).unwrap();
        // Dangling started (a failed agent never writes a result line)
        fs::write(
            run_dir.join("journal.jsonl"),
            "{\"type\":\"started\",\"key\":\"v2:k1\",\"agentId\":\"a1\"}\n",
        )
        .unwrap();
        write_agent_file(&run_dir, "a1", "Doomed agent", 1_000);
        // Completion snapshot marks the run terminal
        let snapshots = root.path().join("workflows");
        fs::create_dir_all(&snapshots).unwrap();
        fs::write(
            snapshots.join("wf_failed.json"),
            r#"{"status":"failed","workflowName":"my-flow","totalTokens":42}"#,
        )
        .unwrap();

        // A stale launch-event name should lose to the completion snapshot.
        let workflow_names: HashMap<String, String> =
            [("wf_failed".to_string(), "stale-launch-name".to_string())].into();
        let agents = scan_subagents(
            &subagents_dir,
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &workflow_names,
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        let run = &agents[0];
        assert_eq!(run.name.as_deref(), Some("my-flow"));
        assert_eq!(run.context_tokens, 42);
        assert_eq!(run.state, SessionState::Idle);
        assert_eq!(run.task, "failed");

        // Once the snapshot ages past the linger window the run hides
        set_mtime_ago(&snapshots.join("wf_failed.json"), RUN_DONE_LINGER_SECS + 31);
        let agents = scan_subagents(
            &subagents_dir,
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn workflow_run_freezes_runtime_at_snapshot_duration() {
        // The agent transcript started long ago — if runtime were still
        // computed live (now - started_at) it would show a huge number
        // instead of the run's actual, already-finished duration.
        let root = tempfile::tempdir().unwrap();
        let subagents_dir = root.path().join("subagents");
        let run_dir = subagents_dir.join("workflows").join("wf_done");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("journal.jsonl"),
            concat!(
                "{\"type\":\"started\",\"key\":\"v2:k1\",\"agentId\":\"a1\"}\n",
                "{\"type\":\"result\",\"key\":\"v2:k1\",\"agentId\":\"a1\",\"result\":\"ok\"}\n",
            ),
        )
        .unwrap();
        fs::write(
            run_dir.join("agent-a1.jsonl"),
            "{\"type\":\"user\",\"timestamp\":\"2020-01-01T00:00:00.000Z\",\"message\":{\"content\":\"go\"}}\n",
        )
        .unwrap();
        let snapshots = root.path().join("workflows");
        fs::create_dir_all(&snapshots).unwrap();
        fs::write(
            snapshots.join("wf_done.json"),
            r#"{"status":"completed","workflowName":"my-flow","totalTokens":42,"durationMs":125000}"#,
        )
        .unwrap();

        let agents = scan_subagents(
            &subagents_dir,
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].runtime_secs, Some(125));
    }

    // ── parse_teammate membership + runtime ──────────────────────

    /// Meta + team config fixture: a teammate transcript whose meta names a
    /// team rooted in the same tempdir.
    fn write_teammate_with_team(dir: &Path, member_json: &str) -> (PathBuf, PathBuf) {
        write_agent_file(dir, "amy-fixer-782b353b34c66890", "Fix things", 3_000);
        let transcript = dir.join("agent-amy-fixer-782b353b34c66890.jsonl");
        fs::write(
            dir.join("agent-amy-fixer-782b353b34c66890.meta.json"),
            r#"{"name":"my-fixer","taskKind":"in_process_teammate","teamName":"session-abc"}"#,
        )
        .unwrap();
        let teams_dir = dir.join("teams");
        let team_root = teams_dir.join("session-abc");
        fs::create_dir_all(&team_root).unwrap();
        fs::write(
            team_root.join("config.json"),
            format!(r#"{{"name":"session-abc","members":[{member_json}]}}"#),
        )
        .unwrap();
        (transcript, teams_dir)
    }

    #[test]
    fn parse_teammate_removed_member_lingers_then_hidden() {
        let dir = tempfile::tempdir().unwrap();
        // Config readable but our teammate is not a member — terminated.
        let (transcript, teams_dir) = write_teammate_with_team(
            dir.path(),
            r#"{"agentId":"other@session-abc","name":"other","joinedAt":1783569232384}"#,
        );

        // Fresh transcript: still shows, same as a just-finished workflow run.
        let result = parse_teammate(&transcript, "my-fixer", &spawned_status(), Some(&teams_dir));
        assert!(result.is_some());

        // Quiet past the linger window: hidden.
        set_mtime_ago(&transcript, RUN_DONE_LINGER_SECS + 1);
        let result = parse_teammate(&transcript, "my-fixer", &spawned_status(), Some(&teams_dir));
        assert!(result.is_none());
    }

    #[test]
    fn parse_teammate_runtime_from_joined_at() {
        let dir = tempfile::tempdir().unwrap();
        let joined = chrono::Utc::now() - chrono::TimeDelta::seconds(90);
        let member = format!(
            r#"{{"agentId":"my-fixer@session-abc","name":"my-fixer","joinedAt":{}}}"#,
            joined.timestamp_millis()
        );
        let (transcript, teams_dir) = write_teammate_with_team(dir.path(), &member);

        let data = parse_teammate(&transcript, "my-fixer", &spawned_status(), Some(&teams_dir))
            .expect("member teammate should render");
        let runtime = data.runtime_secs.expect("runtime from joinedAt");
        assert!((85..=95).contains(&runtime), "got {runtime}");
    }

    #[test]
    fn parse_teammate_turn_runtime_from_last_user_entry() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let joined = chrono::Utc::now() - chrono::TimeDelta::seconds(600);
        let member = format!(
            r#"{{"agentId":"my-fixer@session-abc","name":"my-fixer","joinedAt":{}}}"#,
            joined.timestamp_millis()
        );
        let (transcript, teams_dir) = write_teammate_with_team(dir.path(), &member);
        // A mailbox message 45s ago started a new turn
        let turn_start = chrono::Utc::now() - chrono::TimeDelta::seconds(45);
        let mut f = fs::File::options().append(true).open(&transcript).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"{}","message":{{"content":"next task"}}}}"#,
            turn_start.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        )
        .unwrap();
        f.sync_all().unwrap();

        let data = parse_teammate(&transcript, "my-fixer", &spawned_status(), Some(&teams_dir))
            .expect("member teammate should render");
        let runtime = data.runtime_secs.expect("turn runtime");
        assert!((40..=50).contains(&runtime), "got {runtime}");
    }

    // ── team_membership ──────────────────────────────────────────

    #[test]
    fn team_membership_member_absent_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let team_dir = dir.path().join("session-abc");
        fs::create_dir_all(&team_dir).unwrap();
        fs::write(
            team_dir.join("config.json"),
            r#"{"name":"session-abc","members":[
                {"agentId":"my-fixer@session-abc","name":"my-fixer","joinedAt":1783569232384}
            ]}"#,
        )
        .unwrap();

        match team_membership(dir.path(), "session-abc", "my-fixer") {
            TeamMembership::Member(Some(joined)) => {
                assert_eq!(joined.timestamp_millis(), 1_783_569_232_384);
            }
            other => panic!("expected Member(Some(_)), got {other:?}"),
        }
        // A member missing from a readable config was killed
        assert_eq!(
            team_membership(dir.path(), "session-abc", "killed-one"),
            TeamMembership::Absent
        );
        // A missing config is no signal at all
        assert_eq!(
            team_membership(dir.path(), "no-such-team", "my-fixer"),
            TeamMembership::Unknown
        );
    }

    // ── teammate_name_of ─────────────────────────────────────────

    #[test]
    fn teammate_name_extracted_from_id() {
        assert_eq!(
            teammate_name_of("afix-load-docrefs-fb64396d7b46c88f"),
            Some("fix-load-docrefs")
        );
    }

    #[test]
    fn teammate_name_none_for_anonymous_id() {
        assert_eq!(teammate_name_of("a2a043d116fbd87f0"), None);
    }

    #[test]
    fn teammate_name_none_for_non_hex_suffix() {
        assert_eq!(teammate_name_of("amy-fixer-notahash!"), None);
        assert_eq!(teammate_name_of("amy-fixer-"), None);
    }

    #[test]
    fn teammate_name_splits_at_last_dash() {
        // Name segments containing dashes stay intact
        assert_eq!(
            teammate_name_of("aw4-dedup-census-1161ac98aa13d730"),
            Some("w4-dedup-census")
        );
    }

    // ── workflow run helpers ─────────────────────────────────────

    #[test]
    fn workflow_run_missing_journal_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("workflows").join("wf_nojournal");
        fs::create_dir_all(&run_dir).unwrap();
        write_agent_file(&run_dir, "wf1", "No journal here", 1_000);

        let agents = scan_subagents(
            dir.path(),
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(agents.is_empty());
    }

    #[test]
    fn workflow_run_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_workflow_runs(dir.path(), &HashMap::new(), &HashMap::new()).is_empty());
    }

    // ── read_subagent_task prefix stripping ─────────────────────

    /// Write a subagent file with custom first-line content and return the
    /// parsed task string.
    fn task_from_content(content: &str) -> String {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-test.jsonl");
        let user_line = format!(
            r#"{{"type":"user","message":{{"content":{}}}}}"#,
            serde_json::json!(content),
        );
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{user_line}").unwrap();
        f.sync_all().unwrap();

        let file = fs::File::open(&path).unwrap();
        read_subagent_head(&file).0
    }

    #[test]
    fn subagent_task_strips_hash_prefix() {
        assert_eq!(
            task_from_content("# Search the codebase"),
            "Search the codebase"
        );
    }

    #[test]
    fn subagent_task_strips_task_prefix() {
        assert_eq!(task_from_content("Task: Run the linter"), "Run the linter");
    }

    #[test]
    fn subagent_task_strips_heading_task_prefix() {
        assert_eq!(
            task_from_content("## Task: Investigate flaky test"),
            "Investigate flaky test"
        );
    }

    #[test]
    fn subagent_task_no_prefix() {
        assert_eq!(
            task_from_content("Plain task description"),
            "Plain task description"
        );
    }

    #[test]
    fn subagent_task_truncates_long_text() {
        let long = "A".repeat(100);
        let result = task_from_content(&long);
        assert!(result.len() <= 63); // 60 chars + "..." ellipsis
    }

    #[test]
    fn subagent_task_uses_first_line_only() {
        assert_eq!(
            task_from_content("First line\nSecond line\nThird line"),
            "First line"
        );
    }

    /// Write a subagent file with array-format content and return the parsed
    /// task string.
    fn task_from_array_content(text: &str) -> String {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-test.jsonl");
        let user_line = format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"text","text":{}}}]}}}}"#,
            serde_json::json!(text),
        );
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{user_line}").unwrap();
        f.sync_all().unwrap();

        let file = fs::File::open(&path).unwrap();
        read_subagent_head(&file).0
    }

    #[test]
    fn subagent_task_array_content() {
        assert_eq!(
            task_from_array_content("Search the codebase"),
            "Search the codebase"
        );
    }

    #[test]
    fn subagent_task_array_content_with_prefix() {
        assert_eq!(task_from_array_content("## Task: Run tests"), "Run tests");
    }

    #[test]
    fn subagent_task_array_content_multiline() {
        assert_eq!(
            task_from_array_content("First line\nSecond line"),
            "First line"
        );
    }

    // ── read_subagent_usage via parse_subagent ─────────────────────

    /// Write a subagent file with known model and token counts, then verify
    /// `parse_subagent` extracts them correctly.
    fn write_usage_agent(dir: &Path, model: &str, input_tokens: u64) -> PathBuf {
        use std::io::Write;
        let path = dir.join("agent-usage_test.jsonl");
        let user_line = r#"{"type":"user","message":{"content":"test task"}}"#;
        let usage = format!(
            r#"{{"type":"assistant","message":{{"model":"{model}","usage":{{"input_tokens":{input_tokens}}},"content":[{{"type":"text","text":"ok"}}],"stop_reason":"end_turn"}}}}"#
        );
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{user_line}").unwrap();
        writeln!(f, "{usage}").unwrap();
        f.sync_all().unwrap();
        path
    }

    #[test]
    fn parse_subagent_extracts_model_and_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_usage_agent(dir.path(), "claude-sonnet-4-20250514", 12_345);
        let (agent, _) = parse_subagent(&path).unwrap();
        assert_eq!(agent.model, ModelShort::Sonnet);
        assert_eq!(agent.context_tokens, 12_345);
    }

    #[test]
    fn parse_subagent_zero_tokens_not_overwritten() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-zero_test.jsonl");
        let user_line = r#"{"type":"user","message":{"content":"test"}}"#;
        // First usage with positive tokens, then one with zero
        let usage1 = r#"{"type":"assistant","message":{"model":"claude-opus-4-20250514","usage":{"input_tokens":5000},"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#;
        let usage2 = r#"{"type":"assistant","message":{"model":"claude-opus-4-20250514","usage":{"input_tokens":0},"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#;
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{user_line}").unwrap();
        writeln!(f, "{usage1}").unwrap();
        writeln!(f, "{usage2}").unwrap();
        f.sync_all().unwrap();

        let (agent, _) = parse_subagent(&path).unwrap();
        // Zero-token usage should not overwrite the previous positive value
        assert_eq!(agent.context_tokens, 5000);
        assert_eq!(agent.model, ModelShort::Opus);
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_parse_model_never_panics(s in "\\PC*") {
                let _model = parse_model(&s);
            }

            #[test]
            fn prop_parse_model_contains_match(
                // Use digits + dashes to avoid accidentally embedding another model name
                prefix in "[0-9\\-]{0,10}",
                suffix in "[0-9\\-]{0,10}",
            ) {
                let model_fable = format!("{prefix}fable{suffix}");
                assert_eq!(parse_model(&model_fable), ModelShort::Fable);

                let model_mythos = format!("{prefix}mythos{suffix}");
                assert_eq!(parse_model(&model_mythos), ModelShort::Mythos);

                let model_opus = format!("{prefix}opus{suffix}");
                assert_eq!(parse_model(&model_opus), ModelShort::Opus);

                let model_sonnet = format!("{prefix}sonnet{suffix}");
                assert_eq!(parse_model(&model_sonnet), ModelShort::Sonnet);

                let model_haiku = format!("{prefix}haiku{suffix}");
                assert_eq!(parse_model(&model_haiku), ModelShort::Haiku);
            }
        }
    }
}
