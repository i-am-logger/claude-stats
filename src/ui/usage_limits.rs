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

// ── viewmodel ────────────────────────────────────────────────

/// When a limit is at 100% and resets in fewer than 1800s (30 min), the timer
/// colour changes from red to yellow to indicate the reset is imminent.
const RESET_SOON_SECS: i64 = 1800;

fn usage_height(usage: Option<&UsageData>, rate_limited: bool) -> u16 {
    if let Some(data) = usage {
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
    } else if rate_limited {
        // title + countdown line
        2
    } else {
        1
    }
}

fn timer_color(percent: u16, remaining_secs: i64) -> Color {
    if percent >= 100 {
        if remaining_secs > RESET_SOON_SECS {
            Color::Red
        } else {
            Color::Yellow
        }
    } else {
        DIM
    }
}

fn no_data_message(fetching: bool, error: Option<&FetchError>) -> (String, Color) {
    if fetching {
        ("Loading usage data...".to_string(), DIM)
    } else if let Some(err) = error {
        (format!("Error: {err}"), Color::Red)
    } else {
        (String::new(), DIM)
    }
}

// ── render ───────────────────────────────────────────────────

pub(super) struct UsageLimitsSection<'a> {
    pub(super) usage: &'a Option<UsageData>,
    pub(super) error: &'a Option<FetchError>,
    pub(super) fetching: bool,
}

impl Section for UsageLimitsSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        let rate_limited = self
            .error
            .as_ref()
            .is_some_and(|e| matches!(e, FetchError::RateLimited { .. }));
        usage_height(self.usage.as_ref(), rate_limited)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let rate_limit_label = self.error.as_ref().and_then(FetchError::rate_limit_label);

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
                    render_limit(
                        title,
                        limit,
                        rate_limit_label.as_deref(),
                        frame,
                        &chunks,
                        &mut i,
                    );
                    i += 1; // spacer
                }
            }
        } else if let Some(label) = &rate_limit_label {
            // No data yet but rate limited — show title with countdown
            let constraints = vec![Constraint::Length(1), Constraint::Length(1)];
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(&constraints)
                .split(area);
            let title = Paragraph::new(Line::from(vec![
                Span::styled("◔ Current session", Style::default()),
                Span::styled(
                    format!(" — retrying in {label}"),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            if let Some(&area) = chunks.first() {
                frame.render_widget(title, padded(area));
            }
        } else {
            let (msg, color) = no_data_message(self.fetching, self.error.as_ref());
            let p = Paragraph::new(msg).style(Style::default().fg(color));
            frame.render_widget(p, padded(area));
        }
    }
}

fn render_limit(
    title: &str,
    limit: &UsageLimit,
    rate_limit_label: Option<&str>,
    frame: &mut Frame<'_>,
    chunks: &[Rect],
    i: &mut usize,
) {
    let percent = limit.percent();
    let color = percent_color(percent);

    let mut spans = vec![
        Span::styled(title, Style::default()),
        Span::styled(format!(" ({percent}%)"), Style::default().fg(color)),
    ];
    if let Some(label) = rate_limit_label {
        spans.push(Span::styled(
            format!(" — retrying in {label}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    let title_w = Paragraph::new(Line::from(spans));
    let Some(&area) = chunks.get(*i) else { return };
    frame.render_widget(title_w, padded(area));
    *i += 1;

    let Some(&area) = chunks.get(*i) else { return };
    render_bar(frame, padded(area), percent, color);
    *i += 1;

    if let Some(remaining) = limit.remaining_secs() {
        let tc = timer_color(percent, remaining);
        let label = limit.remaining_label();
        let timer = Paragraph::new(Line::from(Span::styled(
            format!("Resets in {label}"),
            Style::default().fg(tc),
        )));
        let Some(&area) = chunks.get(*i) else { return };
        frame.render_widget(timer, padded(area));
    }
    *i += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── usage_height ─────────────────────────────────────────

    #[test]
    fn usage_height_no_data() {
        assert_eq!(usage_height(None, false), 1);
    }

    #[test]
    fn usage_height_no_data_rate_limited() {
        assert_eq!(usage_height(None, true), 2);
    }

    #[test]
    fn usage_height_no_limits() {
        let data = UsageData {
            five_hour: None,
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
        };
        assert_eq!(usage_height(Some(&data), false), 0);
    }

    #[test]
    fn usage_height_one_limit() {
        let data = UsageData {
            five_hour: Some(UsageLimit {
                utilization: Some(50.0),
                resets_at: None,
            }),
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
        };
        assert_eq!(usage_height(Some(&data), false), 4);
    }

    #[test]
    fn usage_height_all_limits() {
        let limit = || {
            Some(UsageLimit {
                utilization: Some(50.0),
                resets_at: None,
            })
        };
        let data = UsageData {
            five_hour: limit(),
            seven_day: limit(),
            seven_day_opus: limit(),
            seven_day_sonnet: limit(),
        };
        assert_eq!(usage_height(Some(&data), false), 16);
    }

    // ── timer_color ──────────────────────────────────────────

    #[test]
    fn timer_color_under_100_is_dim() {
        assert_eq!(timer_color(50, 3600), DIM);
    }

    #[test]
    fn timer_color_at_100_long_reset() {
        assert_eq!(timer_color(100, 3600), Color::Red);
    }

    #[test]
    fn timer_color_at_100_soon_reset() {
        assert_eq!(timer_color(100, 1800), Color::Yellow);
    }

    #[test]
    fn timer_color_at_100_boundary() {
        assert_eq!(timer_color(100, 1801), Color::Red);
    }

    #[test]
    fn timer_color_over_100_soon() {
        assert_eq!(timer_color(150, 0), Color::Yellow);
    }

    // ── no_data_message ──────────────────────────────────────

    #[test]
    fn no_data_fetching() {
        let (msg, color) = no_data_message(true, None);
        assert_eq!(msg, "Loading usage data...");
        assert_eq!(color, DIM);
    }

    #[test]
    fn no_data_error() {
        let err = FetchError::Timeout;
        let (msg, color) = no_data_message(false, Some(&err));
        assert!(msg.starts_with("Error:"));
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn no_data_empty() {
        let (msg, color) = no_data_message(false, None);
        assert!(msg.is_empty());
        assert_eq!(color, DIM);
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_timer_color_deterministic(percent in 0..=200_u16, secs in 0..=7200_i64) {
                let c = timer_color(percent, secs);
                prop_assert!(c == Color::Red || c == Color::Yellow || c == DIM);
            }
        }
    }
}
