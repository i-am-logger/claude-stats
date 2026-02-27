use crate::data::claude_version::ClaudeVersion;
use crate::data::self_version::{self, SelfVersion};
use crate::ui::common::{padded, Section, DIM};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub(super) struct HeaderSection<'a> {
    pub(super) plan: &'a Option<crate::credentials::Plan>,
    pub(super) account_email: &'a Option<String>,
    pub(super) claude_version: &'a Option<ClaudeVersion>,
    pub(super) self_version: &'a Option<SelfVersion>,
}

impl HeaderSection<'_> {
    fn has_account_line(&self) -> bool {
        self.account_email.is_some() || self.plan.is_some()
    }

    fn has_version_line(&self) -> bool {
        self.claude_version.is_some()
    }
}

impl Section for HeaderSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        let mut h = 3u16; // top spacer + title + bottom spacer
        if self.has_account_line() {
            h += 1;
        }
        if self.has_version_line() {
            h += 1;
        }
        h
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let h = self.height(area.width);
        let constraints: Vec<Constraint> = (0..h).map(|_| Constraint::Length(1)).collect();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(&constraints)
            .split(area);

        let mut row = 1; // skip top spacer

        // Title: Claude Stats v{version} (X.Y.Z available)
        let mut title_spans = vec![
            Span::styled(
                "Claude Stats",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" v{}", self_version::CURRENT_VERSION),
                Style::default().fg(DIM),
            ),
        ];
        if let Some(sv) = self.self_version {
            if sv.is_outdated() {
                if let Some(latest) = &sv.latest {
                    title_spans.push(Span::styled(
                        format!(" ({latest} available)"),
                        Style::default().fg(Color::Cyan),
                    ));
                }
            }
        }
        let title = Line::from(title_spans);
        if let Some(&area) = chunks.get(row) {
            frame.render_widget(Paragraph::new(title), padded(area));
        }
        row += 1;

        // Account line (optional): email — Plan
        if self.has_account_line() {
            let mut spans = Vec::new();
            if let Some(email) = self.account_email {
                spans.push(Span::styled("\u{2013} ", Style::default().fg(DIM)));
                spans.push(Span::styled(email.clone(), Style::default()));
            }
            if let Some(plan) = self.plan {
                if !spans.is_empty() {
                    spans.push(Span::styled(" — ", Style::default().fg(DIM)));
                }
                spans.push(Span::styled(plan.to_string(), Style::default().fg(DIM)));
            }
            if let Some(&area) = chunks.get(row) {
                frame.render_widget(Paragraph::new(Line::from(spans)), padded(area));
            }
            row += 1;
        }

        // Version line (optional): Claude Code v{installed}
        if let Some(cv) = self.claude_version {
            let mut spans = Vec::new();
            if let Some(installed) = &cv.installed {
                spans.push(Span::styled("\u{2013} ", Style::default().fg(DIM)));
                spans.push(Span::styled("Claude Code", Style::default()));
                spans.push(Span::styled(
                    format!(" v{installed}"),
                    Style::default().fg(DIM),
                ));
                if cv.is_outdated() {
                    if let Some(latest) = &cv.latest {
                        spans.push(Span::styled(
                            format!(" ({latest} available)"),
                            Style::default().fg(Color::Cyan),
                        ));
                    }
                }
            } else {
                spans.push(Span::styled(
                    "Claude Code (not found)",
                    Style::default().fg(DIM),
                ));
            }
            if let Some(&area) = chunks.get(row) {
                frame.render_widget(Paragraph::new(Line::from(spans)), padded(area));
            }
        }
    }
}
