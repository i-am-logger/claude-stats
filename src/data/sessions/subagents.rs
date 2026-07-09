#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use super::activity::detect_state_from_tail;
use super::tail::{
    entry_timestamp, extract_tokens, is_assistant_usage, parse_json_line, seek_tail, TeammateStatus,
};
use super::{ModelShort, SessionState, SubagentData};
use crate::fmt::truncate_str;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

/// Hide workflow-run agents whose transcript hasn't been written for this
/// long. A crashed or killed run never writes its `result` journal lines, so
/// journal orphans alone would render forever. Live agents write at least
/// every ~60s; a single long model turn can stay silent for ~15min; dead runs
/// observed in practice are stale by many hours.
pub(crate) const AGENT_STALE_SECS: u64 = 1800;

/// Hide idle teammates once their transcript has been quiet this long. An
/// idle teammate is "available" (it can be resumed via its mailbox), so it
/// stays on the roster briefly, then drops off.
pub(crate) const TEAMMATE_IDLE_HIDE_SECS: u64 = 900;

/// How long a finished workflow run ("N/N agents done") lingers on the
/// roster after its last write before disappearing.
pub(crate) const RUN_DONE_LINGER_SECS: u64 = 120;

/// Scan subagent JSONL files and return data for agents that are still active.
///
/// `active_ids` is the set of Task-tool agent IDs (e.g.
/// `"a2a043d116fbd87f0"`) that parent-JSONL tracking shows as still
/// in-progress. `teammates` maps named-teammate lifecycle statuses observed
/// in the parent JSONL; their transcripts are matched by the name embedded
/// in the filename (`agent-a<name>-<hash>.jsonl`). Workflow runs under
/// `subagents/workflows/<run-id>/` aggregate into a single row each.
#[allow(
    clippy::implicit_hasher,
    reason = "internal API, always called with std collections"
)]
pub fn scan_subagents(
    subagents_dir: &Path,
    active_ids: &HashSet<String>,
    teammates: &HashMap<String, TeammateStatus>,
) -> Vec<SubagentData> {
    let mut agents = if active_ids.is_empty() && teammates.is_empty() {
        Vec::new()
    } else {
        collect_agents(subagents_dir, active_ids, teammates)
    };

    agents.extend(scan_workflow_runs(subagents_dir));

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
    teammates: &HashMap<String, TeammateStatus>,
) -> Vec<SubagentData> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

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

            // Task-tool agents confirmed active by id
            if active_ids.contains(agent_id) {
                return parse_subagent(&path);
            }

            // Named teammates: the transcript id embeds the name
            let tm_name = teammate_name_of(agent_id)?;
            let status = teammates.get(tm_name)?;
            parse_teammate(&path, tm_name, status)
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

fn parse_teammate(path: &Path, name: &str, status: &TeammateStatus) -> Option<SubagentData> {
    let mut data = parse_subagent(path)?;
    if status.is_idle() {
        // Idle means "available", not terminated — keep the roster row while
        // the transcript is fresh, drop it once it goes quiet.
        if data.last_write_age_secs > TEAMMATE_IDLE_HIDE_SECS {
            return None;
        }
        data.state = SessionState::Idle;
    } else if data.last_write_age_secs > AGENT_STALE_SECS {
        // Spawned but never sent an idle notification, and the transcript has
        // been quiet far longer than any model turn — abandoned or killed.
        return None;
    }
    // The meta.json name wins, but the tracker name fills any gap.
    data.name.get_or_insert_with(|| name.to_string());
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
fn scan_workflow_runs(subagents_dir: &Path) -> Vec<SubagentData> {
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
            parse_workflow_run(&run_dir, scripts_dir.as_deref())
        })
        .collect()
}

fn parse_workflow_run(run_dir: &Path, scripts_dir: Option<&Path>) -> Option<SubagentData> {
    let journal = fs::File::open(run_dir.join("journal.jsonl")).ok()?;
    let mut started: HashSet<String> = HashSet::new();
    let mut finished: HashSet<String> = HashSet::new();
    for line in BufReader::new(journal).lines().map_while(Result::ok) {
        let Some(val) = parse_json_line(&line) else {
            continue;
        };
        let Some(id) = val.get("agentId").and_then(|v| v.as_str()) else {
            continue;
        };
        match val.get("type").and_then(|v| v.as_str()) {
            Some("started") => {
                started.insert(id.to_string());
            }
            Some("result") => {
                finished.insert(id.to_string());
            }
            _ => {}
        }
    }
    if started.is_empty() {
        return None;
    }
    let total = started.len() as u32;
    let done = started.iter().filter(|id| finished.contains(*id)).count() as u32;

    // Aggregate the run's agent transcripts: summed tokens, earliest start,
    // freshest write.
    let mut context_tokens: u64 = 0;
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
            let (_, tokens) = read_subagent_usage(&file, metadata.len());
            context_tokens += tokens;
        }
    }
    if last_write_age_secs == u64::MAX {
        last_write_age_secs = 0;
    }

    let running = done < total;
    // A finished run lingers briefly, then drops off the roster.
    if !running && last_write_age_secs > RUN_DONE_LINGER_SECS {
        return None;
    }

    let runtime_secs = started_at.map(|t| {
        (chrono::Utc::now() - t)
            .num_seconds()
            .max(0)
            .cast_unsigned()
    });

    Some(SubagentData {
        task: String::new(),
        name: Some(workflow_run_name(run_dir, scripts_dir)),
        model: ModelShort::Unknown,
        context_tokens,
        runtime_secs,
        last_write_age_secs,
        state: if running {
            SessionState::Working
        } else {
            SessionState::Idle
        },
        progress: Some((done, total)),
    })
}

