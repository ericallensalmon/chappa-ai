//! Helpers for tests in this and the dependent crates. Compiled only for this
//! crate's own tests or with the `test-support` feature, which dependents
//! enable from `[dev-dependencies]` alone.

use std::path::Path;

/// Write `body` to `path` as an executable that can be run straight away.
///
/// Writing an executable and then running it from a test process races on
/// Linux: while the file is open for writing here, another test thread may
/// fork to start a child of its own, and that child holds a copy of the write
/// descriptor until it execs. Running the file inside that window fails with
/// "Text file busy" (ETXTBSY). Writing a temporary file and renaming it does
/// not help, since the forked child holds the same file. So this process
/// never opens the executable for writing at all: the body goes to a sibling
/// file that is never run, and a `cp` child creates the executable and has
/// exited before this returns.
///
/// Panics on any failure; it is for tests.
#[cfg(unix)]
pub fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let name = path
        .file_name()
        .expect("an executable needs a file name")
        .to_string_lossy();
    let staged = path.with_file_name(format!(".{name}.body"));
    std::fs::write(&staged, body).expect("write the staged body");
    let status = std::process::Command::new("cp")
        .arg(&staged)
        .arg(path)
        .status()
        .expect("run cp");
    assert!(
        status.success(),
        "cp to {} failed: {status}",
        path.display()
    );
    std::fs::remove_file(&staged).expect("remove the staged body");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("make it executable");
}

/// Write `body` to `path`. Windows has no exec-while-open race: what makes a
/// file runnable there is its extension, not a mode bit.
#[cfg(not(unix))]
pub fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write the executable");
}

#[cfg(all(test, unix))]
mod tests {
    use super::write_executable;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_executable_runs_and_nothing_is_left_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("say hi");
        write_executable(&path, "#!/bin/sh\necho hi\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let out = std::process::Command::new(&path).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["say hi"]);
    }

    /// Many threads each writing and at once running their own script, while
    /// every other thread is forking: the race the helper exists for.
    #[test]
    fn scripts_written_while_other_threads_fork_never_read_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let dir = dir.path().to_owned();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        let path = dir.join(format!("s{t}-{i}"));
                        write_executable(&path, "#!/bin/sh\nexit 0\n");
                        let status = std::process::Command::new(&path).status().unwrap();
                        assert!(status.success());
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }
}
