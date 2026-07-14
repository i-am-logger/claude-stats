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

/// A session's activity text, as far as `activity.rs` alone can resolve it.
///
/// Almost everything resolves directly to `Text`; `TaskUpdate` is the one
/// exception — its `tool_use` input is only `{taskId, status}`, no human
/// text, so the caller (which has `session_id`/`tasks_root`, unavailable in
/// this filesystem-agnostic module) must resolve it with a further shared-
/// `TaskList` lookup by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Text(String),
    PendingTaskUpdate {
        task_id: String,
    },
    /// The session just went idle because its last turn finished — a
    /// `system`/`turn_duration` entry, carrying how long that turn took.
    /// The caller decides whether it's recent enough to still be worth
    /// showing (this module has no clock/"now" access) before falling back
    /// to a blank idle row.
    Done {
        duration_ms: u64,
    },
}

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

/// `durationMs` of a `system`/`turn_duration` entry — `None` for any other
/// line shape.
fn turn_duration_ms(val: &Value) -> Option<u64> {
    if val.get("type").and_then(|t| t.as_str()) != Some("system")
        || val.get("subtype").and_then(|s| s.as_str()) != Some("turn_duration")
    {
        return None;
    }
    val.get("durationMs").and_then(Value::as_u64)
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

    // Complete if stop_reason is present and non-null
    let is_complete = val
        .pointer("/message/stop_reason")
        .is_some_and(|sr| !sr.is_null());

    if is_complete {
        SessionState::Idle
    } else {
        SessionState::Thinking
    }
}

