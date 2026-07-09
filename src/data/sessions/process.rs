use std::collections::HashMap;
use std::ffi::OsString;

/// Encode an absolute path into the project directory name format used by
/// Claude Code under `~/.claude/projects/`. Every `/` is replaced with `-`.
///
/// Example: `/home/user/project` → `-home-user-project`
fn encode_cwd(path: &std::path::Path) -> OsString {
    let s = path.to_string_lossy();
    OsString::from(s.replace('/', "-"))
}

/// Check whether the first NUL-separated token of a `/proc/<pid>/cmdline`
/// buffer names a `claude` binary — either the bare name or a path whose
/// basename is `claude` (e.g. `/usr/local/bin/claude`). Deliberately does
/// not match `claude-code-proxy`, `claude-stats`, etc.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn argv0_is_claude(cmdline: &[u8]) -> bool {
    cmdline
        .split(|&b| b == 0)
        .next()
        .and_then(|argv0| std::str::from_utf8(argv0).ok())
        .is_some_and(|s| {
            std::path::Path::new(s).file_name() == Some(std::ffi::OsStr::new("claude"))
        })
}

/// Whether process-based session detection is available on this platform.
/// When `true`, `active_project_dirs()` returns the real set of active
/// projects. When `false`, it returns an empty set and callers should fall
/// back to mtime-based detection.
pub(crate) const AVAILABLE: bool = cfg!(target_os = "linux");

/// Return a map of project directory names to the number of running `claude`
/// processes in each. On Linux this scans `/proc`; on other platforms it
/// returns an empty map (check `AVAILABLE` to distinguish "no sessions" from
/// "detection unavailable").
pub(crate) fn active_project_dirs() -> HashMap<OsString, usize> {
    #[cfg(target_os = "linux")]
    {
        scan_proc()
    }
    #[cfg(not(target_os = "linux"))]
    {
        HashMap::new()
    }
}

#[cfg(target_os = "linux")]
fn scan_proc() -> HashMap<OsString, usize> {
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;

    /// Check if `/proc/<pid>` belongs to a running `claude` CLI.
    ///
    /// `comm` catches direct execs and the node-based packaging (which sets
    /// the process title). The nixpkgs packaging (≥ 2.1.170) launches the
    /// native binary through the glibc loader, so `comm` reads
    /// "ld-linux-x86-64" — fall back to argv[0], which the wrapper sets to
    /// "claude" via `exec -a`.
    fn is_claude_process(pid_dir: &Path) -> bool {
        if fs::read_to_string(pid_dir.join("comm")).is_ok_and(|s| s.trim() == "claude") {
            return true;
        }
        fs::read(pid_dir.join("cmdline")).is_ok_and(|buf| argv0_is_claude(&buf))
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

    let mut dirs = HashMap::new();
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
            *dirs.entry(encode_cwd(&cwd)).or_insert(0) += 1;
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
        // Smoke test: should not panic.
        let _dirs = active_project_dirs();
    }

    // ── argv0_is_claude ───────────────────────────────────────────

    #[test]
    fn argv0_bare_claude_with_args() {
        // nixpkgs loader form: argv[0] is "claude" via `exec -a`
        assert!(argv0_is_claude(
            b"claude\0--library-path\0/nix/store/lib\0/nix/store/claude-code/claude\0"
        ));
    }

    #[test]
    fn argv0_path_basename_claude() {
        assert!(argv0_is_claude(b"/usr/local/bin/claude\0"));
    }

    #[test]
    fn argv0_bare_claude_no_nul() {
        assert!(argv0_is_claude(b"claude"));
    }

    #[test]
    fn argv0_rejects_prefixed_names() {
        assert!(!argv0_is_claude(b"claude-code-proxy\0"));
        assert!(!argv0_is_claude(b"claude-stats\0"));
    }

    #[test]
    fn argv0_rejects_loader_path() {
        assert!(!argv0_is_claude(
            b"/nix/store/glibc-2.42/lib/ld-linux-x86-64.so.2\0--library-path\0"
        ));
    }

    #[test]
    fn argv0_empty_cmdline() {
        // Kernel threads have an empty cmdline.
        assert!(!argv0_is_claude(b""));
    }

    #[test]
    fn argv0_invalid_utf8() {
        assert!(!argv0_is_claude(&[0xff, 0xfe, 0x00]));
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
