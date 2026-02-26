mod common;
mod contexts;
mod header;
mod indicators;
mod status_section;
mod usage_limits;

use crate::app::State;
use common::Section;
use contexts::ContextsSection;
use header::HeaderSection;
use indicators::IndicatorDots;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    Frame,
};
use status_section::StatusSection;
use usage_limits::UsageLimitsSection;

pub(crate) fn render(state: &State, frame: &mut Frame<'_>) {
    let area = frame.area();

    let header = HeaderSection {
        plan: &state.plan,
        account_email: &state.account_email,
        claude_version: &state.claude_version,
    };
    let status = StatusSection {
        status: &state.status,
    };
    let contexts = ContextsSection {
        sessions: &state.sessions,
        tick: state.tick,
    };
    let limits = UsageLimitsSection {
        usage: &state.usage,
        error: &state.error,
        fetching: state.fetching,
    };

    let sections: Vec<&dyn Section> = vec![&header, &status, &contexts, &limits];
    let mut constraints: Vec<Constraint> = sections
        .iter()
        .map(|s| Constraint::Length(s.height(area.width)))
        .collect();
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(&constraints)
        .split(area);

    for (idx, section) in sections.iter().enumerate() {
        section.render(frame, chunks[idx]);
    }

    // Overlay indicator dots last
    let dots = IndicatorDots {
        fetching: state.fetching,
        health: state.health,
    };
    dots.render(frame, area);
}
