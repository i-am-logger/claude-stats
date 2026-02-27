#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use super::tail::{parse_json_line, read_last_lines};
use super::SessionState;
use crate::fmt::truncate_str;
use serde_json::Value;
use std::collections::VecDeque;
use std::fs;

/// Classification of JSONL line types. State-indicating lines (Progress,
/// Assistant, System, User) drive session state; Metadata lines are skipped
/// when scanning for state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Progress,
    Assistant,
    System,
    User,
    Metadata,
}

fn classify_line(val: &Value) -> LineKind {
    match val.get("type").and_then(|t| t.as_str()) {
        Some("progress") => LineKind::Progress,
        Some("assistant" | "message") => LineKind::Assistant,
        Some("system") => LineKind::System,
        Some("user") => LineKind::User,
        _ => LineKind::Metadata,
    }
}

fn state_from_json(val: &Value) -> SessionState {
    match classify_line(val) {
        LineKind::Progress | LineKind::User => SessionState::Working,
        LineKind::System => match val.get("subtype").and_then(|s| s.as_str()) {
            Some("compact_boundary") => SessionState::Working,
            // turn_duration, init, and any other system subtypes → idle
            _ => SessionState::Idle,
        },
        LineKind::Assistant => state_from_assistant(val),
        LineKind::Metadata => SessionState::Idle,
    }
}

fn state_from_assistant(val: &Value) -> SessionState {
    let Some(content) = val.pointer("/message/content").and_then(|c| c.as_array()) else {
        return SessionState::Idle;
    };

    let has_tool_use = content
        .iter()
        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
    if has_tool_use {
        return SessionState::Working;
    }

    let has_thinking = content
        .iter()
        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("thinking"));

    // Complete if stop_reason is present and non-null
    let is_complete = val
        .pointer("/message/stop_reason")
        .is_some_and(|sr| !sr.is_null());

    if has_thinking {
        return if is_complete {
            SessionState::Idle
        } else {
            SessionState::Thinking
        };
    }

    // Text-only: still streaming → thinking, complete → idle
    if is_complete {
        SessionState::Idle
    } else {
        SessionState::Thinking
    }
}

pub fn detect_state_and_activity(lines: &VecDeque<String>) -> (SessionState, String) {
    if lines.is_empty() {
        return (SessionState::Idle, String::new());
    }

    let parsed: Vec<Option<Value>> = lines.iter().map(|l| parse_json_line(l)).collect();

    // Scan backwards for the last state-indicating line
    let state_entry = parsed.iter().rev().find_map(|opt| {
        let val = opt.as_ref()?;
        let kind = classify_line(val);
        if kind == LineKind::Metadata {
            None
        } else {
            Some(val)
        }
    });

    let state = state_entry.map_or(SessionState::Idle, state_from_json);

    let activity = match state {
        SessionState::Thinking => "thinking...".to_string(),
        SessionState::Working => extract_activity_from_parsed(&parsed),
        SessionState::Idle => String::new(),
    };

    (state, activity)
}

/// Lightweight state detection for subagent files — reads last 3 lines and
/// returns just the state without computing an activity string.
pub fn detect_state_from_tail(file: &fs::File, file_len: u64) -> SessionState {
    let lines = read_last_lines(file, file_len, 3);
    if lines.is_empty() {
        return SessionState::Idle;
    }

    lines
        .iter()
        .rev()
        .filter_map(|l| parse_json_line(l))
        .find(|val| classify_line(val) != LineKind::Metadata)
        .map_or(SessionState::Idle, |val| state_from_json(&val))
}

fn extract_activity_from_parsed(parsed: &[Option<Value>]) -> String {
    for val in parsed.iter().rev().flatten() {
        match classify_line(val) {
            LineKind::Progress => {
                if let Some(activity) = extract_progress_activity(val) {
                    return activity;
                }
                // bash_progress or unrecognized — continue scanning
            }
            LineKind::Assistant => {
                return extract_tool_names(val);
            }
            _ => {}
        }
    }
    "working".to_string()
}

fn extract_progress_activity(val: &Value) -> Option<String> {
    let data_type = val.pointer("/data/type").and_then(|t| t.as_str());
    match data_type {
        Some("bash_progress") => {
            // bash_progress doesn't carry the command; let scan continue
            // to find the preceding assistant tool_use block
            None
        }
        Some("hook_progress") => {
            let hook_name = val
                .pointer("/data/hookName")
                .and_then(|n| n.as_str())
                .unwrap_or("hook");
            Some(format!("Hook({})", truncate_str(hook_name, 30)))
        }
        Some("agent_progress") => {
            let prompt = val
                .pointer("/data/prompt")
                .and_then(|p| p.as_str())
                .unwrap_or("");
            let first_line = prompt.lines().next().unwrap_or("").trim();
            if first_line.is_empty() {
                Some("subagent working".to_string())
            } else {
                Some(format!("Agent({})", truncate_str(first_line, 40)))
            }
        }
        Some("waiting_for_task") => Some("waiting for task".to_string()),
        _ => {
            // Other progress types (tool output streaming, etc.) — working
            Some("working".to_string())
        }
    }
}

