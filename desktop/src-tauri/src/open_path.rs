//! Opening file paths named in terminal output.
//!
//! The webview detects a path under the cursor and resolves it against the
//! base directory of the entry that printed it (`ui/src/term/paths.ts`, pure,
//! no IPC while hovering). A ctrl/cmd+click then comes here twice:
//!
//! 1. [`resolve_path`] checks the target on disk (`{path, exists, kind}`), so
//!    a missing file is reported instead of handed to an editor that would
//!    create it, and a directory goes to the file manager instead.
//! 2. [`open_in_editor`] opens it: the `editorCommand` setting's template, or
//!    the OS default handler when the template is empty.
//!
//! Container agents print container paths; [`parse_mounts`] reads the bind
//! mounts off `docker inspect` so the webview can map those paths back to the
//! host.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use tauri::{Manager, State};

use crate::agent_tools::shell_words;
use crate::settings::SettingsState;

/// What a path names on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    File,
    Dir,
}

/// The click-time answer for one path. `kind` is `None` when nothing exists
/// there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedPath {
    pub path: String,
    pub exists: bool,
    pub kind: Option<PathKind>,
}

/// `text` as an absolute path: `~` expands to `home`, a relative path joins
/// `base`, and `.`/`..` collapse lexically (no symlink resolution, and no
/// `\\?\` prefix the way `canonicalize` would add on Windows). `None` when
/// the path is relative with no base to join, or `~` with no home: the app's
/// own working directory says nothing about where the output came from.
pub fn absolute_path(base: Option<&Path>, text: &str, home: Option<&Path>) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let joined = if let Some(rest) = tilde_rest(text) {
        home?.join(rest)
    } else {
        let path = Path::new(text);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.filter(|b| b.is_absolute())?.join(path)
        }
    };
    Some(normalize(&joined))
}

/// The part after `~` for `~`, `~/x` and `~\x`; `None` for anything else
/// (`~user` is not expanded).
fn tilde_rest(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('~')?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix(['/', '\\'])
}

/// Collapse `.` and `..` without touching the filesystem. A `..` at the root
/// stays at the root, as the OS treats it.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.file_name().is_some() {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve and stat. A path that cannot be made absolute is reported as
/// missing under its text as given.
pub fn resolve(base: Option<&Path>, text: &str, home: Option<&Path>) -> ResolvedPath {
    let Some(path) = absolute_path(base, text, home) else {
        return ResolvedPath {
            path: text.trim().to_owned(),
            exists: false,
            kind: None,
        };
    };
    let kind = std::fs::metadata(&path)
        .ok()
        .map(|m| if m.is_dir() { PathKind::Dir } else { PathKind::File });
    ResolvedPath {
        path: path.to_string_lossy().into_owned(),
        exists: kind.is_some(),
        kind,
    }
}

fn home(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path().home_dir().ok()
}

/// The user's home directory, so the webview can expand `~/` and give
/// shells with no spawn directory a base. `None` when the OS reports none.
#[tauri::command]
pub fn home_dir(app: tauri::AppHandle) -> Option<String> {
    home(&app).map(|p| p.to_string_lossy().into_owned())
}

/// Click-time existence check for a detected path. `base` is the directory
/// a relative `text` is relative to; the webview usually sends an absolute
/// path already.
#[tauri::command]
pub fn resolve_path(app: tauri::AppHandle, base: Option<String>, text: String) -> ResolvedPath {
    resolve(base.as_deref().map(Path::new), &text, home(&app).as_deref())
}

/// Open `path` at `line`/`col` (both default to 1). A directory opens in the
/// file manager. A file opens with the `editorCommand` template, or with the
/// OS default handler when the template is empty. The editor is started
/// detached and never waited on; the error text names what went wrong
/// (missing file, a program that would not start, a refused default open).
#[tauri::command]
pub fn open_in_editor(
    settings: State<'_, SettingsState>,
    path: String,
    line: Option<u32>,
    col: Option<u32>,
) -> Result<(), String> {
    let target = Path::new(&path);
    let meta = std::fs::metadata(target).map_err(|_| format!("not found: {path}"))?;
    if meta.is_dir() {
        return open_with_os(target, PathKind::Dir);
    }
    let template = settings.snapshot().editor_command;
    if template.trim().is_empty() {
        return open_with_os(target, PathKind::File);
    }
    let argv = editor_argv(&template, &path, line.unwrap_or(1), col.unwrap_or(1));
    spawn_detached(&argv)
}

// --- the editor template ------------------------------------------------------

