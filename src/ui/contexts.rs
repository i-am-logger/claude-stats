use crate::data::sessions::{SessionData, SessionState, SubagentData, CONTEXT_WINDOW};
use crate::ui::common::{indented, padded, percent_color, render_bar, Section, DIM, SPINNER};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

// ── viewmodel ────────────────────────────────────────────────

fn contexts_height(sessions: &[SessionData]) -> u16 {
    if sessions.is_empty() {
        return 0;
    }
    let mut h: u16 = 2; // header + blank
    for session in sessions {
        h = h.saturating_add(4); // title + bar + info + state
        h = h.saturating_add(session.agents.len() as u16);
        h = h.saturating_add(1); // spacer
    }
    h
}

fn session_indicator(state: SessionState, tick: u64) -> (String, Color) {
    match state {
        SessionState::Thinking | SessionState::Working => {
            let frame_idx = (tick as usize) % SPINNER.len();
            let color = match state {
                SessionState::Thinking => Color::Cyan,
                _ => Color::Green,
            };
            (SPINNER[frame_idx].to_string(), color)
        }
        SessionState::Idle => ("\u{25cb}".to_string(), DIM), // ○
    }
}

fn format_tokens_k(tokens: u64) -> String {
    format!("{:.2}k", tokens as f64 / 1000.0)
}

fn format_limit_k(limit: u64) -> String {
    format!("{:.0}k", limit as f64 / 1000.0)
}

fn format_agent_tokens_k(tokens: u64) -> String {
    format!("{:.1}k", tokens as f64 / 1000.0)
}

fn state_color(state: SessionState) -> Color {
    match state {
        SessionState::Thinking => Color::Cyan,
        SessionState::Working => Color::Green,
        SessionState::Idle => DIM,
    }
}

fn agent_state_display(state: SessionState) -> (&'static str, Color) {
    match state {
        SessionState::Thinking => ("thinking", Color::Cyan),
        SessionState::Working => ("working", Color::Green),
        SessionState::Idle => ("idle", DIM),
    }
}

fn tree_connector(is_last: bool) -> &'static str {
    if is_last {
        "\u{2514} " // └
    } else {
        "\u{251c} " // ├
    }
}

// ── render ───────────────────────────────────────────────────

pub(super) struct ContextsSection<'a> {
    pub(super) sessions: &'a [SessionData],
    pub(super) tick: u64,
}

