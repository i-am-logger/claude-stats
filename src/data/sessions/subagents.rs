use super::activity::state_from_line;
use super::tail::{extract_tokens, parse_json_line, read_last_line, seek_tail, truncate_str};
use super::SubagentData;
use std::fs;
use std::io::BufRead;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Subagent files older than 30s are considered finished and excluded from the
/// active subagent list (half of `MAX_AGE_SECS` for the parent session).
const SUBAGENT_MAX_AGE_SECS: u64 = 30;

pub(super) fn scan_subagents(subagents_dir: &Path) -> Vec<SubagentData> {
    let now = SystemTime::now();
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
            if name.contains("compact") {
                return None;
            }
            let age = now
                .duration_since(fs::metadata(&path).ok()?.modified().ok()?)
                .unwrap_or(Duration::MAX)
                .as_secs();
            if age > SUBAGENT_MAX_AGE_SECS {
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

    let task = read_subagent_task(path);
    let (model_short, context_tokens) = read_subagent_usage(&file, file_len);

    let last_line = read_last_line(&file, file_len)?;
    let state = state_from_line(&last_line);

    Some(SubagentData {
        task,
        model_short,
        context_tokens,
        state,
    })
}

fn read_subagent_task(path: &Path) -> String {
    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    let reader = std::io::BufReader::new(file);

    for line in reader.lines().take(3).map_while(Result::ok) {
        if !line.contains("\"type\":\"user\"") {
            continue;
        }
        let Some(val) = parse_json_line(&line) else {
            continue;
        };
        if let Some(content) = val.pointer("/message/content") {
            if let Some(s) = content.as_str() {
                let trimmed = s.trim().lines().next().unwrap_or("").trim();
                let cleaned = trimmed
                    .trim_start_matches('#')
                    .trim_start_matches("Task:")
                    .trim_start_matches("## Task:")
                    .trim();
                return truncate_str(cleaned, 60);
            }
        }
    }
    String::new()
}

fn read_subagent_usage(file: &fs::File, file_len: u64) -> (String, u64) {
    let seek_pos = file_len.saturating_sub(super::RECENT_TAIL_BYTES);
    let Some(reader) = seek_tail(file, seek_pos) else {
        return (String::new(), 0);
    };

    let mut model = String::new();
    let mut tokens: u64 = 0;

    for line in reader.lines().map_while(Result::ok) {
        if !line.contains("\"type\":\"assistant\"") || line.contains("\"type\":\"progress\"") {
            continue;
        }
        if !line.contains("\"usage\"") {
            continue;
        }
        let Some(val) = parse_json_line(&line) else {
            continue;
        };
        if let Some(usage) = val.pointer("/message/usage") {
            let total = extract_tokens(usage);
            if total > 0 {
                tokens = total;
            }
        }
        if let Some(m) = val.pointer("/message/model").and_then(|v| v.as_str()) {
            model = shorten_model(m);
        }
    }

    (model, tokens)
}

pub(super) fn shorten_model(model: &str) -> String {
    if model.contains("opus") {
        "opus".to_string()
    } else if model.contains("sonnet") {
        "sonnet".to_string()
    } else if model.contains("haiku") {
        "haiku".to_string()
    } else {
        model.split('-').next().unwrap_or(model).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_model_opus() {
        assert_eq!(shorten_model("claude-opus-4-20250514"), "opus");
    }

    #[test]
    fn shorten_model_sonnet() {
        assert_eq!(shorten_model("claude-sonnet-4-20250514"), "sonnet");
    }

    #[test]
    fn shorten_model_haiku() {
        assert_eq!(shorten_model("claude-haiku-4-20250514"), "haiku");
    }

    #[test]
    fn shorten_model_unknown() {
        assert_eq!(shorten_model("gpt-4o-2025"), "gpt");
    }

    #[test]
    fn shorten_model_no_dash() {
        assert_eq!(shorten_model("custom"), "custom");
    }
}
