use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub(super) const DIM: Color = Color::DarkGray;
pub(super) const GAUGE_BG: Color = Color::DarkGray;
pub(super) const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub(super) trait Section {
    fn height(&self, width: u16) -> u16;
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
}

pub(super) fn percent_color(percent: u16) -> Color {
    if percent >= 85 {
        Color::Red
    } else if percent >= 70 {
        Color::Yellow
    } else {
        Color::Reset
    }
}

pub(super) fn bar_filled_width(percent: u16, total_width: usize) -> usize {
    if total_width > 0 {
        (percent.min(100) as usize * total_width) / 100
    } else {
        0
    }
}

pub(super) fn padded(r: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(r)[1]
}

pub(super) fn indented(r: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(r)[1]
}

pub(super) fn render_bar(frame: &mut Frame<'_>, area: Rect, percent: u16, color: Color) {
    let width = area.width as usize;
    let filled = bar_filled_width(percent, width);
    let mut spans = Vec::with_capacity(width);
    for j in 0..width {
        let c = if j < filled { color } else { GAUGE_BG };
        spans.push(Span::styled("▮", Style::default().fg(c)));
    }
    let bar = Paragraph::new(Line::from(spans));
    frame.render_widget(bar, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── percent_color ────────────────────────────────────────

    #[test]
    fn percent_color_zero() {
        assert_eq!(percent_color(0), Color::Reset);
    }

    #[test]
    fn percent_color_at_69() {
        assert_eq!(percent_color(69), Color::Reset);
    }

    #[test]
    fn percent_color_at_70() {
        assert_eq!(percent_color(70), Color::Yellow);
    }

    #[test]
    fn percent_color_at_84() {
        assert_eq!(percent_color(84), Color::Yellow);
    }

    #[test]
    fn percent_color_at_85() {
        assert_eq!(percent_color(85), Color::Red);
    }

    #[test]
    fn percent_color_at_100() {
        assert_eq!(percent_color(100), Color::Red);
    }

    // ── bar_filled_width ─────────────────────────────────────

    #[test]
    fn bar_zero_width() {
        assert_eq!(bar_filled_width(50, 0), 0);
    }

    #[test]
    fn bar_zero_percent() {
        assert_eq!(bar_filled_width(0, 20), 0);
    }

    #[test]
    fn bar_full_percent() {
        assert_eq!(bar_filled_width(100, 20), 20);
    }

    #[test]
    fn bar_over_100_clamped() {
        assert_eq!(bar_filled_width(150, 20), 20);
    }

    #[test]
    fn bar_50_percent() {
        assert_eq!(bar_filled_width(50, 20), 10);
    }

    #[test]
    fn bar_rounding() {
        assert_eq!(bar_filled_width(33, 10), 3);
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_bar_filled_never_exceeds_width(percent in 0..=u16::MAX, width in 0..=500_usize) {
                let filled = bar_filled_width(percent, width);
                prop_assert!(filled <= width);
            }

            #[test]
            fn prop_percent_color_always_valid(percent in 0..=u16::MAX) {
                let c = percent_color(percent);
                prop_assert!(c == Color::Red || c == Color::Yellow || c == Color::Reset);
            }
        }
    }
}
