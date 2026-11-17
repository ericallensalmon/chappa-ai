//! The login shell's PATH, for an app started from Finder or the Dock.
//!
//! A macOS GUI launch inherits launchd's minimal PATH
//! (`/usr/bin:/bin:/usr/sbin:/sbin`), not the one the person's login shell
//! builds. Homebrew (`/opt/homebrew/bin`), version managers and most agent
//! CLIs live outside it, so `git`, `docker`, `node` or `gitleaks` would not
//! be found by anything the app starts. At startup the app asks the login
//! shell for its PATH once, puts it ahead of the inherited one, and sets
//! the result on its own environment, which every pane, agent, source,
//! stack runner, git call and secret scan inherits.
//!
//! The login shell (`$SHELL -l`) reads `~/.zprofile` but not `~/.zshrc`,
//! where many installers and version managers put their PATH lines. So
//! when a common tool is still missing after the first read, the shell is
//! asked once more as an interactive login shell (`$SHELL -i -l`), which
//! reads both. The interactive read is only a fallback because an
//! interactive profile does more (prompt themes, update checks, plugin
//! managers) and is slower. Each read is bounded by the same short timeout,
//! so startup waits at most for the two.
//!
//! Only macOS does this. A Linux desktop session normally starts from the
//! login profile already, and running a person's login shell at every
//! start (whose profile may print, prompt or start agents) is a cost worth
//! paying only where the inherited PATH is known to be wrong. Windows
//! programs read PATH from the registry-built environment and are left as
//! they are.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Printed before and after the PATH, so whatever a profile prints around
/// it is ignored.
pub const START_MARKER: &str = "__CHAPPA_LOGIN_PATH_START__";
pub const END_MARKER: &str = "__CHAPPA_LOGIN_PATH_END__";

/// How long the login shell may take. A slow or stuck profile must not hold
/// the window back for long; past this the inherited PATH is used as is.
pub const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(3);

/// Where Homebrew installs on Apple Silicon and on Intel Macs: added when
/// absent even if the login shell could not be read.
pub const HOMEBREW_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// Tools that almost every project here needs. One of them missing from the
/// PATH the login shell gave is the sign that the interactive profile holds
/// PATH lines the login profile does not.
pub const COMMON_TOOLS: [&str; 4] = ["git", "node", "docker", "npm"];

/// The text between the markers in `output`, trimmed; `None` when either
/// marker is missing or nothing sits between them.
pub fn parse_marked(output: &str) -> Option<&str> {
    let start = output.find(START_MARKER)? + START_MARKER.len();
    let rest = &output[start..];
    let end = rest.find(END_MARKER)?;
    let path = rest[..end].trim();
    (!path.is_empty()).then_some(path)
}

/// `login` first, then `inherited`, then each of `extra`, keeping the first
/// occurrence of every directory and dropping empty entries.
pub fn merge_path(login: Option<&OsStr>, inherited: Option<&OsStr>, extra: &[&str]) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let lists = [login, inherited].into_iter().flatten();
    let all = lists
        .flat_map(std::env::split_paths)
        .chain(extra.iter().map(PathBuf::from));
    for dir in all {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    // Every entry came out of split_paths or is a fixed directory, so none
    // holds the separator; join cannot fail on them.
    std::env::join_paths(dirs).unwrap_or_default()
}

/// The entries of `tools` that no directory of `path` holds, `has` telling
/// whether a candidate file exists.
pub fn missing_tools<'a>(
    path: &OsStr,
    tools: &[&'a str],
    has: impl Fn(&Path) -> bool,
) -> Vec<&'a str> {
    let dirs: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|d| !d.as_os_str().is_empty())
        .collect();
    tools
        .iter()
        .copied()
        .filter(|tool| !dirs.iter().any(|d| has(&d.join(tool))))
        .collect()
}

