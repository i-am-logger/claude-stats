use crate::data::sessions::{PhaseProgress, SessionData, SessionState, SubagentData};
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
        return 3; // header + blank + "no active contexts"
    }
    let mut h: u16 = 2; // header + blank
    for session in sessions {
        h = h.saturating_add(3); // title + bar + info
        if has_state_row(session) {
            h = h.saturating_add(1);
        }
        h = h.saturating_add(agent_row_count(&session.agents) as u16);
        h = h.saturating_add(1); // spacer
    }
    h
}

/// A named teammate (not a workflow-run aggregate row) sitting idle — the
/// same rows Claude Code's own picker collapses once there are more than a
/// few of them.
fn is_idle_teammate(agent: &SubagentData) -> bool {
    agent.progress.is_none() && agent.name.is_some() && agent.state == SessionState::Idle
}

/// Beyond this many idle teammates, collapse them into a single "N idle"
/// summary row instead of listing each one — mirrors Claude Code's roster,
/// which does the same once idle teammates stop fitting comfortably.
const IDLE_TEAMMATE_COLLAPSE_THRESHOLD: usize = 3;

/// How many roster rows `agents` renders as, after idle-teammate collapsing.
fn agent_row_count(agents: &[SubagentData]) -> usize {
    let idle_teammates = agents.iter().filter(|a| is_idle_teammate(a)).count();
    if idle_teammates > IDLE_TEAMMATE_COLLAPSE_THRESHOLD {
        agents.len() - idle_teammates + 1
    } else {
        agents.len()
    }
}

enum AgentRow<'a> {
    Single(&'a SubagentData),
    CollapsedIdle(usize),
}

/// Plan the roster rows for `agents`: every agent as its own row, unless
/// idle teammates outnumber `IDLE_TEAMMATE_COLLAPSE_THRESHOLD`, in which case
/// they're pulled out into one trailing `CollapsedIdle` row.
fn plan_agent_rows(agents: &[SubagentData]) -> Vec<AgentRow<'_>> {
    let idle_teammates = agents.iter().filter(|a| is_idle_teammate(a)).count();
    if idle_teammates <= IDLE_TEAMMATE_COLLAPSE_THRESHOLD {
        return agents.iter().map(AgentRow::Single).collect();
    }
    let mut rows: Vec<AgentRow<'_>> = agents
        .iter()
        .filter(|a| !is_idle_teammate(a))
        .map(AgentRow::Single)
        .collect();
    rows.push(AgentRow::CollapsedIdle(idle_teammates));
    rows
}

/// Whether the session gets a state row. An empty state row would render as
/// a second blank line, making inter-session gaps uneven.
fn has_state_row(session: &SessionData) -> bool {
    !session.activity.is_empty() || !session.agents.is_empty()
}

/// Roster summary distinguishing plain agents from aggregated workflow
/// runs: "2 agents", "1 workflow", "1 agent · 2 workflows".
fn format_roster_count(agents: &[SubagentData]) -> String {
    let workflows = agents.iter().filter(|a| a.progress.is_some()).count();
    let plain = agents.len() - workflows;
    let mut parts = Vec::new();
    if plain > 0 {
        parts.push(format!(
            "{plain} agent{}",
            if plain == 1 { "" } else { "s" }
        ));
    }
    if workflows > 0 {
        parts.push(format!(
            "{workflows} workflow{}",
            if workflows == 1 { "" } else { "s" }
        ));
    }
    parts.join(" · ")
}

/// The phase a workflow run is "on" right now, for a compact single-line
/// indicator: the first not-fully-done phase in declaration order, or the
/// last phase if every phase is done. Returns `(1-based index, total phases,
/// phase)`. `None` for runs with no declared phases.
fn current_phase(phases: &[PhaseProgress]) -> Option<(usize, usize, &PhaseProgress)> {
    let total = phases.len();
    // Not-fully-done: either no agent dispatched into it yet (`total == 0`,
    // vacuously "done < total" would be false and wrongly skip it) or some
    // dispatched agent hasn't finished.
    let position = phases
        .iter()
        .position(|p| p.total == 0 || p.done < p.total)
        .unwrap_or_else(|| total.saturating_sub(1));
    phases.get(position).map(|p| (position + 1, total, p))
}

