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
//! Only macOS does this. A Linux desktop session normally starts from the
//! login profile already, and running a person's login shell at every
//! start (whose profile may print, prompt or start agents) is a cost worth
//! paying only where the inherited PATH is known to be wrong. Windows
//! programs read PATH from the registry-built environment and are left as
//! they are.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
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

/// The command line the login shell runs. `printenv` prints the exported
/// value whatever the shell's own syntax for lists (fish keeps PATH as a
/// list), and `;` separates commands in every common shell.
pub fn probe_command() -> String {
    format!("echo {START_MARKER}; /usr/bin/printenv PATH; echo {END_MARKER}")
}

/// Run `shell -l -c <probe>` and return the PATH it prints, or `None` when
/// the shell fails, prints no marked PATH, or is still running after
/// `timeout` (it is killed then).
#[cfg(unix)]
pub fn read_login_path(shell: &OsStr, timeout: Duration) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(shell)
        .arg("-l")
        .arg("-c")
        .arg(probe_command())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
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
/// directories when absent, and set the result on this process. Call before
/// any thread is started: it changes the process environment.
pub fn adopt_login_shell_path() {
    #[cfg(target_os = "macos")]
    {
        let login = read_login_path(&login_shell(), LOGIN_SHELL_TIMEOUT);
        let inherited = std::env::var_os("PATH");
        let merged = merge_path(
            login.as_deref().map(OsStr::new),
            inherited.as_deref(),
            &HOMEBREW_DIRS,
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

    #[cfg(unix)]
    #[test]
    fn a_real_login_shell_reports_its_path() {
        let path = read_login_path(OsStr::new("/bin/sh"), Duration::from_secs(10))
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
            read_login_path(stuck.as_os_str(), Duration::from_millis(300)),
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
            read_login_path(failing.as_os_str(), Duration::from_secs(5)),
            None
        );

        let missing = dir.path().join("no-such-shell");
        assert_eq!(
            read_login_path(missing.as_os_str(), Duration::from_secs(5)),
            None
        );
    }
}
