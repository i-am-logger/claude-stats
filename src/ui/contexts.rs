use crate::data::sessions::{SessionData, SessionState, SubagentData, CONTEXT_WINDOW};
use crate::ui::common::{indented, padded, percent_color, render_bar, Section, DIM, SPINNER};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub(super) struct ContextsSection<'a> {
    pub(super) sessions: &'a [SessionData],
    pub(super) tick: u64,
}

impl Section for ContextsSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        if self.sessions.is_empty() {
            return 0;
        }
        let mut h: u16 = 2; // header + blank
        for session in self.sessions {
            h = h.saturating_add(4); // title + bar + info + state
            h = h.saturating_add(session.agents.len() as u16);
            h = h.saturating_add(1); // spacer
        }
        h
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.sessions.is_empty() {
            return;
        }

        let mut constraints = Vec::new();
        constraints.push(Constraint::Length(1)); // header
        constraints.push(Constraint::Length(1)); // blank
        for session in self.sessions {
            constraints.push(Constraint::Length(1)); // title
            constraints.push(Constraint::Length(1)); // bar
            constraints.push(Constraint::Length(1)); // info
            constraints.push(Constraint::Length(1)); // state
            for _ in &session.agents {
                constraints.push(Constraint::Length(1));
            }
            constraints.push(Constraint::Length(1)); // spacer
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(&constraints)
            .split(area);

        let mut i = 0;

        let Some(&header_area) = chunks.get(i) else {
            return;
        };
        let ctx_header = Paragraph::new(Line::from(Span::styled(
            format!("⊙ Active contexts ({})", self.sessions.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(ctx_header, padded(header_area));
        i += 2; // header + blank

        for session in self.sessions {
            let Some(rows) = chunks.get(i..i + 4) else {
                return;
            };
            render_title_row(session, self.tick, frame, rows[0]);
            render_context_bar(session, frame, rows[1]);
            render_info_row(session, frame, rows[2]);
            render_state_row(session, frame, rows[3]);
            i += 4;
            render_agents(&session.agents, frame, &chunks, &mut i);
            i += 1; // spacer
        }
    }
}

fn render_title_row(session: &SessionData, tick: u64, frame: &mut Frame<'_>, area: Rect) {
    let bar_color = percent_color(session.context_percent);
    let percent = session.context_percent;
    let tokens_k = session.context_tokens as f64 / 1000.0;
    let limit_k = CONTEXT_WINDOW as f64 / 1000.0;

    let (indicator, indicator_color) = match session.state {
        SessionState::Thinking | SessionState::Working => {
            let frame_idx = (tick as usize) % SPINNER.len();
            let color = match session.state {
                SessionState::Thinking => Color::Cyan,
                _ => Color::Green,
            };
            (SPINNER[frame_idx].to_string(), color)
        }
        SessionState::Idle => ("○".to_string(), DIM),
    };

    let row = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{indicator} "),
            Style::default().fg(indicator_color),
        ),
        Span::styled(&session.title, Style::default()),
        Span::styled(
            format!(" ({percent}% — {tokens_k:.2}k/{limit_k:.0}k)"),
            Style::default().fg(bar_color),
        ),
    ]));
    frame.render_widget(row, indented(area));
}

fn render_context_bar(session: &SessionData, frame: &mut Frame<'_>, area: Rect) {
    let bar_area = indented(area);
    render_bar(
        frame,
        bar_area,
        session.context_percent,
        percent_color(session.context_percent),
    );
}

fn render_info_row(session: &SessionData, frame: &mut Frame<'_>, area: Rect) {
    let mut spans = vec![
        Span::styled("⎇ ", Style::default().fg(DIM)),
        Span::styled(&session.git_branch, Style::default().fg(DIM)),
    ];
    if session.compactions > 0 {
        spans.push(Span::styled(
            format!("  {}x compacted", session.compactions),
            Style::default().fg(Color::Yellow),
        ));
    }
    spans.push(Span::styled(
        format!("  {}", &session.last_activity_label),
        Style::default().fg(DIM),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), indented(area));
}

fn render_state_row(session: &SessionData, frame: &mut Frame<'_>, area: Rect) {
    let state_color = match session.state {
        SessionState::Thinking => Color::Cyan,
        SessionState::Working => Color::Green,
        SessionState::Idle => DIM,
    };
    let mut spans = Vec::new();
    if !session.activity.is_empty() {
        spans.push(Span::styled(
            &session.activity,
            Style::default().fg(state_color),
        ));
    }
    if !session.agents.is_empty() {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", Style::default().fg(DIM)));
        }
        spans.push(Span::styled(
            format!("{} agents", session.agents.len()),
            Style::default().fg(Color::Magenta),
        ));
    }
    if !spans.is_empty() {
        frame.render_widget(Paragraph::new(Line::from(spans)), indented(area));
    }
}

fn agent_indent(r: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(6),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(r)[1]
}

fn render_agents(agents: &[SubagentData], frame: &mut Frame<'_>, chunks: &[Rect], i: &mut usize) {
    for (idx, agent) in agents.iter().enumerate() {
        let Some(&area) = chunks.get(*i) else {
            return;
        };
        let connector = if idx + 1 == agents.len() {
            "└ "
        } else {
            "├ "
        };
        let (a_state, a_color) = match agent.state {
            SessionState::Thinking => ("thinking", Color::Cyan),
            SessionState::Working => ("working", Color::Green),
            SessionState::Idle => ("idle", DIM),
        };
        let tokens_k = agent.context_tokens as f64 / 1000.0;
        let mut spans = vec![
            Span::styled(connector, Style::default().fg(DIM)),
            Span::styled(agent.model.to_string(), Style::default().fg(Color::Blue)),
            Span::styled(format!(" {tokens_k:.1}k"), Style::default().fg(DIM)),
            Span::styled(format!(" {a_state}"), Style::default().fg(a_color)),
        ];
        if !agent.task.is_empty() {
            spans.push(Span::styled(
                format!(" — {}", agent.task),
                Style::default().fg(DIM),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), agent_indent(area));
        *i += 1;
    }
}
