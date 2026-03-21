//! Shared test helpers for session parsing tests.

use std::io::Write;
use std::path::Path;

/// Write lines to a temp file and return the `File` handle.
pub(crate) fn jsonl_file(lines: &[&str]) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
    f.as_file().sync_all().unwrap();
    f
}

pub(crate) fn assistant_usage_line(input_tokens: u64) -> String {
    format!(
        r#"{{"type":"assistant","message":{{"model":"claude-sonnet-4-6","usage":{{"input_tokens":{input_tokens}}},"content":[{{"type":"text","text":"ok"}}],"stop_reason":"end_turn"}}}}"#
    )
}

/// Write a minimal subagent JSONL file.
pub(crate) fn write_agent_file(dir: &Path, agent_id: &str, task: &str, tokens: u64) {
    let path = dir.join(format!("agent-{agent_id}.jsonl"));
    let user_line = format!(r#"{{"type":"user","message":{{"content":"{task}"}}}}"#);
    let usage_line = format!(
        r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":{tokens}}},"content":[{{"type":"text","text":"ok"}}],"stop_reason":"end_turn"}}}}"#
    );
    let mut f = std::fs::File::create(path).unwrap();
    writeln!(f, "{user_line}").unwrap();
    writeln!(f, "{usage_line}").unwrap();
    f.sync_all().unwrap();
}

pub(crate) fn progress_line(agent_id: &str, tool_use_id: &str) -> String {
    format!(
        r#"{{"type":"progress","parentToolUseID":"{tool_use_id}","data":{{"type":"agent_progress","agentId":"{agent_id}"}}}}"#
    )
}

pub(crate) fn tool_result_line(tool_use_id: &str) -> String {
    format!(
        r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_use_id}","content":"done"}}]}}}}"#
    )
}

/// User message with `toolUseResult.isAsync: true` — launches a background agent.
pub(crate) fn async_agent_launch_line(
    tool_use_id: &str,
    agent_id: &str,
    description: &str,
) -> String {
    format!(
        r#"{{"type":"user","toolUseResult":{{"isAsync":true,"status":"async_launched","agentId":"{agent_id}","description":"{description}"}},"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_use_id}","content":"launched"}}]}}}}"#
    )
}

/// `queue-operation` enqueue with completed status — marks a background agent done.
pub(crate) fn queue_operation_complete_line(agent_id: &str) -> String {
    format!(
        r#"{{"type":"queue-operation","operation":"enqueue","content":"<task-notification>\n<task-id>{agent_id}</task-id>\n<status>completed</status>\n</task-notification>"}}"#
    )
}
