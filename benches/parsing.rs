#![allow(
    unused_crate_dependencies,
    reason = "bench binary inherits all workspace deps"
)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::io::{Seek, SeekFrom, Write};

use claude_stats::data::sessions::activity::detect_state_and_activity;
use claude_stats::data::sessions::tail::{
    extract_tokens, parse_json_line, scan_from_offset, scan_session_file,
};

fn assistant_usage_line(input_tokens: u64) -> String {
    format!(
        r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":{input_tokens}}},"content":[{{"type":"text","text":"ok"}}],"stop_reason":"end_turn"}}}}"#
    )
}

fn user_line() -> &'static str {
    r#"{"type":"user","cwd":"/home/user/project","gitBranch":"main","message":{"content":"hello"}}"#
}

fn tool_use_line() -> &'static str {
    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}],"stop_reason":null}}"#
}

fn progress_line() -> &'static str {
    r#"{"type":"progress","data":{"type":"bash_progress","output":"running..."}}"#
}

fn make_session_file(line_count: usize) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    writeln!(f, "{}", user_line()).unwrap();
    for i in 0..line_count.saturating_sub(1) {
        writeln!(f, "{}", assistant_usage_line((i as u64 + 1) * 1000)).unwrap();
    }
    f.as_file().sync_all().unwrap();
    f
}

fn bench_parse_json_line(c: &mut Criterion) {
    let line = assistant_usage_line(50_000);
    c.bench_function("parse_json_line", |b| {
        b.iter(|| parse_json_line(black_box(&line)));
    });
}

fn bench_extract_tokens(c: &mut Criterion) {
    let usage = serde_json::json!({
        "input_tokens": 100,
        "cache_creation_input_tokens": 50,
        "cache_read_input_tokens": 25
    });
    c.bench_function("extract_tokens", |b| {
        b.iter(|| extract_tokens(black_box(&usage)));
    });
}

fn bench_scan_session_file(c: &mut Criterion) {
    let mut group = c.benchmark_group("scan_session_file");
    for size in [10, 100, 500, 1000] {
        let f = make_session_file(size);
        group.bench_function(format!("{size}_lines"), |b| {
            b.iter(|| {
                f.as_file().seek(SeekFrom::Start(0)).unwrap();
                scan_session_file(black_box(f.as_file()));
            });
        });
    }
    group.finish();
}

fn bench_scan_from_offset(c: &mut Criterion) {
    let f = make_session_file(500);
    let file_len = f.as_file().metadata().unwrap().len();
    let mid_offset = file_len / 2;
    c.bench_function("scan_from_offset_mid", |b| {
        b.iter(|| {
            scan_from_offset(black_box(f.as_file()), black_box(mid_offset));
        });
    });
}

fn bench_detect_state_and_activity(c: &mut Criterion) {
    let lines = vec![
        user_line().to_string(),
        assistant_usage_line(50_000),
        tool_use_line().to_string(),
        progress_line().to_string(),
    ];
    c.bench_function("detect_state_and_activity", |b| {
        b.iter(|| detect_state_and_activity(black_box(&lines)));
    });
}

criterion_group!(
    benches,
    bench_parse_json_line,
    bench_extract_tokens,
    bench_scan_session_file,
    bench_scan_from_offset,
    bench_detect_state_and_activity,
);
criterion_main!(benches);