/// The PATH to use: the login read merged ahead of `inherited` plus the
/// Homebrew directories, then, only when a [`COMMON_TOOLS`] entry is still
/// missing from that, the interactive read merged ahead of it. `read(false)`
/// asks the login shell, `read(true)` the interactive login shell; either
/// may give nothing.
pub fn resolve_path(
    inherited: Option<&OsStr>,
    mut read: impl FnMut(bool) -> Option<String>,
    has: impl Fn(&Path) -> bool,
) -> OsString {
    let login = read(false);
    let merged = merge_path(login.as_deref().map(OsStr::new), inherited, &HOMEBREW_DIRS);
    if missing_tools(&merged, &COMMON_TOOLS, &has).is_empty() {
        return merged;
    }
    match read(true) {
        Some(interactive) => merge_path(Some(OsStr::new(&interactive)), Some(&merged), &[]),
        None => merged,
    }
}

/// The command line the login shell runs. `printenv` prints the exported
/// value whatever the shell's own syntax for lists (fish keeps PATH as a
/// list), and `;` separates commands in every common shell.
pub fn probe_command() -> String {
    format!("echo {START_MARKER}; /usr/bin/printenv PATH; echo {END_MARKER}")
}

/// The shell's flags: `-l -c`, or `-i -l -c` for an interactive login shell.
pub fn shell_args(interactive: bool) -> Vec<String> {
    let mut args = Vec::new();
    if interactive {
        args.push("-i".to_owned());
    }
    args.extend(["-l".to_owned(), "-c".to_owned(), probe_command()]);
    args
}

/// Run `shell [-i] -l -c <probe>` and return the PATH it prints, or `None`
/// when the shell fails, prints no marked PATH, or is still running after
/// `timeout` (it is killed then).
#[cfg(unix)]
pub fn read_login_path(shell: &OsStr, interactive: bool, timeout: Duration) -> Option<String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new(shell);
    command
        .args(shell_args(interactive))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // A session of its own, so no controlling terminal: an interactive shell
    // started from a terminal would otherwise try to take that terminal over
    // for job control, or stop itself waiting to be in its foreground.
    // SAFETY: setsid is async-signal-safe and touches no memory of this
    // process.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    // The read happens off this thread: a profile that leaves a background
    // job holding stdout open would otherwise block startup past the
    // timeout.
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        let _ = tx.send(text);
    });
    let text = rx.recv_timeout(timeout).ok();
    let _ = child.kill();
    let _ = child.wait();
    parse_marked(&text?).map(str::to_owned)
}

/// The shell to ask: `$SHELL`, else zsh (the macOS default).
#[cfg(unix)]
pub fn login_shell() -> OsString {
    std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".into())
}