pub fn detect_state_and_activity(lines: &VecDeque<String>) -> (SessionState, Activity) {
    if lines.is_empty() {
        return (SessionState::Idle, Activity::Text(String::new()));
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
        SessionState::Thinking => Activity::Text("thinking...".to_string()),
        SessionState::Working => extract_activity_from_parsed(&parsed),
        SessionState::Idle => state_entry
            .and_then(turn_duration_ms)
            .map_or(Activity::Text(String::new()), |duration_ms| {
                Activity::Done { duration_ms }
            }),
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

fn extract_activity_from_parsed(parsed: &[Option<Value>]) -> Activity {
    for val in parsed.iter().rev().flatten() {
        match classify_line(val) {
            // `progress`-type entries (hook_progress/agent_progress/
            // waiting_for_task/bash_progress) are dead in Claude Code
            // 2.1.198+ — confirmed empirically, zero live hits across real
            // transcripts (see docs/claude-code-internals.md §2.4). Still
            // classified as `LineKind::Progress` for state detection
            // (state_from_json), but there's no activity text left to
            // extract here — skip and keep scanning for the last real
            // assistant tool_use.
            // A trailing compact_boundary is a real, alive signal (unlike
            // the dead progress subtypes above) — state_from_json already
            // treats it as Working; say what's actually happening instead
            // of falling through to the generic "working".
            LineKind::System
                if val.get("subtype").and_then(|s| s.as_str()) == Some("compact_boundary") =>
            {
                return Activity::Text("compacting".to_string());
            }
            LineKind::Assistant => {
                return extract_tool_names(val);
            }
            _ => {}
        }
    }
    Activity::Text("working".to_string())
}

fn extract_tool_names(val: &Value) -> Activity {
    let Some(content) = val.pointer("/message/content") else {
        return Activity::Text("working".to_string());
    };
    let Some(arr) = content.as_array() else {
        return Activity::Text("working".to_string());
    };

    // A lone TaskUpdate call needs a further shared-TaskList lookup by id —
    // its own tool_use input is only {taskId, status}, no human text.
    // Scoped to the common case (TaskUpdate is the only tool call in the
    // turn, its usual "just a status bookkeeping call" shape); a TaskUpdate
    // mixed with other tool calls in the same turn falls through to the
    // generic per-tool rendering below like everything else.
    let tool_uses: Vec<&Value> = arr
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .collect();
    if let [only] = tool_uses[..] {
        if only.get("name").and_then(|n| n.as_str()) == Some("TaskUpdate") {
            if let Some(task_id) = only.pointer("/input/taskId").and_then(|v| v.as_str()) {
                return Activity::PendingTaskUpdate {
                    task_id: task_id.to_string(),
                };
            }
        }
    }

    let tools: Vec<String> = arr.iter().filter_map(format_tool_use).collect();
    if tools.is_empty() {
        Activity::Text("working".to_string())
    } else {
        Activity::Text(tools.join(", "))
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
        // The activeForm/subject text is already sitting in the tool_use
        // block — no extra file read needed, unlike TaskUpdate (whose input
        // is only {taskId, status}, so its human-readable text lives only in
        // the shared TaskList sidecar file — see tasks.rs).
        "TaskCreate" => block
            .pointer("/input/activeForm")
            .or_else(|| block.pointer("/input/subject"))
            .and_then(|v| v.as_str())
            .map_or_else(|| "TaskCreate".to_string(), |s| truncate_str(s, 60)),
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

    // ── detect_state_and_activity: turn_duration → Done ─────────────

    #[test]
    fn detect_state_and_activity_turn_duration_is_done() {
        let lines: VecDeque<String> = VecDeque::from([
            json!({"type": "system", "subtype": "turn_duration", "durationMs": 135_000})
                .to_string(),
        ]);
        let (state, activity) = detect_state_and_activity(&lines);
        assert_eq!(state, SessionState::Idle);
        assert_eq!(
            activity,
            Activity::Done {
                duration_ms: 135_000
            }
        );
    }

    #[test]
    fn detect_state_and_activity_plain_idle_has_no_done() {
        let lines: VecDeque<String> =
            VecDeque::from([json!({"type": "system", "subtype": "init"}).to_string()]);
        let (state, activity) = detect_state_and_activity(&lines);
        assert_eq!(state, SessionState::Idle);
        assert_eq!(activity, Activity::Text(String::new()));
    }

    #[test]
    fn turn_duration_ms_rejects_other_subtypes() {
        let val = json!({"type": "system", "subtype": "init", "durationMs": 5000});
        assert_eq!(turn_duration_ms(&val), None);
    }

    #[test]
    fn turn_duration_ms_missing_field() {
        let val = json!({"type": "system", "subtype": "turn_duration"});
        assert_eq!(turn_duration_ms(&val), None);
    }

    // ── extract_activity ───────────────────────────────────────────

    #[test]
    fn activity_progress_line_skipped_falls_through_to_tool_use() {
        // `progress`-type entries carry no usable activity text (dead since
        // 2.1.198+) — the scan must skip over one and keep looking for the
        // preceding real assistant tool_use.
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
        let parsed = vec![Some(tool_val), Some(progress_val)];
        assert_eq!(
            extract_activity_from_parsed(&parsed),
            Activity::Text("Bash(cargo test)".to_string())
        );
    }

    #[test]
    fn activity_progress_line_alone_falls_back_to_working() {
        let val = json!({
            "type": "progress",
            "data": {"type": "bash_progress", "output": "stuff"}
        });
        assert_eq!(
            extract_activity_from_parsed(&[Some(val)]),
            Activity::Text("working".to_string())
        );
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
        assert_eq!(
            extract_activity_from_parsed(&parsed),
            Activity::Text("Read(main.rs)".to_string())
        );
    }

    #[test]
    fn activity_fallback_working() {
        let parsed: Vec<Option<Value>> = vec![None, None];
        assert_eq!(
            extract_activity_from_parsed(&parsed),
            Activity::Text("working".to_string())
        );
    }

    #[test]
    fn activity_compact_boundary_shows_compacting() {
        let val = json!({"type": "system", "subtype": "compact_boundary"});
        assert_eq!(
            extract_activity_from_parsed(&[Some(val)]),
            Activity::Text("compacting".to_string())
        );
    }

    #[test]
    fn activity_other_system_subtype_falls_back_to_working() {
        let val = json!({"type": "system", "subtype": "turn_duration"});
        assert_eq!(
            extract_activity_from_parsed(&[Some(val)]),
            Activity::Text("working".to_string())
        );
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
    fn format_tool_use_task_create_shows_active_form() {
        let block = json!({
            "type": "tool_use",
            "name": "TaskCreate",
            "input": {"subject": "Fix the bug", "activeForm": "Fixing the bug"}
        });
        assert_eq!(format_tool_use(&block).unwrap(), "Fixing the bug");
    }

    #[test]
    fn format_tool_use_task_create_falls_back_to_subject() {
        let block = json!({
            "type": "tool_use",
            "name": "TaskCreate",
            "input": {"subject": "Fix the bug"}
        });
        assert_eq!(format_tool_use(&block).unwrap(), "Fix the bug");
    }

    #[test]
    fn format_tool_use_task_create_no_fields() {
        let block = json!({"type": "tool_use", "name": "TaskCreate", "input": {}});
        assert_eq!(format_tool_use(&block).unwrap(), "TaskCreate");
    }

    #[test]
    fn extract_tool_names_lone_task_update_is_pending() {
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "tool_use", "name": "TaskUpdate", "input": {"taskId": "42", "status": "in_progress"}}
                ]
            }
        });
        assert_eq!(
            extract_tool_names(&val),
            Activity::PendingTaskUpdate {
                task_id: "42".to_string()
            }
        );
    }

    #[test]
    fn extract_tool_names_task_update_mixed_with_other_tool_renders_as_text() {
        // Not the lone-call shape TaskUpdate usually has — falls through to
        // the generic per-tool rendering (bare "TaskUpdate" alongside it)
        // rather than trying to resolve a lookup for just one of several
        // tool calls in the same turn.
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
                    {"type": "tool_use", "name": "TaskUpdate", "input": {"taskId": "42", "status": "in_progress"}}
                ]
            }
        });
        assert_eq!(
            extract_tool_names(&val),
            Activity::Text("Bash(ls), TaskUpdate".to_string())
        );
    }

    #[test]
    fn extract_tool_names_task_update_missing_task_id_falls_back_to_text() {
        let val = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "tool_use", "name": "TaskUpdate", "input": {"status": "in_progress"}}
                ]
            }
        });
        assert_eq!(
            extract_tool_names(&val),
            Activity::Text("TaskUpdate".to_string())
        );
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
        assert_eq!(
            extract_tool_names(&val),
            Activity::Text("Bash(ls), Read(bar.rs)".to_string())
        );
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
        assert_eq!(
            extract_activity_from_parsed(&parsed),
            Activity::Text("working".to_string())
        );
    }

    #[test]
    fn extract_activity_empty_input() {
        let parsed: Vec<Option<Value>> = vec![];
        assert_eq!(
            extract_activity_from_parsed(&parsed),
            Activity::Text("working".to_string())
        );
    }

    #[test]
    fn extract_tool_names_missing_content() {
        let val = json!({"type": "assistant", "message": {}});
        assert_eq!(
            extract_tool_names(&val),
            Activity::Text("working".to_string())
        );
    }

    #[test]
    fn extract_tool_names_empty_content() {
        let val = json!({
            "type": "assistant",
            "message": {"content": []}
        });
        assert_eq!(
            extract_tool_names(&val),
            Activity::Text("working".to_string())
        );
    }

    #[test]
    fn extract_tool_names_no_tool_use_blocks() {
        let val = json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "hi"}]}
        });
        assert_eq!(
            extract_tool_names(&val),
            Activity::Text("working".to_string())
        );
    }

    // ── detect_state_from_tail ────────────────────────────────────

    #[test]
    fn detect_state_from_tail_skips_metadata() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // metadata, working-assistant, metadata — state should come from the assistant line
        let metadata = r#"{"type":"tag","value":"v1"}"#;
        let working = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}],"stop_reason":null}}"#;
        writeln!(f, "{metadata}").unwrap();
        writeln!(f, "{working}").unwrap();
        writeln!(f, "{metadata}").unwrap();
        f.as_file().sync_all().unwrap();

        let file = fs::File::open(f.path()).unwrap();
        let file_len = file.metadata().unwrap().len();
        let state = detect_state_from_tail(&file, file_len);
        assert_eq!(state, SessionState::Working);
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
                    .map(|_| format!(r#"{{"type":"{type_str}"}}"#))
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
