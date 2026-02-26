use crate::ui::common::{padded, Section, DIM};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct HeaderSection<'a> {
    pub plan: &'a Option<crate::credentials::Plan>,
}

impl Section for HeaderSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        3
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        let mut spans = vec![
            Span::styled(
                "Claude Stats",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" v{VERSION}"), Style::default().fg(DIM)),
        ];
        if let Some(plan) = self.plan {
            spans.push(Span::styled(" — ", Style::default().fg(DIM)));
            spans.push(Span::styled(
                plan.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        let header = Paragraph::new(Line::from(spans));
        frame.render_widget(header, padded(chunks[1]));
    }
}
