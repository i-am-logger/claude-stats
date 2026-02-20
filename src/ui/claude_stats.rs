use crate::data::usage::{UsageData, UsageLimit};
use crate::data::HealthStatus;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

const DIM: Color = Color::DarkGray;
const GAUGE_BG: Color = Color::DarkGray;

pub struct ClaudeStats<'a> {
    pub usage: &'a Option<UsageData>,
    pub error: &'a Option<String>,
    pub health: Option<HealthStatus>,
    pub working: bool,
    pub plan: &'a Option<String>,
}

impl<'a> ClaudeStats<'a> {
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match self.usage {
            Some(data) => render_gauges(data, self.plan, frame, area),
            None if self.working => {
                let p = Paragraph::new("Loading usage data...").style(Style::default().fg(DIM));
                frame.render_widget(p, area);
            }
            None => {
                if let Some(ref err) = self.error {
                    let p = Paragraph::new(format!("Error: {}", err))
                        .style(Style::default().fg(Color::Red));
                    frame.render_widget(p, area);
                }
            }
        }

        // Two dots — top right
        // Left: activity — gray at rest, cyan while fetching
        // Right: health — green/yellow/red, always shows last result
        if area.width >= 4 {
            let activity_color = if self.working { Color::Cyan } else { DIM };
            let health_color = match self.health {
                Some(HealthStatus::Ok) => Color::Green,
                Some(HealthStatus::Slow) => Color::Yellow,
                Some(HealthStatus::Error) => Color::Red,
                None => DIM,
            };
            let dot_area = Rect::new(area.width - 4, 0, 4, 1);
            let dots = Paragraph::new(Line::from(vec![
                Span::styled("●", Style::default().fg(activity_color)),
                Span::raw(" "),
                Span::styled("●", Style::default().fg(health_color)),
            ]))
            .alignment(ratatui::layout::Alignment::Right);
            frame.render_widget(dots, dot_area);
        }
    }
}

fn render_gauges(data: &UsageData, plan: &Option<String>, frame: &mut Frame, area: Rect) {
    let mut constraints: Vec<Constraint> = Vec::new();

    // Header
    constraints.push(Constraint::Length(1)); // blank
    constraints.push(Constraint::Length(1)); // "Plan usage limits"
    constraints.push(Constraint::Length(1)); // blank

    if data.five_hour.is_some() {
        constraints.push(Constraint::Length(1)); // title
        constraints.push(Constraint::Length(1)); // gauge
        constraints.push(Constraint::Length(1)); // reset time
        constraints.push(Constraint::Length(1)); // spacer
    }

    if data.seven_day.is_some() {
        constraints.push(Constraint::Length(1)); // title
        constraints.push(Constraint::Length(1)); // gauge
        constraints.push(Constraint::Length(1)); // reset time
        constraints.push(Constraint::Length(1)); // spacer
    }

    if data.seven_day_opus.is_some() {
        constraints.push(Constraint::Length(1)); // title
        constraints.push(Constraint::Length(1)); // gauge
        constraints.push(Constraint::Length(1)); // reset time
        constraints.push(Constraint::Length(1)); // spacer
    }

    if data.seven_day_sonnet.is_some() {
        constraints.push(Constraint::Length(1)); // title
        constraints.push(Constraint::Length(1)); // gauge
        constraints.push(Constraint::Length(1)); // reset time
    }

    constraints.push(Constraint::Min(0)); // fill

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(&constraints)
        .split(area);

    let padded = |r: Rect| -> Rect {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(0),
                Constraint::Length(2),
            ])
            .split(r)[1]
    };

    let mut i = 0;

    // Header
    i += 1; // blank
    let header_text = match plan {
        Some(name) => format!("{} — usage limits", name),
        None => "Plan usage limits".to_string(),
    };
    let header = Paragraph::new(Line::from(Span::styled(
        header_text,
        Style::default().add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(header, padded(chunks[i]));
    i += 2; // header + blank

    if let Some(ref limit) = data.five_hour {
        render_limit(
            "◔ Current session",
            limit,
            true,
            frame,
            &chunks,
            &padded,
            &mut i,
        );
        i += 1; // spacer
    }

    if let Some(ref limit) = data.seven_day {
        render_limit("◈ All models", limit, true, frame, &chunks, &padded, &mut i);
        i += 1; // spacer
    }

    if let Some(ref limit) = data.seven_day_opus {
        render_limit("◆ Opus only", limit, true, frame, &chunks, &padded, &mut i);
        i += 1; // spacer
    }

    if let Some(ref limit) = data.seven_day_sonnet {
        render_limit(
            "◇ Sonnet only",
            limit,
            true,
            frame,
            &chunks,
            &padded,
            &mut i,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_limit(
    title: &str,
    limit: &UsageLimit,
    show_countdown: bool,
    frame: &mut Frame,
    chunks: &[Rect],
    padded: &dyn Fn(Rect) -> Rect,
    i: &mut usize,
) {
    let percent = limit.percent();

    // Title with percentage
    let color = if percent >= 85 {
        Color::Red
    } else if percent >= 70 {
        Color::Yellow
    } else {
        Color::Reset
    };
    let title_w = Paragraph::new(Line::from(vec![
        Span::styled(title, Style::default()),
        Span::styled(format!(" ({}%)", percent), Style::default().fg(color)),
    ]));
    frame.render_widget(title_w, padded(chunks[*i]));
    *i += 1;

    // Segmented usage bar
    let bar_area = padded(chunks[*i]);
    let width = bar_area.width as usize;
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
    frame.render_widget(bar, bar_area);
    *i += 1;

    // Reset timer
    if show_countdown {
        if let Some(remaining) = limit.remaining_secs() {
            let timer_color = if percent >= 100 {
                if remaining > 1800 {
                    Color::Red
                } else {
                    Color::Yellow
                }
            } else {
                DIM
            };
            let label = limit.remaining_label();
            let timer = Paragraph::new(Line::from(Span::styled(
                format!("Resets in {}", label),
                Style::default().fg(timer_color),
            )));
            frame.render_widget(timer, padded(chunks[*i]));
        }
        *i += 1;
    }
}
