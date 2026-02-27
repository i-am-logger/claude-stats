#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use super::activity::detect_state_from_tail;
use super::tail::{extract_tokens, is_assistant_usage, parse_json_line, seek_tail};
use super::{ModelShort, SubagentData};
use crate::fmt::truncate_str;
use std::fs;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Subagent files older than 30s are considered finished and excluded from the
/// active subagent list (half of `MAX_AGE_SECS` for the parent session).
const SUBAGENT_MAX_AGE_SECS: u64 = 30;

pub fn scan_subagents(subagents_dir: &Path) -> Vec<SubagentData> {
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
                prefix in "[a-z\\-]{0,20}",
                suffix in "[a-z0-9\\-]{0,20}",
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