/// The argv for one open. The template is split with [`shell_words`] FIRST,
/// then each word has `{path}`, `{line}` and `{col}` substituted in one
/// left-to-right pass. The path therefore always lands inside a single argv
/// entry: a path with spaces stays one argument, and a path containing quotes
/// or placeholder text can neither add arguments nor be substituted again.
/// A template that never mentions `{path}` gets the path as its last
/// argument (`notepad` alone still opens the file).
pub fn editor_argv(template: &str, path: &str, line: u32, col: u32) -> Vec<String> {
    let line = line.max(1).to_string();
    let col = col.max(1).to_string();
    let mut saw_path = false;
    let mut argv: Vec<String> = shell_words(template)
        .into_iter()
        .map(|word| {
            let mut out = String::with_capacity(word.len());
            let mut rest = word.as_str();
            while let Some(at) = rest.find('{') {
                out.push_str(&rest[..at]);
                let tail = &rest[at..];
                let (value, len) = if tail.starts_with("{path}") {
                    saw_path = true;
                    (path, "{path}".len())
                } else if tail.starts_with("{line}") {
                    (line.as_str(), "{line}".len())
                } else if tail.starts_with("{col}") {
                    (col.as_str(), "{col}".len())
                } else {
                    ("{", 1)
                };
                out.push_str(value);
                rest = &tail[len..];
            }
            out.push_str(rest);
            out
        })
        .collect();
    if !saw_path {
        argv.push(path.to_owned());
    }
    argv
}

/// Start the editor without a shell and without waiting for it. Output goes
/// nowhere (the app has no place to show it) and a background thread reaps
/// the child so it never lingers as a zombie on Unix.
fn spawn_detached(argv: &[String]) -> Result<(), String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "the editor command is empty".to_owned())?;
    let resolved = resolve_program(program);
    let mut command = Command::new(&resolved);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: a launcher shim like `code.cmd` is a console
        // program and would otherwise flash a console window.
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {program}: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// The program to start for a template's first word. Only Windows needs
/// help: editors install their command-line launcher as a `.cmd` shim
/// (`code.cmd`, `cursor.cmd`) beside an extensionless sh script of the bare
/// name, and the process spawner only ever appends `.exe`, so `code` would
/// not be found (or, found as the sh script, fails with "not a valid Win32
/// application"). A bare name without an extension is therefore looked up
/// on PATH with each PATHEXT extension, the way a shell would. Everything
/// else is used as written.
fn resolve_program(program: &str) -> PathBuf {
    #[cfg(windows)]
    {
        let path = std::env::var_os("PATH");
        let pathext = std::env::var_os("PATHEXT");
        if let Some(found) = find_with_pathext(program, path.as_deref(), pathext.as_deref()) {
            return found;
        }
    }
    PathBuf::from(program)
}

/// [`resolve_program`]'s search, with the environment injected for tests.
#[cfg(windows)]
fn find_with_pathext(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let bare = Path::new(program);
    if program.contains(['/', '\\', ':']) || bare.extension().is_some() {
        return None;
    }
    let pathext = pathext
        .and_then(|e| e.to_str())
        .unwrap_or(".COM;.EXE;.BAT;.CMD");
    for dir in std::env::split_paths(path?) {
        for ext in pathext.split(';').filter(|e| e.starts_with('.')) {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// --- the OS default handler -------------------------------------------------------

/// Extensions the OS default handler would RUN rather than open for reading.
/// Terminal output is untrusted text, so a click on a printed path must never
/// execute it; with no editor configured those files are refused instead.
#[cfg(windows)]
const RUNNABLE_EXTENSIONS: &[&str] = &[
    "exe", "com", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msc", "msi",
    "msp", "scr", "pif", "lnk", "url", "hta", "cpl", "reg", "jar", "appref-ms", "application",
    "appx", "msix", "inf", "scf", "settingcontent-ms",
];

#[cfg(not(windows))]
const RUNNABLE_EXTENSIONS: &[&str] = &["app", "command", "desktop", "jar", "appimage", "run"];

/// Why the default handler must not get `path`, or `None` when it may.
/// Windows: an extension from the list above or from PATHEXT. Elsewhere: an
/// executable file (the default handler may run it) or one of the
/// launcher extensions above (`.app` bundles are directories, so this also
/// applies to directories there).
fn default_open_refusal(path: &Path, kind: PathKind) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let refused = format!("not running {}: choose an editor in Settings to open it", path.display());
    #[cfg(windows)]
    {
        if kind == PathKind::Dir {
            return None;
        }
        let in_pathext = std::env::var("PATHEXT").is_ok_and(|v| {
            v.split(';')
                .any(|e| e.strip_prefix('.').is_some_and(|e| e.eq_ignore_ascii_case(&ext)))
        });
        if !ext.is_empty() && (in_pathext || RUNNABLE_EXTENSIONS.contains(&ext.as_str())) {
            return Some(refused);
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        if RUNNABLE_EXTENSIONS.contains(&ext.as_str()) {
            return Some(refused);
        }
        let executable = std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
        if kind == PathKind::File && executable {
            return Some(refused);
        }
    }
    None
}

/// Hand `path` to the OS default handler (the file manager for a directory).
fn open_with_os(path: &Path, kind: PathKind) -> Result<(), String> {
    if let Some(reason) = default_open_refusal(path, kind) {
        return Err(reason);
    }
    tauri_plugin_opener::open_path(path, None::<&str>)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))
}

// --- container mounts ---------------------------------------------------------------

/// One container mount: `destination` inside the container is backed by
/// `source` on the host. `source` is empty for a mount the host cannot open
/// (a named volume or tmpfs lives inside the container runtime, not in the
/// host's filesystem), so a path under it is reported as container-only
/// rather than mapped to a host path that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub source: String,
    pub destination: String,
}

