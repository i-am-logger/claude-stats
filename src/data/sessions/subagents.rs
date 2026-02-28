#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use super::activity::detect_state_from_tail;
use super::tail::{extract_tokens, is_assistant_usage, parse_json_line, seek_tail};
use super::{ModelShort, SubagentData};
use crate::fmt::truncate_str;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::Path;

/// Scan subagent JSONL files and return data for agents that are still active.
///
/// `active_ids` is the set of agent IDs (e.g. `"a2a043d116fbd87f0"`) that the
/// parent session JSONL shows as still in-progress (have `agent_progress`
/// entries but no corresponding `tool_result`). Only agents in this set are
/// included.
#[allow(
    clippy::implicit_hasher,
    reason = "internal API, always called with std HashMap"
)]
pub fn scan_subagents(subagents_dir: &Path, active_ids: &HashSet<String>) -> Vec<SubagentData> {
    if active_ids.is_empty() {
        return Vec::new();
    }

    let Ok(entries) = fs::read_dir(subagents_dir) else {
        return Vec::new();
    };

    let mut agents: Vec<SubagentData> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                return None;
            }
            let name = path.file_stem()?.to_str()?;
            // Extract agent ID from filename: "agent-a2a043d116fbd87f0" → "a2a043d116fbd87f0"
            let agent_id = name.strip_prefix("agent-")?;
            // Only include agents confirmed active by parent JSONL
            if !active_ids.contains(agent_id) {
                return None;
            }
            parse_subagent(&path)
        })
        .collect();

    agents.sort_by(|a, b| a.task.cmp(&b.task));
    agents
}

fn parse_subagent(path: &Path) -> Option<SubagentData> {
    let file = fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    if file_len == 0 {
        return None;
    }

    let task = read_subagent_task(&file);
    let (model, context_tokens) = read_subagent_usage(&file, file_len);

    let state = detect_state_from_tail(&file, file_len);

    Some(SubagentData {
        task,
        model,
        context_tokens,
        state,
    })
}

fn read_subagent_task(file: &fs::File) -> String {
    let mut file = file;
    if file.seek(SeekFrom::Start(0)).is_err() {
        return String::new();
    }
    let reader = std::io::BufReader::new(&mut file);

    for line in reader.lines().take(3).map_while(Result::ok) {
        let Some(val) = parse_json_line(&line) else {
            continue;
        };
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
            return truncate_str(cleaned, 60);
        }
    }
    String::new()
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
    if model.contains("opus") {
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
        let agents = scan_subagents(dir.path(), &active);
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
        let agents = scan_subagents(dir.path(), &active);
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_subagents_nonexistent_dir() {
        let active: HashSet<String> = ["aaa"].iter().map(|s| (*s).to_string()).collect();
        let agents = scan_subagents(Path::new("/nonexistent/dir/subagents"), &active);
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_subagents_skips_compact_prefix() {
        let dir = tempfile::tempdir().unwrap();
        // "acompact-xyz.jsonl" should not match strip_prefix("agent-")
        let path = dir.path().join("acompact-xyz.jsonl");
        fs::write(&path, r#"{"type":"user","message":{"content":"hi"}}"#).unwrap();

        let active: HashSet<String> = ["xyz"].iter().map(|s| (*s).to_string()).collect();
        let agents = scan_subagents(dir.path(), &active);
        assert!(agents.is_empty());
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
        read_subagent_task(&file)
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
        read_subagent_task(&file)
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
