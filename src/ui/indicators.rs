use crate::data::HealthStatus;
use crate::ui::common::DIM;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub(super) struct StatusLine {
    pub(super) net_active: bool,
    pub(super) disk_active: bool,
    pub(super) health: HealthStatus,
}

impl StatusLine {
    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width < 5 {
            return;
        }

        let net_color = if self.net_active { Color::Cyan } else { DIM };
        let disk_color = if self.disk_active { Color::Cyan } else { DIM };
        let (health_icon, health_color) = match self.health {
            HealthStatus::Ok => ("\u{f012f}", Color::Green), // nf-md-checkbox_blank_circle
            HealthStatus::Slow => ("\u{f0028}", Color::Yellow), // nf-md-alert_circle
            HealthStatus::Error => ("\u{f0028}", Color::Red), // nf-md-alert_circle
        };

        let status_area = Rect::new(area.x + area.width.saturating_sub(5), area.y, 5, 1);
        let line = Paragraph::new(Line::from(vec![
            Span::styled("\u{f059f}", Style::default().fg(net_color)),
            Span::raw(" "),
            Span::styled("\u{f01bc}", Style::default().fg(disk_color)),
            Span::raw(" "),
            Span::styled(health_icon, Style::default().fg(health_color)),
        ]))
        .alignment(Alignment::Right);
        frame.render_widget(line, status_area);
    }
}
