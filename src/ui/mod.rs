pub mod claude_stats;

use crate::data::usage::UsageData;
use crate::data::HealthStatus;
use claude_stats::ClaudeStats;
use ratatui::Frame;

pub fn render(
    usage: &Option<UsageData>,
    error: &Option<String>,
    health: Option<HealthStatus>,
    working: bool,
    plan: &Option<String>,
    frame: &mut Frame,
) {
    let stats = ClaudeStats {
        usage,
        error,
        health,
        working,
        plan,
    };
    stats.render(frame, frame.area());
}
