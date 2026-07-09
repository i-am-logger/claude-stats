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

/// Scan subagent JSONL files and return data for agents that are still active.
///
/// `active_ids` is the set of Task-tool and workflow agent IDs (e.g.
/// `"a2a043d116fbd87f0"`) that parent-JSONL tracking and workflow journals
/// show as still in-progress. `teammates` maps named-teammate lifecycle
/// statuses observed in the parent JSONL; their transcripts are matched by
/// the name embedded in the filename (`agent-a<name>-<hash>.jsonl`).
#[allow(
    clippy::implicit_hasher,
    reason = "internal API, always called with std collections"
)]
pub fn scan_subagents(
    subagents_dir: &Path,
    active_ids: &HashSet<String>,
    teammates: &HashMap<String, TeammateStatus>,
) -> Vec<SubagentData> {
    if active_ids.is_empty() && teammates.is_empty() {
        return Vec::new();
    }

    let mut agents = collect_agents(subagents_dir, active_ids, teammates);

    // Workflow-spawned agents live one level deeper:
    // subagents/workflows/<run-id>/agent-<id>.jsonl. Gate on transcript
    // freshness — a dead run's journal keeps its orphans forever.
    if let Ok(runs) = fs::read_dir(subagents_dir.join("workflows")) {
        let no_teammates = HashMap::new();
        for run in runs.flatten() {
            let path = run.path();
            if path.is_dir() {
                agents.extend(
                    collect_agents(&path, active_ids, &no_teammates)
                        .into_iter()
                        .filter(|a| a.last_write_age_secs <= AGENT_STALE_SECS),
                );
            }
        }
    }

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

            // Task-tool / workflow agents confirmed active by id
            if active_ids.contains(agent_id) {
                return parse_subagent(&path);
            }

            // Named teammates: the transcript id embeds the name
            let (tm_name, status) = teammates
                .iter()
                .find(|(n, _)| matches_teammate_id(agent_id, n))?;
            parse_teammate(&path, tm_name, status)
        })
        .collect()
}

/// A teammate transcript id is `a<name>-<hash>` — e.g.
/// `afix-load-docrefs-fb64396d7b46c88f` for teammate "fix-load-docrefs".
fn matches_teammate_id(agent_id: &str, name: &str) -> bool {
    agent_id
        .strip_prefix('a')
        .and_then(|rest| rest.strip_prefix(name))
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|hash| !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit()))
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

/// Collect agent IDs that workflow journals show as started but not finished.
///
/// Workflow runs don't emit `agent_progress` entries in the parent session
/// JSONL; instead each run journals into
/// `subagents/workflows/<run-id>/journal.jsonl` with one
/// `{"type":"started","agentId":...}` line per launched agent and a matching
/// `{"type":"result",...}` line when it completes.
#[allow(
    clippy::implicit_hasher,
    reason = "internal API, always called with std HashMap"
)]
pub fn workflow_active_ids(subagents_dir: &Path) -> HashSet<String> {
    let mut started = HashSet::new();
    let mut finished: HashSet<String> = HashSet::new();

    let Ok(runs) = fs::read_dir(subagents_dir.join("workflows")) else {
        return started;
    };

    for run in runs.flatten() {
        // Journals accumulate (one dir per run, results can be large) — skip
        // runs where nothing has been written recently instead of re-parsing
        // dead journals every scan.
        if !dir_recently_written(&run.path(), AGENT_STALE_SECS) {
            continue;
        }
        let journal = run.path().join("journal.jsonl");
        let Ok(file) = fs::File::open(&journal) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
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
    }

    started.retain(|id| !finished.contains(id));
    started
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

    #[test]
    fn scan_subagents_finds_workflow_agents() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("workflows").join("wf_abc123");
        fs::create_dir_all(&run_dir).unwrap();
        write_agent_file(&run_dir, "wfagent1", "Verify hypothesis", 2_000);

        let active = HashSet::from(["wfagent1".to_string()]);
        let agents = scan_subagents(dir.path(), &active, &HashMap::new());
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].task, "Verify hypothesis");
    }

    #[test]
    fn scan_subagents_merges_direct_and_workflow_agents() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_file(dir.path(), "direct1", "Direct agent", 1_000);
        let run_dir = dir.path().join("workflows").join("wf_abc123");
        fs::create_dir_all(&run_dir).unwrap();
        write_agent_file(&run_dir, "wfagent1", "Workflow agent", 2_000);

        let active = HashSet::from(["direct1".to_string(), "wfagent1".to_string()]);
        let agents = scan_subagents(dir.path(), &active, &HashMap::new());
        assert_eq!(agents.len(), 2);
    }

    fn set_mtime_ago(path: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(t)).unwrap();
    }

    #[test]
    fn scan_subagents_excludes_stale_workflow_agents() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("workflows").join("wf_dead");
        fs::create_dir_all(&run_dir).unwrap();
        write_agent_file(&run_dir, "orphan1", "Dead workflow agent", 2_000);
        set_mtime_ago(&run_dir.join("agent-orphan1.jsonl"), AGENT_STALE_SECS + 60);

        let active = HashSet::from(["orphan1".to_string()]);
        let agents = scan_subagents(dir.path(), &active, &HashMap::new());
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

    // ── matches_teammate_id ──────────────────────────────────────

    #[test]
    fn teammate_id_matches_name_with_hash() {
        assert!(matches_teammate_id(
            "afix-load-docrefs-fb64396d7b46c88f",
            "fix-load-docrefs"
        ));
    }

    #[test]
    fn teammate_id_rejects_wrong_name() {
        assert!(!matches_teammate_id(
            "afix-load-docrefs-fb64396d7b46c88f",
            "fix-load"
        ));
        assert!(!matches_teammate_id(
            "a2a043d116fbd87f0",
            "fix-load-docrefs"
        ));
    }

    #[test]
    fn teammate_id_rejects_non_hex_suffix() {
        assert!(!matches_teammate_id("amy-fixer-notahash!", "my-fixer"));
        assert!(!matches_teammate_id("amy-fixer-", "my-fixer"));
    }

    // ── workflow_active_ids ──────────────────────────────────────

    #[test]
    fn workflow_active_ids_started_without_result() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("workflows").join("wf_abc123");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("journal.jsonl"),
            concat!(
                "{\"type\":\"started\",\"key\":\"v2:k1\",\"agentId\":\"a1\"}\n",
                "{\"type\":\"started\",\"key\":\"v2:k2\",\"agentId\":\"a2\"}\n",
                "{\"type\":\"result\",\"key\":\"v2:k1\",\"agentId\":\"a1\",\"result\":\"done\"}\n",
            ),
        )
        .unwrap();

        let ids = workflow_active_ids(dir.path());
        assert_eq!(ids, HashSet::from(["a2".to_string()]));
    }

    #[test]
    fn workflow_active_ids_all_finished() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("workflows").join("wf_done");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("journal.jsonl"),
            concat!(
                "{\"type\":\"started\",\"key\":\"v2:k1\",\"agentId\":\"a1\"}\n",
                "{\"type\":\"result\",\"key\":\"v2:k1\",\"agentId\":\"a1\",\"result\":\"ok\"}\n",
            ),
        )
        .unwrap();

        assert!(workflow_active_ids(dir.path()).is_empty());
    }

    #[test]
    fn workflow_active_ids_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(workflow_active_ids(dir.path()).is_empty());
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
