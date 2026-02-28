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

// ── viewmodel ────────────────────────────────────────────────

struct IncidentVM {
    impact_color: Color,
}

struct StatusSectionVM {
    height: u16,
    indicator_color: Color,
    status_icon: &'static str,
    incidents: Vec<IncidentVM>,
}

fn indicator_color(indicator: StatusIndicator) -> Color {
    match indicator {
        StatusIndicator::None => Color::Green,
        StatusIndicator::Minor => Color::Yellow,
        StatusIndicator::Major | StatusIndicator::Critical => Color::Red,
        StatusIndicator::Unknown => DIM,
    }
}

fn impact_color(impact: IncidentImpact) -> Color {
    match impact {
        IncidentImpact::Minor | IncidentImpact::None => Color::Yellow,
        _ => Color::Red,
    }
}

fn status_icon(indicator: StatusIndicator) -> &'static str {
    if indicator == StatusIndicator::None {
        "\u{f012f}" // nf-md-checkbox_blank_circle
    } else {
        "\u{f0028}" // nf-md-alert_circle
    }
}

impl StatusSectionVM {
    fn new(status: Option<&StatusData>) -> Option<Self> {
        let sd = status?;
        let mut height: u16 = 2; // header + spacer
        if !sd.incidents.is_empty() {
            height += 1; // blank
            for incident in &sd.incidents {
                height += 1; // title
                if incident.latest_body().is_some() {
                    height += 1; // body
                }
                height += 1; // timing
            }
        }
        let incidents = sd
            .incidents
            .iter()
            .map(|inc| IncidentVM {
                impact_color: impact_color(inc.impact),
            })
            .collect();
        Some(Self {
            height,
            indicator_color: indicator_color(sd.summary.indicator),
            status_icon: status_icon(sd.summary.indicator),
            incidents,
        })
    }
}

// ── render ───────────────────────────────────────────────────

pub(super) struct StatusSection<'a> {
    pub(super) status: &'a Option<StatusData>,
}

impl Section for StatusSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        StatusSectionVM::new(self.status.as_ref()).map_or(0, |vm| vm.height)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(sd) = self.status else { return };
        let Some(vm) = StatusSectionVM::new(self.status.as_ref()) else {
            return;
        };

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

        let status_header = Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{} ", vm.status_icon),
                Style::default().fg(vm.indicator_color),
            ),
            Span::styled(
                "Claude Status",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" — {}", sd.summary.description),
                Style::default().fg(vm.indicator_color),
            ),
        ]));
        let Some(area) = chunks.get(i) else { return };
        frame.render_widget(status_header, padded(*area));
        i += 1;

        if !sd.incidents.is_empty() {
            i += 1; // blank
            for (idx, incident) in sd.incidents.iter().enumerate() {
                let inc_vm = &vm.incidents[idx];
                let status_label = incident.status.to_string();

                let row = Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!("[{status_label}] "),
                        Style::default()
                            .fg(inc_vm.impact_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(&incident.name, Style::default().fg(inc_vm.impact_color)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::incidents::{Incident, StatusSummary};

    fn dummy_status() -> StatusData {
        StatusData {
            summary: StatusSummary {
                indicator: StatusIndicator::None,
                description: "All Systems Operational".into(),
            },
            incidents: Vec::new(),
        }
    }

    fn dummy_incident(has_body: bool) -> Incident {
        let updates_json = if has_body {
            r#"[{"body":"Investigating the issue","created_at":"2025-01-01T00:00:00Z"}]"#
        } else {
            "[]"
        };
        serde_json::from_str(&format!(
            r#"{{
                "name":"Test incident",
                "status":"investigating",
                "impact":"minor",
                "started_at":"2025-01-01T00:00:00Z",
                "updated_at":"2025-01-01T00:00:00Z",
                "incident_updates":{updates_json}
            }}"#
        ))
        .unwrap()
    }

    // ── indicator_color ──────────────────────────────────────

    #[test]
    fn indicator_color_none_is_green() {
        assert_eq!(indicator_color(StatusIndicator::None), Color::Green);
    }

    #[test]
    fn indicator_color_minor_is_yellow() {
        assert_eq!(indicator_color(StatusIndicator::Minor), Color::Yellow);
    }

    #[test]
    fn indicator_color_major_is_red() {
        assert_eq!(indicator_color(StatusIndicator::Major), Color::Red);
    }

    #[test]
    fn indicator_color_critical_is_red() {
        assert_eq!(indicator_color(StatusIndicator::Critical), Color::Red);
    }

    #[test]
    fn indicator_color_unknown_is_dim() {
        assert_eq!(indicator_color(StatusIndicator::Unknown), DIM);
    }

    // ── impact_color ─────────────────────────────────────────

    #[test]
    fn impact_color_minor_is_yellow() {
        assert_eq!(impact_color(IncidentImpact::Minor), Color::Yellow);
    }

    #[test]
    fn impact_color_none_is_yellow() {
        assert_eq!(impact_color(IncidentImpact::None), Color::Yellow);
    }

    #[test]
    fn impact_color_major_is_red() {
        assert_eq!(impact_color(IncidentImpact::Major), Color::Red);
    }

    #[test]
    fn impact_color_critical_is_red() {
        assert_eq!(impact_color(IncidentImpact::Critical), Color::Red);
    }

    // ── status_icon ──────────────────────────────────────────

    #[test]
    fn status_icon_none_is_circle() {
        assert_eq!(status_icon(StatusIndicator::None), "\u{f012f}");
    }

    #[test]
    fn status_icon_minor_is_alert() {
        assert_eq!(status_icon(StatusIndicator::Minor), "\u{f0028}");
    }

    // ── StatusSectionVM ──────────────────────────────────────

    #[test]
    fn status_vm_none_returns_none() {
        assert!(StatusSectionVM::new(None).is_none());
    }

    #[test]
    fn status_vm_no_incidents_height_2() {
        let sd = dummy_status();
        let vm = StatusSectionVM::new(Some(&sd)).unwrap();
        assert_eq!(vm.height, 2);
        assert!(vm.incidents.is_empty());
    }

    #[test]
    fn status_vm_one_incident_no_body() {
        let mut sd = dummy_status();
        sd.incidents.push(dummy_incident(false));
        let vm = StatusSectionVM::new(Some(&sd)).unwrap();
        // 2 (base) + 1 (blank) + 1 (title) + 1 (timing) = 5
        assert_eq!(vm.height, 5);
        assert_eq!(vm.incidents.len(), 1);
    }

    #[test]
    fn status_vm_one_incident_with_body() {
        let mut sd = dummy_status();
        sd.incidents.push(dummy_incident(true));
        let vm = StatusSectionVM::new(Some(&sd)).unwrap();
        // 2 (base) + 1 (blank) + 1 (title) + 1 (body) + 1 (timing) = 6
        assert_eq!(vm.height, 6);
    }

    #[test]
    fn status_vm_indicator_color_propagated() {
        let mut sd = dummy_status();
        sd.summary.indicator = StatusIndicator::Major;
        let vm = StatusSectionVM::new(Some(&sd)).unwrap();
        assert_eq!(vm.indicator_color, Color::Red);
        assert_eq!(vm.status_icon, "\u{f0028}");
    }
}
