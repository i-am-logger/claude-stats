use std::collections::HashSet;
use std::ffi::OsString;

/// Encode an absolute path into the project directory name format used by
/// Claude Code under `~/.claude/projects/`. Every `/` is replaced with `-`.
///
/// Example: `/home/user/project` → `-home-user-project`
fn encode_cwd(path: &std::path::Path) -> OsString {
    let s = path.to_string_lossy();
    OsString::from(s.replace('/', "-"))
}

/// Whether process-based session detection is available on this platform.
/// When `true`, `active_project_dirs()` returns the real set of active
/// projects. When `false`, it returns an empty set and callers should fall
/// back to mtime-based detection.
pub(crate) const AVAILABLE: bool = cfg!(target_os = "linux");

/// Return the set of project directory names that have a running `claude`
/// process. On Linux this scans `/proc`; on other platforms it returns an
/// empty set (check `AVAILABLE` to distinguish "no sessions" from
/// "detection unavailable").
pub(crate) fn active_project_dirs() -> HashSet<OsString> {
    #[cfg(target_os = "linux")]
    {
        scan_proc()
    }
    #[cfg(not(target_os = "linux"))]
    {
        HashSet::new()
    }
}

#[cfg(target_os = "linux")]
fn scan_proc() -> HashSet<OsString> {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;

    /// Check if `/proc/<pid>/comm` is "claude".
    fn is_claude_process(pid_dir: &Path) -> bool {
        fs::read_to_string(pid_dir.join("comm"))
            .map(|s| s.trim() == "claude")
            .unwrap_or(false)
    }

    /// Read the CWD symlink of a process.
    fn read_cwd(pid_dir: &Path) -> Option<std::path::PathBuf> {
        let cwd = pid_dir.join("cwd");
        // readlink on /proc/<pid>/cwd may fail with EACCES for other users'
        // processes or ENOENT if the process exited between listing and reading.
        match fs::read_link(&cwd) {
            Ok(p) if p.as_os_str() != OsStr::new("") => Some(p),
            _ => None,
        }
    }

    let mut dirs = HashSet::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return dirs;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        // Only numeric directory names (PIDs)
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }

        let pid_dir = entry.path();
        if !is_claude_process(&pid_dir) {
            continue;
        }

        if let Some(cwd) = read_cwd(&pid_dir) {
            dirs.insert(encode_cwd(&cwd));
        }
    }

    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn encode_cwd_simple() {
        let p = Path::new("/home/user/project");
        assert_eq!(encode_cwd(p), "-home-user-project");
    }

    #[test]
    fn encode_cwd_root() {
        let p = Path::new("/");
        assert_eq!(encode_cwd(p), "-");
    }

    #[test]
    fn encode_cwd_nested() {
        let p = Path::new("/home/logger/Code/github/logger/claude-stats");
        assert_eq!(
            encode_cwd(p),
            "-home-logger-Code-github-logger-claude-stats"
        );
    }

    #[test]
    fn encode_cwd_no_leading_slash() {
        let p = Path::new("relative/path");
        assert_eq!(encode_cwd(p), "relative-path");
    }

    #[test]
    fn encode_cwd_trailing_slash() {
        // Path does NOT normalize trailing slashes on Linux, but /proc/*/cwd
        // symlinks never have them, so this case doesn't arise in practice.
        let p = Path::new("/home/user/project/");
        assert_eq!(encode_cwd(p), "-home-user-project-");
    }

    #[test]
    fn encode_cwd_single_component() {
        let p = Path::new("/tmp");
        assert_eq!(encode_cwd(p), "-tmp");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_detection_available_on_linux() {
        assert!(AVAILABLE);
        // Smoke test: should not panic.
        let _dirs = active_project_dirs();
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_encode_cwd_never_panics(path in "\\PC{0,100}") {
                let p = Path::new(&path);
                let _encoded = encode_cwd(p);
            }

            #[test]
            fn prop_encode_cwd_no_slashes(path in "/[a-z/]{1,50}") {
                let p = Path::new(&path);
                let encoded = encode_cwd(p);
                let s = encoded.to_string_lossy();
                assert!(!s.contains('/'), "encoded should not contain /: {s}");
            }
        }
    }
}
