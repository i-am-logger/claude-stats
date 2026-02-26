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

pub(super) fn percent_color(percent: u16) -> Color {
    if percent >= 85 {
        Color::Red
    } else if percent >= 70 {
        Color::Yellow
    } else {
        Color::Reset
    }
}

pub(super) fn render_bar(frame: &mut Frame<'_>, area: Rect, percent: u16, color: Color) {
    let width = area.width as usize;
    let filled = if width > 0 {
        (percent.min(100) as usize * width) / 100
    } else {
        0
    };
    let mut spans = Vec::with_capacity(width);
    for j in 0..width {
        let c = if j < filled { color } else { GAUGE_BG };
        spans.push(Span::styled("▮", Style::default().fg(c)));
    }
    let bar = Paragraph::new(Line::from(spans));
    frame.render_widget(bar, area);
}
