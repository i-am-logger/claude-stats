use crate::data::usage::{UsageData, UsageLimit};
use crate::error::FetchError;
use crate::ui::common::{padded, percent_color, render_bar, Section, DIM};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// When a limit is at 100% and resets in fewer than 1800s (30 min), the timer
/// colour changes from red to yellow to indicate the reset is imminent.
const RESET_SOON_SECS: i64 = 1800;

pub struct UsageLimitsSection<'a> {
    pub usage: &'a Option<UsageData>,
    pub error: &'a Option<FetchError>,
    pub fetching: bool,
}

impl Section for UsageLimitsSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        if let Some(ref data) = self.usage {
            let mut h = 0u16;
            if data.five_hour.is_some() {
                h += 4;
            }
            if data.seven_day.is_some() {
                h += 4;
            }
            if data.seven_day_opus.is_some() {
                h += 4;
            }
            if data.seven_day_sonnet.is_some() {
                h += 4;
            }
            h
        } else {
            1
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        if let Some(ref data) = self.usage {
            let limits: Vec<(&str, &Option<UsageLimit>)> = vec![
                ("◔ Current session", &data.five_hour),
                ("◈ All models", &data.seven_day),
                ("◆ Opus only", &data.seven_day_opus),
                ("◇ Sonnet only", &data.seven_day_sonnet),
            ];

            let mut constraints = Vec::new();
            for (_, limit) in &limits {
                if limit.is_some() {
                    constraints.push(Constraint::Length(1)); // title
                    constraints.push(Constraint::Length(1)); // gauge
                    constraints.push(Constraint::Length(1)); // reset
                    constraints.push(Constraint::Length(1)); // spacer
                }
            }

            if constraints.is_empty() {
                return;
            }

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(&constraints)
                .split(area);

            let mut i = 0;
            for (title, limit_opt) in &limits {
                if let Some(ref limit) = limit_opt {
                    render_limit(title, limit, frame, &chunks, &mut i);
                    i += 1; // spacer
                }
            }
        } else {
            let msg = if self.fetching {
                "Loading usage data...".to_string()
            } else if let Some(ref err) = self.error {
                format!("Error: {err}")
            } else {
                String::new()
            };
            let color = if self.error.is_some() {
                Color::Red
            } else {
                DIM
            };
            let p = Paragraph::new(msg).style(Style::default().fg(color));
            frame.render_widget(p, padded(area));
        }
    }
}

fn render_limit(
    title: &str,
    limit: &UsageLimit,
    frame: &mut Frame,
    chunks: &[Rect],
    i: &mut usize,
) {
    let percent = limit.percent();

    let color = percent_color(percent);

    let title_w = Paragraph::new(Line::from(vec![
        Span::styled(title, Style::default()),
        Span::styled(format!(" ({percent}%)"), Style::default().fg(color)),
    ]));
    frame.render_widget(title_w, padded(chunks[*i]));
    *i += 1;

    let bar_area = padded(chunks[*i]);
    render_bar(frame, bar_area, percent, color);
    *i += 1;

    if let Some(remaining) = limit.remaining_secs() {
        let timer_color = if percent >= 100 {
            if remaining > RESET_SOON_SECS {
                Color::Red
            } else {
                Color::Yellow
            }
        } else {
            DIM
        };
        let label = limit.remaining_label();
        let timer = Paragraph::new(Line::from(Span::styled(
            format!("Resets in {label}"),
            Style::default().fg(timer_color),
        )));
        frame.render_widget(timer, padded(chunks[*i]));
    }
    *i += 1;
}
