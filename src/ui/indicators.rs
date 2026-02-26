use crate::data::HealthStatus;
use crate::ui::common::DIM;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub(super) struct IndicatorDots {
    pub(super) fetching: bool,
    pub(super) health: Option<HealthStatus>,
}

impl IndicatorDots {
    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width < 4 {
            return;
        }

        let activity_color = if self.fetching { Color::Cyan } else { DIM };
        let health_color = match self.health {
            Some(HealthStatus::Ok) => Color::Green,
            Some(HealthStatus::Slow) => Color::Yellow,
            Some(HealthStatus::Error) => Color::Red,
            None => DIM,
        };
        let dot_area = Rect::new(area.x + area.width.saturating_sub(4), area.y, 4, 1);
        let dots = Paragraph::new(Line::from(vec![
            Span::styled("●", Style::default().fg(activity_color)),
            Span::raw(" "),
            Span::styled("●", Style::default().fg(health_color)),
        ]))
        .alignment(Alignment::Right);
        frame.render_widget(dots, dot_area);
    }
}
