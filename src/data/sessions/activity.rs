use super::tail::{read_last_lines, truncate_str};
use super::SessionState;
use std::fs;

pub(super) fn detect_state_and_activity(file: &fs::File, file_len: u64) -> (SessionState, String) {
    let lines = read_last_lines(file, file_len, 5);
    if lines.is_empty() {
        return (SessionState::Idle, String::new());
    }
    let last = &lines[lines.len() - 1];
    let state = state_from_line(last);
    let activity = match state {
        SessionState::Thinking => "thinking...".to_string(),
        SessionState::Working => extract_activity(&lines),
        SessionState::Idle => String::new(),
    };
    (state, activity)
}

pub(super) fn state_from_line(line: &str) -> SessionState {
    if line.contains("\"type\":\"progress\"") {
        if line.contains("\"type\":\"assistant\"") {
            SessionState::Thinking
        } else {
            SessionState::Working
        }
    } else if line.contains("\"type\":\"assistant\"") {
        if line.contains("\"tool_use\"") {
            SessionState::Working
        } else {
            SessionState::Idle
        }
    } else if line.contains("\"type\":\"user\"") {
        SessionState::Working
    } else {
        SessionState::Idle
    }
}

fn extract_activity(lines: &[String]) -> String {
    for line in lines.iter().rev() {
        if line.contains("\"bash_progress\"") {
            if let Some(cmd) = extract_json_string(line, "command") {
                let short = cmd.split_whitespace().take(4).collect::<Vec<_>>().join(" ");
                return format!("Bash({})", truncate_str(&short, 40));
            }
        }

        if line.contains("\"type\":\"assistant\"") && line.contains("\"tool_use\"") {
            return extract_tool_names(line);
        }

        if line.contains("\"hook_progress\"") {
            return "running hook...".to_string();
        }
    }
    "working".to_string()
}

fn extract_tool_names(line: &str) -> String {
    let Some(val) = super::tail::parse_json_line(line) else {
        return "working".to_string();
    };

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

fn format_tool_use(block: &serde_json::Value) -> Option<String> {
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

fn extract_json_string<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\":\"");
    let start = line.find(&pattern)? + pattern.len();
    let end = line[start..].find('"')? + start;
    Some(&line[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_progress_assistant_is_thinking() {
        let line = r#"{"type":"progress","message":{"type":"assistant"}}"#;
        assert_eq!(state_from_line(line), SessionState::Thinking);
    }

    #[test]
    fn state_progress_non_assistant_is_working() {
        let line = r#"{"type":"progress","tool":"bash"}"#;
        assert_eq!(state_from_line(line), SessionState::Working);
    }

    #[test]
    fn state_assistant_with_tool_use_is_working() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use"}]}}"#;
        assert_eq!(state_from_line(line), SessionState::Working);
    }

    #[test]
    fn state_assistant_without_tool_use_is_idle() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text"}]}}"#;
        assert_eq!(state_from_line(line), SessionState::Idle);
    }

    #[test]
    fn state_user_is_working() {
        let line = r#"{"type":"user","message":"hello"}"#;
        assert_eq!(state_from_line(line), SessionState::Working);
    }

    #[test]
    fn state_unknown_is_idle() {
        let line = r#"{"type":"system"}"#;
        assert_eq!(state_from_line(line), SessionState::Idle);
    }
}
