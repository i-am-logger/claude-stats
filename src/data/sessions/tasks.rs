#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

//! Shared task lists (`~/.claude/tasks/<listId>/`) — the same data Claude
//! Code's own status header reads to show `activeForm` text (e.g. "Wiring
//! the next source into the corroboration mechanism…") instead of a
//! decorative verb. Only the main session's own unowned in-progress task is
//! relevant here; tasks owned by a named teammate belong to that teammate's
//! own turn, not the top-level session's status line.

use std::fs;
use std::path::{Path, PathBuf};

/// Sidecar file `~/.claude/tasks/<listId>/<id>.json`. Extra fields
/// (`description`, `blocks`, `blockedBy`) are ignored.
#[derive(Debug, serde::Deserialize)]
struct TaskFile {
    status: Option<String>,
    owner: Option<String>,
    #[serde(rename = "activeForm")]
    active_form: Option<String>,
    subject: Option<String>,
}

/// `~/.claude/tasks/session-<first 8 hex chars of session_id>/` — the
/// implicit-team-keyed shared task list directory for a session.
fn team_dir(tasks_root: &Path, session_id: &str) -> Option<PathBuf> {
    let team_name = format!("session-{}", session_id.get(..8)?);
    Some(tasks_root.join(team_name))
}

fn read_task_file(path: &Path) -> Option<TaskFile> {
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// The current session's `activeForm` text, if it has an in-progress task
/// nobody else owns — the same text and priority Claude Code's own status
/// header shows (`activeForm ?? subject`), taking precedence there over a
/// decorative random verb; here it takes precedence over the generic state
/// word `activity::extract_activity_from_parsed` falls back to when no
/// `TaskCreate` call is in view either. `tasks_root` is injected (production
/// passes `~/.claude/tasks`) for testability.
///
/// Task ids are unique, monotonically-issued strings (`.highwatermark`)
/// formatted as plain integers — sorting numerically picks the
/// earliest-created in-progress task when more than one somehow qualifies.
pub(crate) fn current_task_activity(tasks_root: &Path, session_id: &str) -> Option<String> {
    let dir = team_dir(tasks_root, session_id)?;
    let entries = fs::read_dir(dir).ok()?;

    let mut candidates: Vec<(u64, TaskFile)> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| {
            let id: u64 = e.path().file_stem()?.to_str()?.parse().ok()?;
            let task = read_task_file(&e.path())?;
            Some((id, task))
        })
        .filter(|(_, task)| task.status.as_deref() == Some("in_progress") && task.owner.is_none())
        .collect();
    candidates.sort_by_key(|(id, _)| *id);

    candidates
        .into_iter()
        .next()
        .and_then(|(_, task)| task.active_form.or(task.subject))
}

/// A specific task's `activeForm`/`subject`, by id, regardless of its
/// current status — resolves a `TaskUpdate` tool call's `{taskId, status}`
/// input (which carries no human text of its own) into real text. Reading
/// directly by id, rather than scanning for `status == "in_progress"` like
/// `current_task_activity` above, is what lets this catch the task while
/// it's still mid-transition: the assistant's `TaskUpdate` `tool_use` can land
/// in the session JSONL before the task's own sidecar file's status field
/// has actually flipped to `"in_progress"` yet.
pub(crate) fn activity_for_task_id(
    tasks_root: &Path,
    session_id: &str,
    task_id: &str,
) -> Option<String> {
    let dir = team_dir(tasks_root, session_id)?;
    let task = read_task_file(&dir.join(format!("{task_id}.json")))?;
    task.active_form.or(task.subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(dir: &Path, id: &str, status: &str, owner: Option<&str>, active_form: &str) {
        fs::create_dir_all(dir).unwrap();
        let owner_field = owner.map_or_else(String::new, |o| format!(r#","owner":"{o}""#));
        fs::write(
            dir.join(format!("{id}.json")),
            format!(
                r#"{{"id":"{id}","subject":"subject text","activeForm":"{active_form}","status":"{status}"{owner_field}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn finds_unowned_in_progress_task() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-995de141");
        write_task(&dir, "67", "in_progress", None, "Adding composition axiom");

        let result = current_task_activity(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979");
        assert_eq!(result.as_deref(), Some("Adding composition axiom"));
    }

    #[test]
    fn ignores_owned_task() {
        // Owned by a named teammate — that's the teammate's own turn, not
        // the main session's.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-995de141");
        write_task(
            &dir,
            "67",
            "in_progress",
            Some("build-sumo-source"),
            "Building SUMO source",
        );

        assert!(
            current_task_activity(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979").is_none()
        );
    }

    #[test]
    fn ignores_pending_and_completed_tasks() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-995de141");
        write_task(&dir, "1", "pending", None, "Not started yet");
        write_task(&dir, "2", "completed", None, "Already done");

        assert!(
            current_task_activity(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979").is_none()
        );
    }

    #[test]
    fn picks_lowest_id_when_multiple_in_progress() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-995de141");
        write_task(&dir, "67", "in_progress", None, "Second task");
        write_task(&dir, "9", "in_progress", None, "First task");

        let result = current_task_activity(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979");
        assert_eq!(result.as_deref(), Some("First task"));
    }

    #[test]
    fn falls_back_to_subject_when_no_active_form() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-995de141");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("1.json"),
            r#"{"id":"1","subject":"Fallback subject","status":"in_progress"}"#,
        )
        .unwrap();

        let result = current_task_activity(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979");
        assert_eq!(result.as_deref(), Some("Fallback subject"));
    }

    #[test]
    fn no_tasks_dir_returns_none() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            current_task_activity(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979").is_none()
        );
    }

    #[test]
    fn malformed_task_file_skipped_without_panicking() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-995de141");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("1.json"), "not json").unwrap();

        assert!(
            current_task_activity(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979").is_none()
        );
    }

    // ── activity_for_task_id ────────────────────────────────────────

    #[test]
    fn activity_for_task_id_finds_task_regardless_of_status() {
        // Unlike current_task_activity, a by-id lookup must find the task
        // even while it's still "pending" — this is exactly the race the
        // function exists to close (the TaskUpdate tool_use can land in the
        // JSONL before the task file's own status flips).
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-995de141");
        write_task(&dir, "67", "pending", None, "Adding composition axiom");

        let result =
            activity_for_task_id(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979", "67");
        assert_eq!(result.as_deref(), Some("Adding composition axiom"));
    }

    #[test]
    fn activity_for_task_id_finds_owned_task_too() {
        // Unlike current_task_activity, ownership doesn't matter here — the
        // caller already knows exactly which task it's asking about (its own
        // TaskUpdate call referenced this exact id).
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-995de141");
        write_task(
            &dir,
            "67",
            "in_progress",
            Some("some-teammate"),
            "Doing the thing",
        );

        let result =
            activity_for_task_id(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979", "67");
        assert_eq!(result.as_deref(), Some("Doing the thing"));
    }

    #[test]
    fn activity_for_task_id_missing_task_returns_none() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("session-995de141")).unwrap();

        let result =
            activity_for_task_id(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979", "999");
        assert!(result.is_none());
    }

    #[test]
    fn activity_for_task_id_falls_back_to_subject() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("session-995de141");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("1.json"),
            r#"{"id":"1","subject":"Fallback subject","status":"pending"}"#,
        )
        .unwrap();

        let result = activity_for_task_id(root.path(), "995de141-0fa1-4d50-9416-72f8b0cd4979", "1");
        assert_eq!(result.as_deref(), Some("Fallback subject"));
    }
}