impl Section for ContextsSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        contexts_height(self.sessions)
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
    let tokens_k = format_tokens_k(session.context_tokens);
    let limit_k = format_limit_k(CONTEXT_WINDOW);

    let (indicator, indicator_color) = session_indicator(session.state, tick);

    let row = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{indicator} "),
            Style::default().fg(indicator_color),
        ),
        Span::styled(&session.title, Style::default()),
        Span::styled(
            format!(" ({percent}% — {tokens_k}/{limit_k})"),
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
    let sc = state_color(session.state);
    let mut spans = Vec::new();
    if !session.activity.is_empty() {
        spans.push(Span::styled(&session.activity, Style::default().fg(sc)));
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
        let connector = tree_connector(idx + 1 == agents.len());
        let (a_state, a_color) = agent_state_display(agent.state);
        let tokens_k = format_agent_tokens_k(agent.context_tokens);
        let mut spans = vec![
            Span::styled(connector, Style::default().fg(DIM)),
            Span::styled(agent.model.to_string(), Style::default().fg(Color::Blue)),
            Span::styled(format!(" {tokens_k}"), Style::default().fg(DIM)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::sessions::ModelShort;

    fn make_session(agents: usize, state: SessionState) -> SessionData {
        SessionData {
            title: "test".into(),
            git_branch: "main".into(),
            context_tokens: 50_000,
            context_percent: 30,
            agents: (0..agents)
                .map(|_| SubagentData {
                    task: "test".into(),
                    model: ModelShort::Sonnet,
                    context_tokens: 5000,
                    state: SessionState::Working,
                })
                .collect(),
            compactions: 0,
            last_activity_label: "5s ago".into(),
            state,
            activity: "Bash(ls)".into(),
        }
    }

    // ── contexts_height ──────────────────────────────────────

    #[test]
    fn contexts_height_empty() {
        assert_eq!(contexts_height(&[]), 0);
    }

    #[test]
    fn contexts_height_one_session_no_agents() {
        // 2 (header+blank) + 4 (title+bar+info+state) + 0 agents + 1 spacer = 7
        assert_eq!(contexts_height(&[make_session(0, SessionState::Idle)]), 7);
    }

    #[test]
    fn contexts_height_one_session_with_agents() {
        // 2 + 4 + 2 + 1 = 9
        assert_eq!(
            contexts_height(&[make_session(2, SessionState::Working)]),
            9
        );
    }

    #[test]
    fn contexts_height_two_sessions() {
        let sessions = vec![
            make_session(0, SessionState::Idle),
            make_session(1, SessionState::Working),
        ];
        // 2 + (4+0+1) + (4+1+1) = 2 + 5 + 6 = 13
        assert_eq!(contexts_height(&sessions), 13);
    }

    // ── session_indicator ────────────────────────────────────

    #[test]
    fn session_indicator_idle() {
        let (icon, color) = session_indicator(SessionState::Idle, 0);
        assert_eq!(icon, "\u{25cb}");
        assert_eq!(color, DIM);
    }

    #[test]
    fn session_indicator_thinking() {
        let (icon, color) = session_indicator(SessionState::Thinking, 0);
        assert_eq!(icon, SPINNER[0].to_string());
        assert_eq!(color, Color::Cyan);
    }

    #[test]
    fn session_indicator_working() {
        let (_, color) = session_indicator(SessionState::Working, 0);
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn session_indicator_spinner_wraps() {
        let (icon1, _) = session_indicator(SessionState::Thinking, 0);
        let (icon2, _) = session_indicator(SessionState::Thinking, SPINNER.len() as u64);
        assert_eq!(icon1, icon2);
    }

    // ── format functions ─────────────────────────────────────

    #[test]
    fn format_tokens_k_zero() {
        assert_eq!(format_tokens_k(0), "0.00k");
    }

    #[test]
    fn format_tokens_k_round() {
        assert_eq!(format_tokens_k(50_000), "50.00k");
    }

    #[test]
    fn format_tokens_k_fractional() {
        assert_eq!(format_tokens_k(1_500), "1.50k");
    }

    #[test]
    fn format_limit_k_context_window() {
        assert_eq!(format_limit_k(CONTEXT_WINDOW), "166k");
    }

    #[test]
    fn format_agent_tokens_k_round() {
        assert_eq!(format_agent_tokens_k(5_000), "5.0k");
    }

    // ── state_color ──────────────────────────────────────────

    #[test]
    fn state_color_thinking() {
        assert_eq!(state_color(SessionState::Thinking), Color::Cyan);
    }

    #[test]
    fn state_color_working() {
        assert_eq!(state_color(SessionState::Working), Color::Green);
    }

    #[test]
    fn state_color_idle() {
        assert_eq!(state_color(SessionState::Idle), DIM);
    }

    // ── agent_state_display ──────────────────────────────────

    #[test]
    fn agent_state_thinking() {
        let (label, color) = agent_state_display(SessionState::Thinking);
        assert_eq!(label, "thinking");
        assert_eq!(color, Color::Cyan);
    }

    #[test]
    fn agent_state_working() {
        let (label, color) = agent_state_display(SessionState::Working);
        assert_eq!(label, "working");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn agent_state_idle() {
        let (label, color) = agent_state_display(SessionState::Idle);
        assert_eq!(label, "idle");
        assert_eq!(color, DIM);
    }

    // ── tree_connector ───────────────────────────────────────

    #[test]
    fn tree_connector_last() {
        assert_eq!(tree_connector(true), "\u{2514} ");
    }

    #[test]
    fn tree_connector_not_last() {
        assert_eq!(tree_connector(false), "\u{251c} ");
    }
}
