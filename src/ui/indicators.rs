use crate::data::HealthStatus;
use crate::ui::common::DIM;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

// ── viewmodel ────────────────────────────────────────────────

struct StatusLineVM {
    net_color: Color,
    disk_color: Color,
    health_icon: &'static str,
    health_color: Color,
}

fn activity_color(active: bool) -> Color {
    if active {
        Color::Cyan
    } else {
        DIM
    }
}

fn health_display(health: HealthStatus) -> (&'static str, Color) {
    match health {
        HealthStatus::Ok => ("\u{f012f}", Color::Green),
        HealthStatus::Slow => ("\u{f0028}", Color::Yellow),
        HealthStatus::Error => ("\u{f0028}", Color::Red),
    }
}

impl StatusLineVM {
    fn new(net_active: bool, disk_active: bool, health: HealthStatus) -> Self {
        let (health_icon, health_color) = health_display(health);
        Self {
            net_color: activity_color(net_active),
            disk_color: activity_color(disk_active),
            health_icon,
            health_color,
        }
    }
}

// ── render ───────────────────────────────────────────────────

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

        let vm = StatusLineVM::new(self.net_active, self.disk_active, self.health);

        let status_area = Rect::new(area.x + area.width.saturating_sub(5), area.y, 5, 1);
        let line = Paragraph::new(Line::from(vec![
            Span::styled("\u{f059f}", Style::default().fg(vm.net_color)),
            Span::raw(" "),
            Span::styled("\u{f01bc}", Style::default().fg(vm.disk_color)),
            Span::raw(" "),
            Span::styled(vm.health_icon, Style::default().fg(vm.health_color)),
        ]))
        .alignment(Alignment::Right);
        frame.render_widget(line, status_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── activity_color ───────────────────────────────────────

    #[test]
    fn activity_active_is_cyan() {
        assert_eq!(activity_color(true), Color::Cyan);
    }

    #[test]
    fn activity_inactive_is_dim() {
        assert_eq!(activity_color(false), DIM);
    }

    // ── health_display ───────────────────────────────────────

    #[test]
    fn health_ok() {
        let (icon, color) = health_display(HealthStatus::Ok);
        assert_eq!(icon, "\u{f012f}");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn health_slow() {
        let (icon, color) = health_display(HealthStatus::Slow);
        assert_eq!(icon, "\u{f0028}");
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn health_error() {
        let (_, color) = health_display(HealthStatus::Error);
        assert_eq!(color, Color::Red);
    }

    // ── StatusLineVM ─────────────────────────────────────────

    #[test]
    fn status_line_vm_all_active() {
        let vm = StatusLineVM::new(true, true, HealthStatus::Ok);
        assert_eq!(vm.net_color, Color::Cyan);
        assert_eq!(vm.disk_color, Color::Cyan);
        assert_eq!(vm.health_color, Color::Green);
    }

    #[test]
    fn status_line_vm_all_inactive() {
        let vm = StatusLineVM::new(false, false, HealthStatus::Error);
        assert_eq!(vm.net_color, DIM);
        assert_eq!(vm.disk_color, DIM);
        assert_eq!(vm.health_color, Color::Red);
    }
}
