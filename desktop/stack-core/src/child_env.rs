//! Variables no child process of ours may inherit.
//!
//! A source learns its loopback address and bearer token from its own
//! environment when the app launches it. Everything the source starts
//! (scripts, process components, hooks, git, the secret scanner, an engine
//! CLI) would inherit them, and code running there (a dev server built from
//! a worktree an agent edits, say) could call the source's API with the
//! person's authority. So the source reads them once at startup with
//! [`take_from_process_env`] and every spawn removes them again with
//! [`scrub`], in case anything put them back.
//!
//! gitleaks' own config variables go too: a scan uses the config the caller
//! chose and nothing a parent shell happened to set.

use std::process::Command;

/// The source's loopback address, set by the app on launch.
pub const SOURCE_ADDR_ENV: &str = "CHAPPA_SOURCE_ADDR";
/// The source's bearer token, set by the app on launch.
pub const SOURCE_TOKEN_ENV: &str = "CHAPPA_SOURCE_TOKEN";
/// gitleaks reads a config path or a whole config from these, ahead of the
/// default rules.
pub const GITLEAKS_CONFIG_ENVS: [&str; 2] = ["GITLEAKS_CONFIG", "GITLEAKS_CONFIG_TOML"];

/// Every variable [`scrub`] removes.
pub const SCRUBBED: [&str; 4] = [
    SOURCE_ADDR_ENV,
    SOURCE_TOKEN_ENV,
    GITLEAKS_CONFIG_ENVS[0],
    GITLEAKS_CONFIG_ENVS[1],
];

/// Remove every [`SCRUBBED`] variable from what `cmd` passes on.
pub fn scrub(cmd: &mut Command) -> &mut Command {
    for name in SCRUBBED {
        cmd.env_remove(name);
    }
    cmd
}

/// Read `name` from this process's environment and remove it, so nothing
/// started later inherits it. Call before any thread is started.
pub fn take_from_process_env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok();
    std::env::remove_var(name);
    value
}

/// Remove every [`SCRUBBED`] variable from this process's environment.
/// Call before any thread is started.
pub fn scrub_process_env() {
    for name in SCRUBBED {
        std::env::remove_var(name);
    }
}

/// A command that prints its environment, one `NAME=value` per line: what
/// tests in this crate and its users spawn to show what a child sees.
#[doc(hidden)]
pub fn env_printer() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), "set".into()]
    } else {
        vec!["env".into()]
    }
}

/// Whether printed environment `text` (from [`env_printer`]) names any
/// scrubbed variable.
#[doc(hidden)]
pub fn names_a_scrubbed_variable(text: &str) -> Option<&'static str> {
    SCRUBBED.into_iter().find(|name| {
        text.lines().any(|l| {
            l.split_once('=')
                .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scrubbed_command_does_not_pass_the_variables_on() {
        // Set for this test process: an unscrubbed child would see them.
        for name in SCRUBBED {
            std::env::set_var(name, "planted-for-the-test");
        }
        let argv = env_printer();
        let plain = Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
        assert!(names_a_scrubbed_variable(&String::from_utf8_lossy(&plain.stdout)).is_some());
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let out = scrub(&mut cmd).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(names_a_scrubbed_variable(&text), None, "{text}");
        assert!(text
            .lines()
            .any(|l| l.to_ascii_uppercase().starts_with("PATH=")));
    }
}