/// The fields of one `docker inspect` `.Mounts` entry this app reads.
#[derive(Deserialize)]
struct InspectMount {
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(rename = "Source", default)]
    source: String,
    #[serde(rename = "Destination", default)]
    destination: String,
}

/// `{{json .Mounts}}` as mounts. Anything unparseable is no mounts: the
/// probe must never fail over this field, and no mounts only means container
/// paths are not mapped to the host.
pub fn parse_mounts(json: &str) -> Vec<Mount> {
    serde_json::from_str::<Vec<InspectMount>>(json.trim())
        .unwrap_or_default()
        .into_iter()
        .filter(|m| !m.destination.is_empty())
        .map(|m| Mount {
            source: if m.kind == "bind" { m.source } else { String::new() },
            destination: m.destination,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_reports_file_dir_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "fn main() {}").unwrap();
        let base = dir.path();

        let file = resolve(Some(base), "src/main.rs", None);
        assert!(file.exists);
        assert_eq!(file.kind, Some(PathKind::File));
        assert_eq!(PathBuf::from(&file.path), base.join("src").join("main.rs"));

        let folder = resolve(Some(base), "./src", None);
        assert_eq!(folder.kind, Some(PathKind::Dir));
        assert!(folder.exists);

        let missing = resolve(Some(base), "src/gone.rs", None);
        assert!(!missing.exists);
        assert_eq!(missing.kind, None);
        assert_eq!(PathBuf::from(&missing.path), base.join("src").join("gone.rs"));
    }

    #[test]
    fn resolve_collapses_dot_segments_and_keeps_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("x.txt"), "").unwrap();
        let base = dir.path().join("a");
        let up = resolve(Some(&base), "../x.txt", None);
        assert!(up.exists);
        assert_eq!(PathBuf::from(&up.path), dir.path().join("x.txt"));

        let absolute = dir.path().join("x.txt");
        let other_base = dir.path().join("unrelated");
        let abs = resolve(Some(&other_base), &absolute.to_string_lossy(), None);
        assert!(abs.exists, "an absolute path ignores the base");
    }

    #[test]
    fn resolve_expands_home_and_refuses_a_relative_path_without_a_base() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("notes.md"), "").unwrap();
        let hit = resolve(None, "~/notes.md", Some(home.path()));
        assert!(hit.exists);
        assert_eq!(PathBuf::from(&hit.path), home.path().join("notes.md"));
        let backslash = resolve(None, r"~\notes.md", Some(home.path()));
        assert!(backslash.exists);

        let no_home = resolve(None, "~/notes.md", None);
        assert!(!no_home.exists);
        assert_eq!(no_home.path, "~/notes.md");

        let no_base = resolve(None, "src/main.rs", None);
        assert!(!no_base.exists, "the app's own cwd is not the output's");
        assert_eq!(no_base.path, "src/main.rs");
        assert!(!resolve(None, "   ", None).exists);
    }

    #[test]
    fn parent_segments_stop_at_the_root() {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        let deep = Path::new(root).join("a").join("..").join("..").join("b");
        assert_eq!(normalize(&deep), Path::new(root).join("b"));
    }

    #[test]
    fn template_keeps_a_spaced_path_as_one_argument() {
        let argv = editor_argv("code --goto {path}:{line}:{col}", r"C:\code\my proj\src\app.ts", 12, 5);
        assert_eq!(argv, vec!["code", "--goto", r"C:\code\my proj\src\app.ts:12:5"]);
    }

    #[test]
    fn template_never_resplits_or_resubstitutes_the_path() {
        // A hostile file name: quotes, a fake flag and placeholder text. It
        // must arrive as exactly one argument, verbatim.
        let path = r#"/home/dev/x" --new-window "{line} {col}.rs"#;
        let argv = editor_argv("subl {path}:{line}:{col}", path, 3, 4);
        assert_eq!(argv.len(), 2, "{argv:?}");
        assert_eq!(argv[1], format!("{path}:3:4"));
    }

    #[test]
    fn template_location_defaults_and_quoted_program() {
        // No location in the output: the caller passes 1, and 0 clamps to 1.
        assert_eq!(editor_argv("zed {path}:{line}:{col}", "/w/a.rs", 0, 0), vec!["zed", "/w/a.rs:1:1"]);
        // A quoted program path with spaces is one word; a lone brace is text.
        assert_eq!(
            editor_argv(r#""C:\Program Files\Ed\ed.exe" -l {line} {path} {x}"#, r"C:\w\a.rs", 7, 1),
            vec![r"C:\Program Files\Ed\ed.exe", "-l", "7", r"C:\w\a.rs", "{x}"]
        );
    }

    #[test]
    fn template_without_a_path_placeholder_appends_the_path() {
        assert_eq!(editor_argv("notepad", r"C:\w\a b.txt", 1, 1), vec!["notepad", r"C:\w\a b.txt"]);
    }

    #[cfg(windows)]
    #[test]
    fn bare_program_resolves_to_its_cmd_shim_not_the_sh_script() {
        let dir = tempfile::tempdir().unwrap();
        // The launcher layout editors install: an extensionless sh script
        // beside the `.cmd` shim.
        std::fs::write(dir.path().join("code"), "#!/bin/sh").unwrap();
        std::fs::write(dir.path().join("code.cmd"), "@echo off").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        let pathext = std::ffi::OsStr::new(".COM;.EXE;.BAT;.CMD");
        assert_eq!(
            find_with_pathext("code", Some(&path), Some(pathext)),
            Some(dir.path().join("code.CMD"))
        );
        // Named with an extension, or as a path: used as written.
        assert_eq!(find_with_pathext("code.cmd", Some(&path), Some(pathext)), None);
        assert_eq!(find_with_pathext(r"C:\x\code", Some(&path), Some(pathext)), None);
        assert_eq!(find_with_pathext("nothing-here", Some(&path), Some(pathext)), None);
    }

    #[cfg(windows)]
    #[test]
    fn default_handler_refuses_runnable_files() {
        for name in ["setup.exe", "run.CMD", "x.ps1", "s.vbs", "link.lnk", "a.js"] {
            assert!(default_open_refusal(Path::new(name), PathKind::File).is_some(), "{name}");
        }
        for name in ["main.rs", "README", "notes.md", "app.ts"] {
            assert!(default_open_refusal(Path::new(name), PathKind::File).is_none(), "{name}");
        }
        assert!(default_open_refusal(Path::new(r"C:\w\tools.exe"), PathKind::Dir).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn default_handler_refuses_executables_and_launchers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("build");
        std::fs::write(&script, "#!/bin/sh").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(default_open_refusal(&script, PathKind::File).is_some());
        let text = dir.path().join("notes.md");
        std::fs::write(&text, "").unwrap();
        assert!(default_open_refusal(&text, PathKind::File).is_none());
        assert!(default_open_refusal(Path::new("/Applications/X.app"), PathKind::Dir).is_some());
    }

    /// Captured `docker inspect -f '{{json .Mounts}}'` shape (Docker Desktop
    /// on Windows): a drive bind, a bind reported through the VM's view of
    /// the drive, a named volume and a tmpfs.
    const INSPECT_MOUNTS: &str = r#"[
        {"Type":"bind","Source":"C:\\code\\app","Destination":"/workspace/app","Mode":"","RW":true,"Propagation":"rprivate"},
        {"Type":"bind","Source":"/run/desktop/mnt/host/c/code/wt","Destination":"/workspace/wt","Mode":"","RW":true,"Propagation":"rprivate"},
        {"Type":"volume","Name":"deps","Source":"/var/lib/docker/volumes/deps/_data","Destination":"/workspace/app/node_modules","Driver":"local","Mode":"z","RW":true,"Propagation":""},
        {"Type":"tmpfs","Source":"","Destination":"/scratch","Mode":"","RW":true,"Propagation":""}
    ]"#;

    #[test]
    fn mounts_parse_from_docker_inspect() {
        let mounts = parse_mounts(INSPECT_MOUNTS);
        let pairs: Vec<(&str, &str)> = mounts
            .iter()
            .map(|m| (m.source.as_str(), m.destination.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                (r"C:\code\app", "/workspace/app"),
                ("/run/desktop/mnt/host/c/code/wt", "/workspace/wt"),
                ("", "/workspace/app/node_modules"),
                ("", "/scratch"),
            ]
        );
    }

    #[test]
    fn mounts_tolerate_garbage() {
        assert!(parse_mounts("").is_empty());
        assert!(parse_mounts("null").is_empty());
        assert!(parse_mounts("[]").is_empty());
        assert!(parse_mounts("not json").is_empty());
    }
}
