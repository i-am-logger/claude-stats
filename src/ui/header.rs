use crate::ui::common::{padded, Section};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

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

        let header_text = match self.plan {
            Some(name) => format!("{name} — usage limits"),
            None => "Plan usage limits".to_string(),
        };
        let header = Paragraph::new(Line::from(Span::styled(
            header_text,
            Style::default().add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(header, padded(chunks[1]));
    }
}
