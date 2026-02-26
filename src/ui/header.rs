use crate::data::claude_version::ClaudeVersion;
use crate::ui::common::{padded, Section, DIM};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(super) struct HeaderSection<'a> {
    pub(super) plan: &'a Option<crate::credentials::Plan>,
    pub(super) account_email: &'a Option<String>,
    pub(super) claude_version: &'a Option<ClaudeVersion>,
}

impl Section for HeaderSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        5
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        // Line 1: Claude Stats v{version}
        let line1 = Line::from(vec![
            Span::styled(
                "Claude Stats",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" v{VERSION}"), Style::default().fg(DIM)),
        ]);

        // Line 2: email — Plan
        let mut account_spans = Vec::new();
        if let Some(email) = self.account_email {
            account_spans.push(Span::styled("\u{2013} ", Style::default().fg(DIM)));
            account_spans.push(Span::styled(email.clone(), Style::default()));
        }
        if let Some(plan) = self.plan {
            if !account_spans.is_empty() {
                account_spans.push(Span::styled(" — ", Style::default().fg(DIM)));
            }
            account_spans.push(Span::styled(plan.to_string(), Style::default().fg(DIM)));
        }
        let line2 = Line::from(account_spans);

        // Line 3: Claude Code v{installed} (update available)
        let mut version_spans = Vec::new();
        if let Some(cv) = self.claude_version {
            if let Some(installed) = &cv.installed {
                version_spans.push(Span::styled("\u{2013} ", Style::default().fg(DIM)));
                version_spans.push(Span::styled("Claude Code", Style::default()));
                version_spans.push(Span::styled(
                    format!(" v{installed}"),
                    Style::default().fg(DIM),
                ));
                if cv.is_outdated() {
                    if let Some(latest) = &cv.latest {
                        version_spans.push(Span::styled(
                            format!(" ({latest} available)"),
                            Style::default().fg(Color::Red),
                        ));
                    }
                }
            } else {
                version_spans.push(Span::styled(
                    "Claude Code (not found)",
                    Style::default().fg(DIM),
                ));
            }
        }
        let line3 = Line::from(version_spans);

        if let Some(&area) = chunks.get(1) {
            frame.render_widget(Paragraph::new(line1), padded(area));
        }
        if let Some(&area) = chunks.get(2) {
            frame.render_widget(Paragraph::new(line2), padded(area));
        }
        if let Some(&area) = chunks.get(3) {
            frame.render_widget(Paragraph::new(line3), padded(area));
        }
    }
}