/// Put the login shell's PATH ahead of the inherited one, add the Homebrew
/// directories when absent, fall back to the interactive login shell when a
/// common tool is still missing, and set the result on this process. Call before
/// any thread is started: it changes the process environment.
pub fn adopt_login_shell_path() {
    #[cfg(target_os = "macos")]
    {
        let shell = login_shell();
        let inherited = std::env::var_os("PATH");
        let merged = resolve_path(
            inherited.as_deref(),
            |interactive| read_login_path(&shell, interactive, LOGIN_SHELL_TIMEOUT),
            Path::is_file,
        );
        if !merged.is_empty() {
            std::env::set_var("PATH", merged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(dirs: &[&str]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    fn split(path: &OsStr) -> Vec<String> {
        std::env::split_paths(path)
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_path_between_the_markers_is_read_and_profile_noise_is_ignored() {
        let output = format!(
            "Welcome back\n{START_MARKER}\n/opt/homebrew/bin:/usr/bin\n{END_MARKER}\nbye\n"
        );
        assert_eq!(parse_marked(&output), Some("/opt/homebrew/bin:/usr/bin"));
    }

    #[test]
    fn a_missing_marker_or_an_empty_value_reads_as_nothing() {
        assert_eq!(parse_marked("/usr/bin:/bin\n"), None);
        assert_eq!(parse_marked(&format!("{START_MARKER}\n/usr/bin\n")), None);
        assert_eq!(parse_marked(&format!("/usr/bin\n{END_MARKER}\n")), None);
        assert_eq!(
            parse_marked(&format!("{START_MARKER}\n  \n{END_MARKER}")),
            None
        );
        // The end marker printed before the start one is not a value.
        assert_eq!(
            parse_marked(&format!("{END_MARKER}\n/usr/bin\n{START_MARKER}")),
            None
        );
    }

    #[test]
    fn the_login_path_comes_first_and_no_directory_repeats() {
        let login = joined(&["/opt/homebrew/bin", "/opt/tools/bin", "/usr/bin"]);
        let inherited = joined(&["/usr/bin", "/bin", "/usr/sbin"]);
        let merged = merge_path(Some(&login), Some(&inherited), &HOMEBREW_DIRS);
        assert_eq!(
            split(&merged),
            [
                "/opt/homebrew/bin",
                "/opt/tools/bin",
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/usr/local/bin",
            ]
        );
    }

    #[test]
    fn without_a_login_path_the_inherited_one_gains_the_homebrew_directories() {
        let inherited = joined(&["/usr/bin", "/bin"]);
        let merged = merge_path(None, Some(&inherited), &HOMEBREW_DIRS);
        assert_eq!(
            split(&merged),
            ["/usr/bin", "/bin", "/opt/homebrew/bin", "/usr/local/bin"]
        );
        // Nothing at all still gives the Homebrew directories.
        let merged = merge_path(None, None, &HOMEBREW_DIRS);
        assert_eq!(split(&merged), HOMEBREW_DIRS);
    }

    #[test]
    fn empty_entries_are_dropped() {
        let login = joined(&["", "/usr/bin", ""]);
        let merged = merge_path(Some(&login), None, &[]);
        assert_eq!(split(&merged), ["/usr/bin"]);
    }

    #[test]
    fn a_tool_counts_as_found_in_any_directory_of_the_path() {
        let path = joined(&["/usr/bin", "", "/opt/tools/bin"]);
        let has =
            |p: &Path| p == Path::new("/opt/tools/bin/node") || p == Path::new("/usr/bin/git");
        assert_eq!(missing_tools(&path, &COMMON_TOOLS, has), ["docker", "npm"]);
        assert!(missing_tools(&path, &["git", "node"], has).is_empty());
        // An empty entry is not the current directory here.
        assert_eq!(missing_tools(&joined(&[""]), &["git"], |_| true), ["git"]);
    }

    /// Every common tool, in one directory.
    fn all_tools_in(dir: &'static str) -> impl Fn(&Path) -> bool {
        move |p: &Path| {
            p.parent() == Some(Path::new(dir))
                && COMMON_TOOLS
                    .iter()
                    .any(|t| p.file_name() == Some(OsStr::new(t)))
        }
    }

    #[test]
    fn the_interactive_shell_is_not_asked_when_the_login_path_has_every_tool() {
        let mut asked = Vec::new();
        let merged = resolve_path(
            Some(&joined(&["/usr/bin", "/bin"])),
            |interactive| {
                asked.push(interactive);
                Some("/opt/homebrew/bin:/usr/bin".to_owned())
            },
            all_tools_in("/opt/homebrew/bin"),
        );
        assert_eq!(asked, [false]);
        assert_eq!(
            split(&merged),
            ["/opt/homebrew/bin", "/usr/bin", "/bin", "/usr/local/bin"]
        );
    }

    #[test]
    fn a_missing_tool_asks_the_interactive_shell_once_and_puts_its_path_first() {
        let mut asked = Vec::new();
        let merged = resolve_path(
            Some(&joined(&["/usr/bin", "/bin"])),
            |interactive| {
                asked.push(interactive);
                Some(if interactive {
                    "/opt/nodes/current/bin:/opt/homebrew/bin:/usr/bin".to_owned()
                } else {
                    "/opt/homebrew/bin:/usr/bin".to_owned()
                })
            },
            all_tools_in("/opt/nodes/current/bin"),
        );
        assert_eq!(asked, [false, true]);
        assert_eq!(
            split(&merged),
            [
                "/opt/nodes/current/bin",
                "/opt/homebrew/bin",
                "/usr/bin",
                "/bin",
                "/usr/local/bin",
            ]
        );
    }

    #[test]
    fn when_neither_shell_answers_the_inherited_path_and_homebrew_remain() {
        let mut asked = Vec::new();
        let merged = resolve_path(
            Some(&joined(&["/usr/bin", "/bin"])),
            |interactive| {
                asked.push(interactive);
                None
            },
            |_| false,
        );
        assert_eq!(asked, [false, true]);
        assert_eq!(
            split(&merged),
            ["/usr/bin", "/bin", "/opt/homebrew/bin", "/usr/local/bin"]
        );
    }

    #[test]
    fn the_interactive_read_adds_only_the_interactive_flag() {
        let probe = probe_command();
        assert_eq!(shell_args(false), ["-l", "-c", probe.as_str()]);
        assert_eq!(shell_args(true), ["-i", "-l", "-c", probe.as_str()]);
    }

    #[cfg(unix)]
    #[test]
    fn the_interactive_read_runs_the_shell_with_its_flags_and_ignores_the_noise() {
        use std::os::unix::fs::PermissionsExt;
        // A shell whose PATH line only an interactive start prints, among
        // the kind of noise an interactive profile makes.
        let dir = tempfile::tempdir().unwrap();
        let shell = dir.path().join("noisy-shell");
        let script = [
            "#!/bin/sh".to_owned(),
            r#"if [ "$1" = -i ]; then"#.to_owned(),
            "  echo 'updating plugins...'".to_owned(),
            format!("  echo 'theme loaded{START_MARKER}'"),
            "  echo /opt/nodes/bin:/usr/bin".to_owned(),
            format!("  echo '{END_MARKER}%'"),
            "else".to_owned(),
            format!("  echo {START_MARKER}; echo {END_MARKER}"),
            "fi".to_owned(),
        ]
        .join("\n");
        std::fs::write(&shell, script).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let timeout = Duration::from_secs(5);
        assert_eq!(read_login_path(shell.as_os_str(), false, timeout), None);
        assert_eq!(
            read_login_path(shell.as_os_str(), true, timeout).as_deref(),
            Some("/opt/nodes/bin:/usr/bin")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_real_interactive_login_shell_reports_its_path_without_a_terminal() {
        let started = std::time::Instant::now();
        let path = read_login_path(OsStr::new("/bin/sh"), true, Duration::from_secs(10))
            .expect("sh -i -l prints a PATH");
        assert!(
            split(OsStr::new(&path))
                .iter()
                .any(|d| d == "/bin" || d == "/usr/bin"),
            "{path}"
        );
        // Without a terminal of its own an interactive shell neither stops
        // for job control nor waits for input.
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn a_real_login_shell_reports_its_path() {
        let path = read_login_path(OsStr::new("/bin/sh"), false, Duration::from_secs(10))
            .expect("sh -l prints a PATH");
        assert!(
            split(OsStr::new(&path))
                .iter()
                .any(|d| d == "/bin" || d == "/usr/bin"),
            "{path}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_stuck_or_failing_shell_reads_as_nothing_within_the_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stuck = dir.path().join("stuck-shell");
        std::fs::write(&stuck, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o700)).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(
            read_login_path(stuck.as_os_str(), false, Duration::from_millis(300)),
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );

        let failing = dir.path().join("failing-shell");
        std::fs::write(&failing, "#!/bin/sh\necho no path here\nexit 1\n").unwrap();
        std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            read_login_path(failing.as_os_str(), true, Duration::from_secs(5)),
            None
        );

        let missing = dir.path().join("no-such-shell");
        assert_eq!(
            read_login_path(missing.as_os_str(), true, Duration::from_secs(5)),
            None
        );
    }
}
