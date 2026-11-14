//! How long one step of a job may take. All in one place, so the numbers
//! can be read (and argued with) together.
//!
//! | step | limit | why |
//! |---|---|---|
//! | compose `up` / `start` / `restart` | 15 min | creating a container can include a first image layer extraction and a healthcheck-gated dependency |
//! | compose `stop` / `down` / `rm` | 5 min | the longest grace period in a sane definition, plus removal |
//! | image pull | 30 min | big images over slow links |
//! | source build | 60 min | a cold build of a large service |
//! | process start | 2 min | spawning the dev command and seeing it survive its first moments |
//! | anything else (gc, prune, hooks) | 5 min | |
//!
//! A step that runs out of time is killed and fails the job with a line in
//! its log naming the limit, so a hung engine never leaves a job "running"
//! forever.

use std::time::Duration;

use stack_core::jobs::RunBlock;

pub const COMPOSE_UP: Duration = Duration::from_secs(15 * 60);
pub const COMPOSE_STOP: Duration = Duration::from_secs(5 * 60);
pub const IMAGE_PULL: Duration = Duration::from_secs(30 * 60);
pub const SOURCE_BUILD: Duration = Duration::from_secs(60 * 60);
pub const PROCESS_START: Duration = Duration::from_secs(2 * 60);
pub const OTHER: Duration = Duration::from_secs(5 * 60);

/// What kind of step a run block is, read from its argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    ComposeUp,
    ComposeStop,
    Pull,
    Build,
    ProcessStart,
    Other,
}

impl StepKind {
    /// Classify an engine command. Process starts are told apart by the
    /// caller (they carry the instance environment), not by argv.
    pub fn of(run: &RunBlock) -> StepKind {
        let words: Vec<&str> = run.argv.iter().map(String::as_str).collect();
        let compose = words.contains(&"compose");
        // The verb is the first word after the global flags: skip
        // `--context X`, `--connection X`, `-p X`, `-f X`.
        let mut verb = None;
        let mut i = 1;
        while i < words.len() {
            match words[i] {
                "--context" | "--connection" | "-p" | "-f" => i += 2,
                "compose" => i += 1,
                w if w.starts_with('-') => i += 1,
                w => {
                    verb = Some(w);
                    break;
                }
            }
        }
        match (compose, verb) {
            (true, Some("up" | "start" | "restart" | "create")) => StepKind::ComposeUp,
            (true, Some("stop" | "down" | "rm" | "kill")) => StepKind::ComposeStop,
            (_, Some("pull")) => StepKind::Pull,
            (_, Some("build")) => StepKind::Build,
            _ => StepKind::Other,
        }
    }

    pub fn limit(self) -> Duration {
        match self {
            StepKind::ComposeUp => COMPOSE_UP,
            StepKind::ComposeStop => COMPOSE_STOP,
            StepKind::Pull => IMAGE_PULL,
            StepKind::Build => SOURCE_BUILD,
            StepKind::ProcessStart => PROCESS_START,
            StepKind::Other => OTHER,
        }
    }
}

/// `15 min`, `90 s`.
pub fn describe(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 && s.is_multiple_of(60) {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(argv: &[&str]) -> StepKind {
        StepKind::of(&RunBlock::new(
            argv.iter().map(|s| s.to_string()).collect(),
            ".",
        ))
    }

    #[test]
    fn steps_are_classified_by_their_engine_verb() {
        assert_eq!(
            kind(&["docker", "compose", "-p", "x", "-f", "a.yml", "-f", "o.yml", "up", "-d"]),
            StepKind::ComposeUp
        );
        assert_eq!(
            kind(&[
                "podman",
                "--connection",
                "m",
                "compose",
                "-p",
                "x",
                "stop",
                "-t",
                "10",
                "db"
            ]),
            StepKind::ComposeStop
        );
        assert_eq!(
            kind(&["docker", "compose", "-p", "x", "down"]),
            StepKind::ComposeStop
        );
        assert_eq!(
            kind(&["docker", "pull", "postgres@sha256:aa"]),
            StepKind::Pull
        );
        assert_eq!(
            kind(&["podman", "build", "-t", "api:01234567", "."]),
            StepKind::Build
        );
        assert_eq!(kind(&["docker", "builder", "prune", "-f"]), StepKind::Other);
        assert_eq!(StepKind::Pull.limit(), IMAGE_PULL);
        assert_eq!(describe(COMPOSE_UP), "15 min");
        assert_eq!(describe(Duration::from_secs(90)), "90 s");
    }
}