/// Compact two-unit runtime: "42s", "12m 42s", "1h 4m", "2d 1h".
fn format_runtime(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86_400,
        (secs % 86_400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// A session whose file has gone quiet far longer than any real model turn
/// can't honestly claim `Working`/`Thinking` — its process likely died or
/// hung. Same threshold and reasoning as `agent_state_display` already uses
/// for subagent rows, applied here to the top-level session row too.
fn session_stale(session: &SessionData) -> bool {
    session.last_write_age_secs > AGENT_STALE_DISPLAY_SECS
        && matches!(
            session.state,
            SessionState::Working | SessionState::Thinking
        )
}

fn session_indicator(state: SessionState, is_stale: bool, tick: u64) -> (String, Color) {
    if is_stale {
        return ("\u{25cb}".to_string(), DIM); // ○ — can't honestly claim active
    }
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
    if limit >= 1_000_000 && limit.is_multiple_of(1_000_000) {
        format!("{}M", limit / 1_000_000)
    } else {
        format!("{:.0}k", limit as f64 / 1000.0)
    }
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

/// An agent whose transcript has been quiet this long can't honestly claim
/// active work — render "stale" instead of "working"/"thinking". It stays
/// listed (it might be mid-long-model-turn) until the liveness rules drop it.
const AGENT_STALE_DISPLAY_SECS: u64 = 300;

fn agent_state_display(state: SessionState, last_write_age_secs: u64) -> (&'static str, Color) {
    if last_write_age_secs > AGENT_STALE_DISPLAY_SECS
        && matches!(state, SessionState::Working | SessionState::Thinking)
    {
        return ("stale", DIM);
    }
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
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // header
                    Constraint::Length(1), // blank
                    Constraint::Length(1), // message
                ])
                .split(area);

            let header = Paragraph::new(Line::from(Span::styled(
                "⊙ Active contexts (0)",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            frame.render_widget(header, padded(chunks[0]));

            let msg = Paragraph::new(Line::from(Span::styled(
                "No active contexts",
                Style::default().fg(DIM),
            )));
            frame.render_widget(msg, indented(chunks[2]));
            return;
        }

        let mut constraints = Vec::new();
        constraints.push(Constraint::Length(1)); // header
        constraints.push(Constraint::Length(1)); // blank
        for session in self.sessions {
            constraints.push(Constraint::Length(1)); // title
            constraints.push(Constraint::Length(1)); // bar
            constraints.push(Constraint::Length(1)); // info
            if has_state_row(session) {
                constraints.push(Constraint::Length(1)); // state
            }
            for _ in 0..agent_row_count(&session.agents) {
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
            let Some(rows) = chunks.get(i..i + 3) else {
                return;
            };
            render_title_row(session, self.tick, frame, rows[0]);
            render_context_bar(session, frame, rows[1]);
            render_info_row(session, frame, rows[2]);
            i += 3;
            if has_state_row(session) {
                if let Some(&state_area) = chunks.get(i) {
                    render_state_row(session, frame, state_area);
                }
                i += 1;
            }
            render_agents(&session.agents, frame, &chunks, &mut i);
            i += 1; // spacer
        }
    }
}

fn render_title_row(session: &SessionData, tick: u64, frame: &mut Frame<'_>, area: Rect) {
    let bar_color = percent_color(session.context_percent);
    let percent = session.context_percent;
    let tokens_k = format_tokens_k(session.context_tokens);
    let limit_k = format_limit_k(session.context_window);

    let (indicator, indicator_color) =
        session_indicator(session.state, session_stale(session), tick);

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
        format!("  {}", session.last_activity_label),
        Style::default().fg(DIM),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), indented(area));
}

fn render_state_row(session: &SessionData, frame: &mut Frame<'_>, area: Rect) {
    let stale = session_stale(session);
    let sc = if stale {
        DIM
    } else {
        state_color(session.state)
    };
    let mut spans = Vec::new();
    if !session.activity.is_empty() {
        // Keep the last-known activity text (still informative — what it
        // was doing before going quiet) but mark it as possibly outdated,
        // same as agent_state_display does for subagent rows.
        let text = if stale {
            format!("{} (stale)", session.activity)
        } else {
            session.activity.clone()
        };
        spans.push(Span::styled(text, Style::default().fg(sc)));
        if let Some(runtime) = session.turn_runtime_secs {
            spans.push(Span::styled(
                format!(" · {}", format_runtime(runtime)),
                Style::default().fg(DIM),
            ));
        }
    }
    if !session.agents.is_empty() {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", Style::default().fg(DIM)));
        }
        spans.push(Span::styled(
            format_roster_count(&session.agents),
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
    let rows = plan_agent_rows(agents);
    let total = rows.len();
    for (idx, row) in rows.into_iter().enumerate() {
        let Some(&area) = chunks.get(*i) else {
            return;
        };
        let is_last = idx + 1 == total;
        match row {
            AgentRow::Single(agent) => render_agent_row(agent, is_last, frame, area),
            AgentRow::CollapsedIdle(count) => {
                render_collapsed_idle_row(count, is_last, frame, area);
            }
        }
        *i += 1;
    }
}

fn render_collapsed_idle_row(count: usize, is_last: bool, frame: &mut Frame<'_>, area: Rect) {
    let connector = tree_connector(is_last);
    let line = Line::from(vec![
        Span::styled(connector, Style::default().fg(DIM)),
        Span::styled(format!("{count} idle"), Style::default().fg(DIM)),
    ]);
    frame.render_widget(Paragraph::new(line), agent_indent(area));
}

fn render_agent_row(agent: &SubagentData, is_last: bool, frame: &mut Frame<'_>, area: Rect) {
    {
        let connector = tree_connector(is_last);
        let mut spans = vec![Span::styled(connector, Style::default().fg(DIM))];
        if let Some(name) = &agent.name {
            spans.push(Span::styled(
                format!("{name} "),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if let Some((done, total)) = agent.progress {
            // Aggregated workflow run — mirrors Claude Code's roster entry.
            let (_, a_color) = agent_state_display(agent.state, agent.last_write_age_secs);
            let current = current_phase(&agent.phases);
            if let Some((index, phase_total, phase)) = current {
                spans.push(Span::styled(
                    format!("Phase {index}/{phase_total}: {} ", phase.title),
                    Style::default().fg(DIM),
                ));
            }
            spans.push(Span::styled(
                format!("{done}/{total} agents done"),
                Style::default().fg(a_color),
            ));
            if !agent.task.is_empty() {
                spans.push(Span::styled(
                    format!(" — {}", agent.task),
                    Style::default().fg(Color::Red),
                ));
            } else if let Some(tool) = current.and_then(|(_, _, phase)| phase.current_tool.as_ref())
            {
                spans.push(Span::styled(format!(" — {tool}"), Style::default().fg(DIM)));
            }
        } else {
            let (a_state, a_color) = agent_state_display(agent.state, agent.last_write_age_secs);
            spans.push(Span::styled(
                agent.model.to_string(),
                Style::default().fg(Color::Blue),
            ));
            spans.push(Span::styled(
                format!(" {a_state}"),
                Style::default().fg(a_color),
            ));
            if !agent.task.is_empty() {
                spans.push(Span::styled(
                    format!(" — {}", agent.task),
                    Style::default().fg(DIM),
                ));
            }
        }
        if let Some(runtime) = agent.runtime_secs {
            spans.push(Span::styled(
                format!(" · {}", format_runtime(runtime)),
                Style::default().fg(DIM),
            ));
        }
        if agent.context_tokens > 0 {
            spans.push(Span::styled(
                format!(" · \u{2193}{}", format_agent_tokens_k(agent.context_tokens)),
                Style::default().fg(DIM),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), agent_indent(area));
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
            context_window: 200_000,
            context_percent: 30,
            agents: (0..agents)
                .map(|_| SubagentData {
                    task: "test".into(),
                    name: None,
                    model: ModelShort::Sonnet,
                    context_tokens: 5000,
                    runtime_secs: Some(90),
                    last_write_age_secs: 0,
                    state: SessionState::Working,
                    progress: None,
                    phases: Vec::new(),
                })
                .collect(),
            compactions: 0,
            last_activity_label: "5s ago".into(),
            state,
            activity: "Bash(ls)".into(),
            turn_runtime_secs: None,
            last_write_age_secs: 0,
        }
    }

    // ── contexts_height ──────────────────────────────────────

    #[test]
    fn contexts_height_empty() {
        assert_eq!(contexts_height(&[]), 3);
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
    fn contexts_height_idle_session_without_activity_drops_state_row() {
        let mut session = make_session(0, SessionState::Idle);
        session.activity = String::new();
        // 2 (header+blank) + 3 (title+bar+info) + 0 agents + 1 spacer = 6
        assert_eq!(contexts_height(&[session]), 6);
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

    #[test]
    fn contexts_height_collapses_idle_teammates() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = (0..4).map(|i| idle_teammate(&format!("t{i}"))).collect();
        // 2 (header+blank) + 4 (title+bar+info+state) + 1 collapsed row + 1 spacer = 8
        assert_eq!(contexts_height(&[session]), 8);
    }

    // ── session_indicator ────────────────────────────────────

    #[test]
    fn session_indicator_idle() {
        let (icon, color) = session_indicator(SessionState::Idle, false, 0);
        assert_eq!(icon, "\u{25cb}");
        assert_eq!(color, DIM);
    }

    #[test]
    fn session_indicator_thinking() {
        let (icon, color) = session_indicator(SessionState::Thinking, false, 0);
        assert_eq!(icon, SPINNER[0].to_string());
        assert_eq!(color, Color::Cyan);
    }

    #[test]
    fn session_indicator_working() {
        let (_, color) = session_indicator(SessionState::Working, false, 0);
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn session_indicator_spinner_wraps() {
        let (icon1, _) = session_indicator(SessionState::Thinking, false, 0);
        let (icon2, _) = session_indicator(SessionState::Thinking, false, SPINNER.len() as u64);
        assert_eq!(icon1, icon2);
    }

    #[test]
    fn session_indicator_stale_shows_idle_circle() {
        let (icon, color) = session_indicator(SessionState::Working, true, 0);
        assert_eq!(icon, "\u{25cb}");
        assert_eq!(color, DIM);
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
    fn format_limit_k_1m() {
        assert_eq!(format_limit_k(1_000_000), "1M");
    }

    #[test]
    fn format_limit_k_200k() {
        assert_eq!(format_limit_k(200_000), "200k");
    }

    #[test]
    fn format_agent_tokens_k_round() {
        assert_eq!(format_agent_tokens_k(5_000), "5.0k");
    }

    #[test]
    fn format_runtime_seconds() {
        assert_eq!(format_runtime(42), "42s");
    }

    #[test]
    fn format_runtime_minutes() {
        assert_eq!(format_runtime(762), "12m 42s");
    }

    #[test]
    fn format_runtime_hours() {
        assert_eq!(format_runtime(3840), "1h 4m");
    }

    #[test]
    fn format_runtime_days() {
        assert_eq!(format_runtime(2 * 86_400 + 3600 + 59), "2d 1h");
    }

    fn agent_row(progress: Option<(u32, u32)>) -> SubagentData {
        SubagentData {
            task: "test".into(),
            name: None,
            model: ModelShort::Sonnet,
            context_tokens: 5000,
            runtime_secs: Some(90),
            last_write_age_secs: 0,
            state: SessionState::Working,
            progress,
            phases: Vec::new(),
        }
    }

    fn idle_teammate(name: &str) -> SubagentData {
        SubagentData {
            task: String::new(),
            name: Some(name.into()),
            model: ModelShort::Sonnet,
            context_tokens: 0,
            runtime_secs: None,
            last_write_age_secs: 0,
            state: SessionState::Idle,
            progress: None,
            phases: Vec::new(),
        }
    }

    // ── idle-teammate collapsing ─────────────────────────────

    #[test]
    fn agent_row_count_no_collapse_at_threshold() {
        let agents: Vec<_> = (0..3).map(|i| idle_teammate(&format!("t{i}"))).collect();
        assert_eq!(agent_row_count(&agents), 3);
    }

    #[test]
    fn agent_row_count_collapses_past_threshold() {
        let agents: Vec<_> = (0..4).map(|i| idle_teammate(&format!("t{i}"))).collect();
        assert_eq!(agent_row_count(&agents), 1);
    }

    #[test]
    fn agent_row_count_collapse_keeps_non_idle_rows() {
        let mut agents: Vec<_> = (0..4).map(|i| idle_teammate(&format!("t{i}"))).collect();
        agents.push(agent_row(None)); // Working, no name — not an idle teammate
        assert_eq!(agent_row_count(&agents), 2); // 1 collapsed + 1 kept
    }

    #[test]
    fn agent_row_count_workflow_rows_never_collapse() {
        // progress.is_some() excludes workflow-aggregate rows even when
        // named and terminal (Idle).
        let agents: Vec<_> = (0..5)
            .map(|i| SubagentData {
                progress: Some((2, 2)),
                ..idle_teammate(&format!("wf{i}"))
            })
            .collect();
        assert_eq!(agent_row_count(&agents), 5);
    }

    fn phase(title: &str, done: u32, total: u32) -> PhaseProgress {
        PhaseProgress {
            title: title.into(),
            done,
            total,
            current_tool: None,
        }
    }

    #[test]
    fn roster_count_agents_only() {
        assert_eq!(format_roster_count(&[agent_row(None)]), "1 agent");
        assert_eq!(
            format_roster_count(&[agent_row(None), agent_row(None)]),
            "2 agents"
        );
    }

    #[test]
    fn roster_count_workflows_only() {
        assert_eq!(
            format_roster_count(&[agent_row(Some((0, 2))), agent_row(Some((1, 4)))]),
            "2 workflows"
        );
        assert_eq!(
            format_roster_count(&[agent_row(Some((0, 2)))]),
            "1 workflow"
        );
    }

    #[test]
    fn roster_count_mixed() {
        assert_eq!(
            format_roster_count(&[
                agent_row(None),
                agent_row(Some((0, 2))),
                agent_row(Some((1, 4)))
            ]),
            "1 agent · 2 workflows"
        );
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
        let (label, color) = agent_state_display(SessionState::Thinking, 0);
        assert_eq!(label, "thinking");
        assert_eq!(color, Color::Cyan);
    }

    #[test]
    fn agent_state_working() {
        let (label, color) = agent_state_display(SessionState::Working, 0);
        assert_eq!(label, "working");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn agent_state_idle() {
        let (label, color) = agent_state_display(SessionState::Idle, 0);
        assert_eq!(label, "idle");
        assert_eq!(color, DIM);
    }

    #[test]
    fn agent_state_working_but_quiet_shows_stale() {
        let (label, color) =
            agent_state_display(SessionState::Working, AGENT_STALE_DISPLAY_SECS + 1);
        assert_eq!(label, "stale");
        assert_eq!(color, DIM);
    }

    #[test]
    fn agent_state_idle_never_stale() {
        let (label, _) = agent_state_display(SessionState::Idle, AGENT_STALE_DISPLAY_SECS + 1);
        assert_eq!(label, "idle");
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

    // ── render (TestBackend) ─────────────────────────────────────

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render the section into a test terminal and return the screen text.
    fn render_to_text(sessions: &[SessionData], width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let section = ContextsSection { sessions, tick: 0 };
                let area = frame.area();
                section.render(frame, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn render_empty_shows_no_active_contexts() {
        let text = render_to_text(&[], 60, 5);
        assert!(text.contains("Active contexts (0)"));
        assert!(text.contains("No active contexts"));
    }

    #[test]
    fn render_session_title_bar_and_info() {
        let session = make_session(0, SessionState::Working);
        let text = render_to_text(&[session], 80, 12);
        assert!(text.contains("Active contexts (1)"));
        assert!(text.contains("test (30% — 50.00k/200k)"));
        assert!(text.contains("⎇ main"));
        assert!(text.contains("5s ago"));
        assert!(text.contains("Bash(ls)"));
    }

    #[test]
    fn render_idle_session_indicator() {
        let session = make_session(0, SessionState::Idle);
        let text = render_to_text(&[session], 80, 12);
        assert!(text.contains("○ test"));
    }

    #[test]
    fn render_compactions_shown_when_present() {
        let mut session = make_session(0, SessionState::Idle);
        session.compactions = 3;
        let text = render_to_text(&[session], 80, 12);
        assert!(text.contains("3x compacted"));
    }

    #[test]
    fn render_turn_runtime_shown_next_to_activity() {
        let mut session = make_session(0, SessionState::Working);
        session.turn_runtime_secs = Some(762);
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("Bash(ls) · 12m 42s"));
    }

    #[test]
    fn render_no_turn_runtime_when_idle() {
        let mut session = make_session(0, SessionState::Idle);
        session.turn_runtime_secs = Some(762);
        session.activity = String::new();
        let text = render_to_text(&[session], 100, 12);
        assert!(!text.contains("12m 42s"));
    }

    #[test]
    fn render_agent_row_with_name_runtime_and_tokens() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = vec![SubagentData {
            task: "Fix things".into(),
            name: Some("my-fixer".into()),
            model: ModelShort::Opus,
            context_tokens: 5_000,
            runtime_secs: Some(762),
            last_write_age_secs: 0,
            state: SessionState::Working,
            progress: None,
            phases: Vec::new(),
        }];
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("1 agent"));
        assert!(text.contains("└ my-fixer opus working — Fix things · 12m 42s · ↓5.0k"));
    }

    #[test]
    fn render_workflow_row_with_progress() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = vec![SubagentData {
            task: String::new(),
            name: Some("my-flow".into()),
            model: ModelShort::Unknown,
            context_tokens: 305_800,
            runtime_secs: Some(2518),
            last_write_age_secs: 0,
            state: SessionState::Working,
            progress: Some((1, 2)),
            phases: Vec::new(),
        }];
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("1 workflow"));
        assert!(text.contains("└ my-flow 1/2 agents done · 41m 58s · ↓305.8k"));
    }

    #[test]
    fn render_workflow_row_failed_state_shown() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = vec![SubagentData {
            task: "failed".into(),
            name: Some("my-flow".into()),
            model: ModelShort::Unknown,
            context_tokens: 1_000,
            runtime_secs: Some(60),
            last_write_age_secs: 0,
            state: SessionState::Idle,
            progress: Some((1, 2)),
            phases: Vec::new(),
        }];
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("1/2 agents done — failed"));
    }

    #[test]
    fn render_workflow_row_shows_current_agent_tool() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = vec![SubagentData {
            task: String::new(),
            name: Some("my-flow".into()),
            model: ModelShort::Unknown,
            context_tokens: 1_000,
            runtime_secs: Some(60),
            last_write_age_secs: 0,
            state: SessionState::Working,
            progress: Some((0, 1)),
            phases: vec![PhaseProgress {
                title: "Build".into(),
                done: 0,
                total: 1,
                current_tool: Some("cargo test --workspace".into()),
            }],
        }];
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("0/1 agents done — cargo test --workspace"));
    }

    #[test]
    fn render_workflow_row_with_phase_indicator() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = vec![SubagentData {
            task: String::new(),
            name: Some("my-flow".into()),
            model: ModelShort::Unknown,
            context_tokens: 305_800,
            runtime_secs: Some(2518),
            last_write_age_secs: 0,
            state: SessionState::Working,
            progress: Some((1, 2)),
            phases: vec![
                phase("Unified design", 1, 1),
                phase("Adversarial review", 0, 1),
            ],
        }];
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("Phase 2/2: Adversarial review 1/2 agents done"));
    }

    #[test]
    fn render_workflow_row_phase_not_yet_started_shows_first_phase() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = vec![SubagentData {
            task: String::new(),
            name: Some("my-flow".into()),
            model: ModelShort::Unknown,
            context_tokens: 0,
            runtime_secs: Some(5),
            last_write_age_secs: 0,
            state: SessionState::Working,
            progress: Some((0, 0)),
            phases: vec![phase("Find", 0, 0), phase("Verify", 0, 0)],
        }];
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("Phase 1/2: Find 0/0 agents done"));
    }

    #[test]
    fn render_stale_agent_state() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = vec![SubagentData {
            task: "Quiet work".into(),
            name: None,
            model: ModelShort::Sonnet,
            context_tokens: 1_000,
            runtime_secs: None,
            last_write_age_secs: AGENT_STALE_DISPLAY_SECS + 1,
            state: SessionState::Working,
            progress: None,
            phases: Vec::new(),
        }];
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("sonnet stale — Quiet work"));
        // No runtime span when the start time is unknown
        assert!(!text.contains(" · ↓1.0k · "));
    }

    #[test]
    fn render_stale_session_marked_and_shows_idle_circle() {
        let mut session = make_session(0, SessionState::Working);
        session.activity = "Bash(cargo build)".into();
        session.last_write_age_secs = AGENT_STALE_DISPLAY_SECS + 1;
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("Bash(cargo build) (stale)"));
        assert!(text.contains("○ test"));
    }

    #[test]
    fn render_fresh_working_session_not_marked_stale() {
        let mut session = make_session(0, SessionState::Working);
        session.activity = "Bash(cargo build)".into();
        session.last_write_age_secs = 5;
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("Bash(cargo build)"));
        assert!(!text.contains("(stale)"));
    }

    #[test]
    fn render_multiple_agents_use_tree_connectors() {
        let session = make_session(2, SessionState::Working);
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("├ sonnet working — test"));
        assert!(text.contains("└ sonnet working — test"));
        assert!(text.contains("2 agents"));
    }

    #[test]
    fn render_idle_teammates_not_collapsed_at_threshold() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = (0..3)
            .map(|i| idle_teammate(&format!("teammate-{i}")))
            .collect();
        let text = render_to_text(&[session], 100, 14);
        assert!(text.contains("teammate-0"));
        assert!(text.contains("teammate-1"));
        assert!(text.contains("teammate-2"));
    }

    #[test]
    fn render_idle_teammates_collapse_past_threshold() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = (0..4)
            .map(|i| idle_teammate(&format!("teammate-{i}")))
            .collect();
        let text = render_to_text(&[session], 100, 14);
        assert!(text.contains("\u{2514} 4 idle")); // last row: "└ 4 idle"
        assert!(!text.contains("teammate-0"));
        assert!(!text.contains("teammate-3"));
    }

    #[test]
    fn render_collapse_keeps_non_idle_rows_visible() {
        let mut session = make_session(0, SessionState::Working);
        let mut agents: Vec<_> = (0..4)
            .map(|i| idle_teammate(&format!("teammate-{i}")))
            .collect();
        agents.push(SubagentData {
            task: "Fix things".into(),
            name: Some("active-one".into()),
            model: ModelShort::Opus,
            context_tokens: 1_000,
            runtime_secs: Some(60),
            last_write_age_secs: 0,
            state: SessionState::Working,
            progress: None,
            phases: Vec::new(),
        });
        session.agents = agents;
        let text = render_to_text(&[session], 100, 14);
        assert!(text.contains("active-one"));
        assert!(text.contains("4 idle"));
    }

    #[test]
    fn render_idle_session_without_activity_has_no_state_row() {
        let mut idle = make_session(0, SessionState::Idle);
        idle.activity = String::new();
        let working = make_session(0, SessionState::Working);
        let text = render_to_text(&[idle, working], 80, 16);
        // Both sessions render; the idle one contributes no activity line
        assert_eq!(text.matches("⎇ main").count(), 2);
        assert_eq!(text.matches("Bash(ls)").count(), 1);
    }

    #[test]
    fn render_zero_token_agent_omits_token_span() {
        let mut session = make_session(0, SessionState::Working);
        session.agents = vec![SubagentData {
            task: "Just started".into(),
            name: None,
            model: ModelShort::Unknown,
            context_tokens: 0,
            runtime_secs: Some(5),
            last_write_age_secs: 0,
            state: SessionState::Working,
            progress: None,
            phases: Vec::new(),
        }];
        let text = render_to_text(&[session], 100, 12);
        assert!(text.contains("? working — Just started · 5s"));
        assert!(!text.contains("↓0.0k"));
    }
}
