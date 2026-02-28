use crate::credentials::Plan;
use crate::data::claude_version::ClaudeVersion;
use crate::data::self_version::{self, SelfVersion};
use crate::ui::common::{padded, Section, DIM};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

// ── viewmodel ────────────────────────────────────────────────

struct HeaderVM {
    height: u16,
    has_account_line: bool,
    has_version_line: bool,
    self_update_text: Option<String>,
    claude_update_text: Option<String>,
    claude_installed: Option<String>,
}

fn self_update_text(self_version: Option<&SelfVersion>) -> Option<String> {
    let sv = self_version?;
    if sv.is_outdated() {
        sv.latest.as_ref().map(|v| format!("({v} available)"))
    } else {
        None
    }
}

fn claude_update_text(claude_version: Option<&ClaudeVersion>) -> Option<String> {
    let cv = claude_version?;
    if cv.is_outdated() {
        cv.latest.as_ref().map(|v| format!("({v} available)"))
    } else {
        None
    }
}

impl HeaderVM {
    fn new(
        plan: Option<&Plan>,
        email: Option<&String>,
        claude_version: Option<&ClaudeVersion>,
        self_version: Option<&SelfVersion>,
    ) -> Self {
        let has_account_line = email.is_some() || plan.is_some();
        let has_version_line = claude_version.is_some();
        let mut height = 3u16; // top spacer + title + bottom spacer
        if has_account_line {
            height += 1;
        }
        if has_version_line {
            height += 1;
        }
        let claude_installed = claude_version.and_then(|cv| cv.installed.clone());
        Self {
            height,
            has_account_line,
            has_version_line,
            self_update_text: self_update_text(self_version),
            claude_update_text: claude_update_text(claude_version),
            claude_installed,
        }
    }
}

// ── render ───────────────────────────────────────────────────

pub(super) struct HeaderSection<'a> {
    pub(super) plan: &'a Option<Plan>,
    pub(super) account_email: &'a Option<String>,
    pub(super) claude_version: &'a Option<ClaudeVersion>,
    pub(super) self_version: &'a Option<SelfVersion>,
}

impl Section for HeaderSection<'_> {
    fn height(&self, _width: u16) -> u16 {
        let vm = HeaderVM::new(
            self.plan.as_ref(),
            self.account_email.as_ref(),
            self.claude_version.as_ref(),
            self.self_version.as_ref(),
        );
        vm.height
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let vm = HeaderVM::new(
            self.plan.as_ref(),
            self.account_email.as_ref(),
            self.claude_version.as_ref(),
            self.self_version.as_ref(),
        );
        let constraints: Vec<Constraint> = (0..vm.height).map(|_| Constraint::Length(1)).collect();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(&constraints)
            .split(area);

        let mut row = 1; // skip top spacer

        // Title: Claude Stats v{version} (X.Y.Z available)
        let mut title_spans = vec![
            Span::styled(
                "Claude Stats",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" v{}", self_version::CURRENT_VERSION),
                Style::default().fg(DIM),
            ),
        ];
        if let Some(update_text) = &vm.self_update_text {
            title_spans.push(Span::styled(
                format!(" {update_text}"),
                Style::default().fg(Color::Cyan),
            ));
        }
        let title = Line::from(title_spans);
        if let Some(&area) = chunks.get(row) {
            frame.render_widget(Paragraph::new(title), padded(area));
        }
        row += 1;

        // Account line (optional): email — Plan
        if vm.has_account_line {
            let mut spans = Vec::new();
            if let Some(email) = self.account_email {
                spans.push(Span::styled("\u{2013} ", Style::default().fg(DIM)));
                spans.push(Span::styled(email.clone(), Style::default()));
            }
            if let Some(plan) = self.plan {
                if !spans.is_empty() {
                    spans.push(Span::styled(" — ", Style::default().fg(DIM)));
                }
                spans.push(Span::styled(plan.to_string(), Style::default().fg(DIM)));
            }
            if let Some(&area) = chunks.get(row) {
                frame.render_widget(Paragraph::new(Line::from(spans)), padded(area));
            }
            row += 1;
        }

        // Version line (optional): Claude Code v{installed}
        if vm.has_version_line {
            let mut spans = Vec::new();
            if let Some(installed) = &vm.claude_installed {
                spans.push(Span::styled("\u{2013} ", Style::default().fg(DIM)));
                spans.push(Span::styled("Claude Code", Style::default()));
                spans.push(Span::styled(
                    format!(" v{installed}"),
                    Style::default().fg(DIM),
                ));
                if let Some(update_text) = &vm.claude_update_text {
                    spans.push(Span::styled(
                        format!(" {update_text}"),
                        Style::default().fg(Color::Cyan),
                    ));
                }
            } else {
                spans.push(Span::styled(
                    "Claude Code (not found)",
                    Style::default().fg(DIM),
                ));
            }
            if let Some(&area) = chunks.get(row) {
                frame.render_widget(Paragraph::new(Line::from(spans)), padded(area));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_base_height_3() {
        let vm = HeaderVM::new(None, None, None, None);
        assert_eq!(vm.height, 3);
        assert!(!vm.has_account_line);
        assert!(!vm.has_version_line);
    }

    #[test]
    fn header_with_email_adds_account_line() {
        let email = "a@b.com".to_string();
        let vm = HeaderVM::new(None, Some(&email), None, None);
        assert!(vm.has_account_line);
        assert_eq!(vm.height, 4);
    }

    #[test]
    fn header_with_plan_adds_account_line() {
        let vm = HeaderVM::new(Some(&Plan::Max), None, None, None);
        assert!(vm.has_account_line);
        assert_eq!(vm.height, 4);
    }

    #[test]
    fn header_with_claude_version_adds_version_line() {
        let cv = ClaudeVersion {
            installed: Some("1.0.0".into()),
            latest: None,
        };
        let vm = HeaderVM::new(None, None, Some(&cv), None);
        assert!(vm.has_version_line);
        assert_eq!(vm.height, 4);
        assert_eq!(vm.claude_installed.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn header_all_lines() {
        let cv = ClaudeVersion::default();
        let email = "a@b".to_string();
        let vm = HeaderVM::new(Some(&Plan::Pro), Some(&email), Some(&cv), None);
        assert_eq!(vm.height, 5);
    }

    #[test]
    fn header_self_update_text_when_outdated() {
        let sv = SelfVersion {
            latest: Some("99.99.99".into()),
        };
        let vm = HeaderVM::new(None, None, None, Some(&sv));
        assert!(vm.self_update_text.is_some());
        assert!(vm.self_update_text.unwrap().contains("99.99.99"));
    }

    #[test]
    fn header_self_update_text_when_current() {
        let sv = SelfVersion {
            latest: Some(self_version::CURRENT_VERSION.into()),
        };
        let vm = HeaderVM::new(None, None, None, Some(&sv));
        assert!(vm.self_update_text.is_none());
    }

    #[test]
    fn header_claude_update_text_when_outdated() {
        let cv = ClaudeVersion {
            installed: Some("1.0.0".into()),
            latest: Some("2.0.0".into()),
        };
        let vm = HeaderVM::new(None, None, Some(&cv), None);
        assert_eq!(vm.claude_update_text.as_deref(), Some("(2.0.0 available)"));
    }

    #[test]
    fn header_claude_update_text_when_current() {
        let cv = ClaudeVersion {
            installed: Some("1.0.0".into()),
            latest: Some("1.0.0".into()),
        };
        let vm = HeaderVM::new(None, None, Some(&cv), None);
        assert!(vm.claude_update_text.is_none());
    }
}