/// Resolve a workflow run's human name from its persisted script:
/// `<session>/workflows/scripts/<name>-<run-id>.js`. Falls back to the
/// run-dir name (`wf_...`).
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
/// (`agent-<id>.meta.json`). Present for teammates; name is optional for
/// anonymous Task-tool agents.
#[derive(Debug, serde::Deserialize)]
struct AgentMeta {
    name: Option<String>,
    description: Option<String>,
}

fn read_meta(jsonl_path: &Path) -> Option<AgentMeta> {
    let meta_path = jsonl_path.with_extension("meta.json");
    let data = fs::read_to_string(meta_path).ok()?;
    serde_json::from_str(&data).ok()
}

fn parse_subagent(path: &Path) -> Option<SubagentData> {
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

    let (model, context_tokens) = read_subagent_usage(&file, file_len);

    let state = detect_state_from_tail(&file, file_len);

    Some(SubagentData {
        task,
        name,
        model,
        context_tokens,
        runtime_secs,
        last_write_age_secs,
        state,
        progress: None,
    })
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

fn read_subagent_usage(file: &fs::File, file_len: u64) -> (ModelShort, u64) {
    let seek_pos = file_len.saturating_sub(super::RECENT_TAIL_BYTES);
    let Some(reader) = seek_tail(file, seek_pos) else {
        return (ModelShort::Unknown, 0);
    };

    let mut model = ModelShort::Unknown;
    let mut tokens: u64 = 0;

    for line in reader.lines().map_while(Result::ok) {
        let Some(val) = parse_json_line(&line) else {
            continue;
        };
        if !is_assistant_usage(&val) {
            continue;
        }
        if let Some(usage) = val.pointer("/message/usage") {
            let total = extract_tokens(usage);
            if total > 0 {
                tokens = total;
            }
        }
        if let Some(m) = val.pointer("/message/model").and_then(|v| v.as_str()) {
            model = parse_model(m);
        }
    }

    (model, tokens)
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
    use std::path::PathBuf;

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
        let agents = scan_subagents(dir.path(), &active, &HashMap::new());
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
        let agents = scan_subagents(dir.path(), &active, &HashMap::new());
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_subagents_nonexistent_dir() {
        let active = HashSet::from(["aaa".to_string()]);
        let agents = scan_subagents(
            Path::new("/nonexistent/dir/subagents"),
            &active,
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
        let agents = scan_subagents(dir.path(), &active, &HashMap::new());
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

        let agents = scan_subagents(dir.path(), &HashSet::new(), &HashMap::new());
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

        let agents = scan_subagents(&subagents_dir, &HashSet::new(), &HashMap::new());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name.as_deref(), Some("my-flow"));
    }

    #[test]
    fn scan_subagents_merges_direct_agents_and_workflow_runs() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(dir.path(), "direct1", "Direct agent", 1_000);
        write_workflow_run(dir.path(), "wf_abc123");

        let active = HashSet::from(["direct1".to_string()]);
        let agents = scan_subagents(dir.path(), &active, &HashMap::new());
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

        let agents = scan_subagents(dir.path(), &HashSet::new(), &HashMap::new());
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
        let agents = scan_subagents(dir.path(), &HashSet::new(), &HashMap::new());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].progress, Some((2, 2)));
        assert_eq!(agents[0].state, SessionState::Idle);

        // Quiet past the linger window: hidden
        for entry in fs::read_dir(&run_dir).unwrap().flatten() {
            set_mtime_ago(&entry.path(), RUN_DONE_LINGER_SECS + 60);
        }
        let agents = scan_subagents(dir.path(), &HashSet::new(), &HashMap::new());
        assert!(agents.is_empty());
    }

    // ── teammates ────────────────────────────────────────────────

    fn spawned_status() -> TeammateStatus {
        TeammateStatus {
            spawned_at: Some(chrono::Utc::now()),
            last_idle_at: None,
        }
    }

    fn idle_status() -> TeammateStatus {
        let now = chrono::Utc::now();
        TeammateStatus {
            spawned_at: Some(now - chrono::TimeDelta::seconds(600)),
            last_idle_at: Some(now),
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
        let agents = scan_subagents(dir.path(), &HashSet::new(), &teammates);
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
        let agents = scan_subagents(dir.path(), &HashSet::new(), &teammates);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state, SessionState::Idle);
    }

    #[test]
    fn scan_subagents_idle_teammate_stale_hidden() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(
            dir.path(),
            "amy-fixer-782b353b34c66890",
            "Fix things",
            3_000,
        );
        set_mtime_ago(
            &dir.path().join("agent-amy-fixer-782b353b34c66890.jsonl"),
            TEAMMATE_IDLE_HIDE_SECS + 60,
        );

        let teammates = HashMap::from([("my-fixer".to_string(), idle_status())]);
        let agents = scan_subagents(dir.path(), &HashSet::new(), &teammates);
        assert!(agents.is_empty());
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
        let agents = scan_subagents(dir.path(), &HashSet::new(), &teammates);
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
        let agents = scan_subagents(dir.path(), &HashSet::new(), &teammates);
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
        let agents = scan_subagents(dir.path(), &HashSet::new(), &teammates);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name.as_deref(), Some("my-fixer"));
        assert_eq!(agents[0].task, "Curated task title");
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

        let agents = scan_subagents(dir.path(), &HashSet::new(), &HashMap::new());
        assert!(agents.is_empty());
    }

    #[test]
    fn workflow_run_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_workflow_runs(dir.path()).is_empty());
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
        let agent = parse_subagent(&path).unwrap();
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

        let agent = parse_subagent(&path).unwrap();
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