fn extract_tool_names(val: &Value) -> String {
    let Some(content) = val.pointer("/message/content") else {
        return "working".to_string();
    };

    let tools: Vec<String> = content
        .as_array()
        .map(|arr| arr.iter().filter_map(format_tool_use).collect())
        .unwrap_or_default();

    if tools.is_empty() {
        "working".to_string()
    } else {
        tools.join(", ")
    }
}

fn format_tool_use(block: &Value) -> Option<String> {
    if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
        return None;
    }

    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
    Some(match name {
        "Bash" => {
            let cmd = block
                .pointer("/input/command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let short = cmd.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
            if short.is_empty() {
                "Bash".to_string()
            } else {
                format!("Bash({})", truncate_str(&short, 30))
            }
        }
        "Read" | "Edit" | "Write" | "Glob" | "Grep" => {
            let path = block
                .pointer("/input/file_path")
                .or_else(|| block.pointer("/input/pattern"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let short = path.rsplit('/').next().unwrap_or(path);
            if short.is_empty() {
                name.to_string()
            } else {
                format!("{}({})", name, truncate_str(short, 25))
            }
        }
        "Task" => "Task(subagent)".to_string(),
        _ => name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── classify_line ──────────────────────────────────────────────

    #[test]
    fn classify_progress() {
        let val = json!({"type": "progress", "data": {}});
        assert_eq!(classify_line(&val), LineKind::Progress);
    }

    #[test]
    fn classify_assistant() {
        let val = json!({"type": "assistant", "message": {}});
        assert_eq!(classify_line(&val), LineKind::Assistant);
    }

    #[test]
    fn classify_message_as_assistant() {
        let val = json!({"type": "message", "message": {"stop_reason": "end_turn"}});
        assert_eq!(classify_line(&val), LineKind::Assistant);
    }

    #[test]
    fn classify_system() {
        let val = json!({"type": "system", "subtype": "turn_duration"});
        assert_eq!(classify_line(&val), LineKind::System);
    }

    #[test]
    fn classify_user() {
        let val = json!({"type": "user", "message": {"content": "hi"}});
        assert_eq!(classify_line(&val), LineKind::User);
    }

    #[test]
    fn classify_metadata_types() {
        for t in &[
            "file-history-snapshot",
            "queue-operation",
            "summary",
            "custom-title",
            "tag",
            "mode",
            "agent-name",
            "agent-color",
            "agent-setting",
            "pr-link",
            "attribution-snapshot",
            "speculation-accept",
        ] {
            let val = json!({"type": t});
            assert_eq!(
                classify_line(&val),
                LineKind::Metadata,
                "expected Metadata for {t}"
            );
        }
    }

    #[test]
    fn classify_unknown_type() {
        let val = json!({"type": "some-future-type"});
        assert_eq!(classify_line(&val), LineKind::Metadata);
    }

    #[test]
    fn classify_missing_type() {
        let val = json!({"data": "no type field"});
        assert_eq!(classify_line(&val), LineKind::Metadata);
    }

    // ── state_from_json ────────────────────────────────────────────

    #[test]
    fn state_progress_is_working() {
        let val = json!({"type": "progress", "data": {"type": "bash_progress"}});
        assert_eq!(state_from_json(&val), SessionState::Working);
    }

    #[test]
    fn state_user_is_working() {
        let val = json!({"type": "user", "message": {"content": "hello"}});
        assert_eq!(state_from_json(&val), SessionState::Working);
    }

    #[test]
    fn state_assistant_tool_use_is_working() {
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "tool_use", "name": "Bash", "input": {}}],
                "stop_reason": null
            }
        });
        assert_eq!(state_from_json(&val), SessionState::Working);
    }

    #[test]
    fn state_assistant_thinking_only_is_thinking() {
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "thinking", "thinking": "hmm"}],
                "stop_reason": null
            }
        });
        assert_eq!(state_from_json(&val), SessionState::Thinking);
    }

    #[test]
    fn state_assistant_thinking_plus_text_streaming_is_thinking() {
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "thinking", "thinking": "hmm"},
                    {"type": "text", "text": "partial"}
                ],
                "stop_reason": null
            }
        });
        assert_eq!(state_from_json(&val), SessionState::Thinking);
    }

    #[test]
    fn state_assistant_thinking_plus_text_complete_is_idle() {
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "thinking", "thinking": "hmm"},
                    {"type": "text", "text": "done"}
                ],
                "stop_reason": "end_turn"
            }
        });
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_assistant_text_only_complete_is_idle() {
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": "done"}],
                "stop_reason": "end_turn"
            }
        });
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_message_type_complete_is_idle() {
        let val = json!({
            "type": "message",
            "message": {
                "content": [{"type": "text", "text": "done"}],
                "stop_reason": "end_turn"
            }
        });
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_message_type_streaming_is_thinking() {
        let val = json!({
            "type": "message",
            "message": {
                "content": [{"type": "text", "text": "partial"}],
                "stop_reason": null
            }
        });
        assert_eq!(state_from_json(&val), SessionState::Thinking);
    }

    #[test]
    fn state_assistant_text_streaming_is_thinking() {
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": "partial"}],
                "stop_reason": null
            }
        });
        assert_eq!(state_from_json(&val), SessionState::Thinking);
    }

    #[test]
    fn state_assistant_no_content_is_idle() {
        let val = json!({"type": "assistant", "message": {}});
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_system_turn_duration_is_idle() {
        let val = json!({"type": "system", "subtype": "turn_duration", "durationMs": 5000});
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_system_compact_boundary_is_working() {
        let val = json!({"type": "system", "subtype": "compact_boundary"});
        assert_eq!(state_from_json(&val), SessionState::Working);
    }

    #[test]
    fn state_system_generic_is_idle() {
        let val = json!({"type": "system", "subtype": "init"});
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_metadata_is_idle() {
        let val = json!({"type": "tag", "value": "test"});
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    // ── extract_activity ───────────────────────────────────────────

    #[test]
    fn activity_hook_progress() {
        let val = json!({
            "type": "progress",
            "data": {"type": "hook_progress", "hookName": "pre-commit"}
        });
        assert_eq!(
            extract_activity_from_parsed(&[Some(val)]),
            "Hook(pre-commit)"
        );
    }

    #[test]
    fn activity_agent_progress_with_prompt() {
        let val = json!({
            "type": "progress",
            "data": {"type": "agent_progress", "prompt": "Search for tests\nin the repo"}
        });
        assert_eq!(
            extract_activity_from_parsed(&[Some(val)]),
            "Agent(Search for tests)"
        );
    }

    #[test]
    fn activity_agent_progress_no_prompt() {
        let val = json!({
            "type": "progress",
            "data": {"type": "agent_progress"}
        });
        assert_eq!(
            extract_activity_from_parsed(&[Some(val)]),
            "subagent working"
        );
    }

    #[test]
    fn activity_waiting_for_task() {
        let val = json!({
            "type": "progress",
            "data": {"type": "waiting_for_task"}
        });
        assert_eq!(
            extract_activity_from_parsed(&[Some(val)]),
            "waiting for task"
        );
    }

    #[test]
    fn activity_bash_progress_falls_through_to_tool_use() {
        let tool_val = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test"}}
                ]
            }
        });
        let progress_val = json!({
            "type": "progress",
            "data": {"type": "bash_progress", "output": "running..."}
        });
        // tool_use comes first, then bash_progress
        let parsed = vec![Some(tool_val), Some(progress_val)];
        assert_eq!(extract_activity_from_parsed(&parsed), "Bash(cargo test)");
    }

    #[test]
    fn activity_bash_progress_alone_falls_back_to_working() {
        let val = json!({
            "type": "progress",
            "data": {"type": "bash_progress", "output": "stuff"}
        });
        assert_eq!(extract_activity_from_parsed(&[Some(val)]), "working");
    }

    #[test]
    fn activity_metadata_lines_skipped() {
        let meta = json!({"type": "tag", "value": "v1"});
        let tool = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "tool_use", "name": "Read", "input": {"file_path": "/src/main.rs"}}
                ]
            }
        });
        let parsed = vec![Some(tool), Some(meta.clone()), Some(meta)];
        assert_eq!(extract_activity_from_parsed(&parsed), "Read(main.rs)");
    }

    #[test]
    fn activity_fallback_working() {
        let parsed: Vec<Option<Value>> = vec![None, None];
        assert_eq!(extract_activity_from_parsed(&parsed), "working");
    }

    // ── format_tool_use ────────────────────────────────────────────

    #[test]
    fn format_tool_use_bash() {
        let block = json!({"type": "tool_use", "name": "Bash", "input": {"command": "git status"}});
        assert_eq!(format_tool_use(&block).unwrap(), "Bash(git status)");
    }

    #[test]
    fn format_tool_use_read() {
        let block =
            json!({"type": "tool_use", "name": "Read", "input": {"file_path": "/a/b/c.rs"}});
        assert_eq!(format_tool_use(&block).unwrap(), "Read(c.rs)");
    }

    #[test]
    fn format_tool_use_glob() {
        let block = json!({"type": "tool_use", "name": "Glob", "input": {"pattern": "**/*.rs"}});
        // rsplit('/') on "**/*.rs" yields "*.rs" (last path segment)
        assert_eq!(format_tool_use(&block).unwrap(), "Glob(*.rs)");
    }

    #[test]
    fn format_tool_use_task() {
        let block = json!({"type": "tool_use", "name": "Task", "input": {}});
        assert_eq!(format_tool_use(&block).unwrap(), "Task(subagent)");
    }

    #[test]
    fn format_tool_use_unknown() {
        let block = json!({"type": "tool_use", "name": "CustomTool", "input": {}});
        assert_eq!(format_tool_use(&block).unwrap(), "CustomTool");
    }

    #[test]
    fn format_tool_use_non_tool() {
        let block = json!({"type": "text", "text": "hello"});
        assert!(format_tool_use(&block).is_none());
    }

    #[test]
    fn format_tool_use_bash_empty_command() {
        let block = json!({"type": "tool_use", "name": "Bash", "input": {}});
        assert_eq!(format_tool_use(&block).unwrap(), "Bash");
    }

    #[test]
    fn format_tool_use_read_empty_path() {
        let block = json!({"type": "tool_use", "name": "Read", "input": {}});
        assert_eq!(format_tool_use(&block).unwrap(), "Read");
    }

    #[test]
    fn format_tool_use_multiple_tools() {
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
                    {"type": "tool_use", "name": "Read", "input": {"file_path": "/foo/bar.rs"}}
                ]
            }
        });
        assert_eq!(extract_tool_names(&val), "Bash(ls), Read(bar.rs)");
    }

    // ── edge cases ─────────────────────────────────────────────────

    #[test]
    fn state_from_json_empty_object() {
        let val = json!({});
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_from_json_null_type() {
        let val = json!({"type": null});
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_from_json_numeric_type() {
        let val = json!({"type": 42});
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_assistant_content_not_array() {
        let val = json!({
            "type": "assistant",
            "message": {"content": "just a string"}
        });
        assert_eq!(state_from_json(&val), SessionState::Idle);
    }

    #[test]
    fn state_assistant_empty_content_array() {
        let val = json!({
            "type": "assistant",
            "message": {"content": [], "stop_reason": null}
        });
        // Empty content, still streaming → thinking
        assert_eq!(state_from_json(&val), SessionState::Thinking);
    }

    #[test]
    fn extract_activity_all_unparseable() {
        let parsed: Vec<Option<Value>> = vec![None, None, None];
        assert_eq!(extract_activity_from_parsed(&parsed), "working");
    }

    #[test]
    fn extract_activity_empty_input() {
        let parsed: Vec<Option<Value>> = vec![];
        assert_eq!(extract_activity_from_parsed(&parsed), "working");
    }

    #[test]
    fn extract_tool_names_missing_content() {
        let val = json!({"type": "assistant", "message": {}});
        assert_eq!(extract_tool_names(&val), "working");
    }

    #[test]
    fn extract_tool_names_empty_content() {
        let val = json!({
            "type": "assistant",
            "message": {"content": []}
        });
        assert_eq!(extract_tool_names(&val), "working");
    }

    #[test]
    fn extract_tool_names_no_tool_use_blocks() {
        let val = json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "hi"}]}
        });
        assert_eq!(extract_tool_names(&val), "working");
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn arb_type() -> impl Strategy<Value = &'static str> {
            prop_oneof![
                Just("progress"),
                Just("assistant"),
                Just("system"),
                Just("user"),
                Just("tag"),
                Just("unknown"),
            ]
        }

        proptest! {
            #[test]
            fn prop_classify_line_never_panics(type_str in arb_type()) {
                let val = json!({"type": type_str});
                let _ = classify_line(&val);
            }

            #[test]
            fn prop_detect_state_and_activity_valid_state(
                line_count in 0..=10_usize,
                type_str in arb_type(),
            ) {
                let lines: VecDeque<String> = (0..line_count)
                    .map(|_| format!(r#"{{"type":"{}"}}"#, type_str))
                    .collect();
                let (state, _activity) = detect_state_and_activity(&lines);
                // State must be a valid variant
                assert!(matches!(
                    state,
                    SessionState::Idle | SessionState::Thinking | SessionState::Working
                ));
            }
        }
    }
}
