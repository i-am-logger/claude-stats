use crate::data::incidents::{IncidentImpact, StatusData, StatusIndicator};
use crate::fmt::truncate_str;
use crate::ui::common::{indented, padded, Section, DIM};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub(super) struct StatusSection<'a> {
    pub(super) status: &'a Option<StatusData>,
}

fn indicator_color(indicator: StatusIndicator) -> Color {
    match indicator {
        StatusIndicator::None => Color::Green,
        StatusIndicator::Minor => Color::Yellow,
        StatusIndicator::Major | StatusIndicator::Critical => Color::Red,
        StatusIndicator::Unknown => DIM,
    }
}

impl Section for StatusSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        let Some(sd) = self.status.as_ref() else {
            return 0;
        };
        let mut h: u16 = 2; // header + spacer
        if !sd.incidents.is_empty() {
            h += 1; // blank
            for incident in &sd.incidents {
                h += 1; // title
                if incident.latest_body().is_some() {
                    h += 1; // body
                }
                h += 1; // timing
            }
        }
        h
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(sd) = self.status else { return };

        let mut constraints = vec![Constraint::Length(1)]; // header
        if !sd.incidents.is_empty() {
            constraints.push(Constraint::Length(1)); // blank
            for incident in &sd.incidents {
                constraints.push(Constraint::Length(1)); // title
                if incident.latest_body().is_some() {
                    constraints.push(Constraint::Length(1)); // body
                }
                constraints.push(Constraint::Length(1)); // timing
            }
        }
        constraints.push(Constraint::Length(1)); // spacer

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(&constraints)
            .split(area);

        let mut i = 0;

        let color = indicator_color(sd.summary.indicator);
        let icon = if sd.summary.indicator == StatusIndicator::None {
            "\u{f012f}" // nf-md-checkbox_blank_circle
        } else {
            "\u{f0028}" // nf-md-alert_circle
        };
        let status_header = Paragraph::new(Line::from(vec![
            Span::styled(format!("{icon} "), Style::default().fg(color)),
            Span::styled(
                "Claude Status",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" — {}", sd.summary.description),
                Style::default().fg(color),
            ),
        ]));
        let Some(area) = chunks.get(i) else { return };
        frame.render_widget(status_header, padded(*area));
        i += 1;

        if !sd.incidents.is_empty() {
            i += 1; // blank
            for incident in &sd.incidents {
                let status_label = incident.status.to_string();
                let impact_color = match incident.impact {
                    IncidentImpact::Minor | IncidentImpact::None => Color::Yellow,
                    _ => Color::Red,
                };

                let row = Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!("[{status_label}] "),
                        Style::default()
                            .fg(impact_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(&incident.name, Style::default().fg(impact_color)),
                ]));
                let Some(area) = chunks.get(i) else { return };
                frame.render_widget(row, indented(*area));
                i += 1;

                if let Some(body) = incident.latest_body() {
                    let Some(area) = chunks.get(i) else { return };
                    let body_area = indented(*area);
                    let clean = body.replace('\n', " ");
                    let truncated = truncate_str(&clean, body_area.width as usize);
                    let body_row = Paragraph::new(Line::from(Span::styled(
                        truncated,
                        Style::default().fg(DIM),
                    )));
                    frame.render_widget(body_row, body_area);
                    i += 1;
                }

                let timing = Paragraph::new(Line::from(Span::styled(
                    format!(
                        "Started {} · Updated {}",
                        incident.started_ago(),
                        incident.updated_ago()
                    ),
                    Style::default().fg(DIM),
                )));
                let Some(area) = chunks.get(i) else { return };
                frame.render_widget(timing, indented(*area));
                i += 1;
            }
        }
    }
}
