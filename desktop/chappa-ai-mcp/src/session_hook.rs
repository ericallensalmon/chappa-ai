//! `chappa-ai-mcp session-hook`: a Claude Code `SessionStart` hook that tells
//! the app which conversation the agent row is having, so the row can be
//! resumed later.
//!
//! Claude Code names its session id only in its machine-readable output; in
//! the terminal UI it prints nothing a regex could catch. Its `SessionStart`
//! hook, though, fires on launch, `--resume`, `/clear` and compaction, with a
//! JSON object on stdin:
//!
//! ```json
//! {"session_id": "9e93b89b-…", "transcript_path": "…", "cwd": "…",
//!  "hook_event_name": "SessionStart", "source": "startup"}
//! ```
//!
//! (`source` is `startup`, `resume`, `clear` or `compact`; other fields come
//! and go between versions and are ignored.) The app exports
//! `CHAPPA_AI_PROCESS_ID` to every process it spawns, so the hook posts
//! `{session_id, provider}` to `POST /processes/{id}/session` through the
//! same verified client the MCP tools use: the identity check from the
//! endpoint file first, the token only after it. Each report replaces the
//! last, so after `/clear` the saved id is the new conversation.
//!
//! Contract with Claude Code: the hook never blocks or disturbs the agent.
//! It always exits 0, writes nothing to stdout (a `SessionStart` hook's
//! stdout is added to the model's context), reports problems on stderr only,
//! and gives up after [`TOTAL_DEADLINE`]. Outside the app (no process id, no
//! endpoint file) it does nothing at all.

use std::io::Read;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::{Client, Target};

/// The subcommand word.
pub const SUBCOMMAND: &str = "session-hook";
/// The provider named in the report.
pub const PROVIDER: &str = "claude";
/// How long the connect may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// How long any single read may stall.
pub const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// The whole hook, stdin included, never runs longer than this.
pub const TOTAL_DEADLINE: Duration = Duration::from_secs(2);
/// More hook input than this is not a `SessionStart` payload.
const MAX_INPUT: u64 = 256 * 1024;

/// What one hook run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Not running under the app (no process id, or no endpoint file):
    /// nothing was read or sent.
    Skipped,
    /// The input held no usable `session_id`; nothing was sent.
    NoSessionId,
    /// The app took the id.
    Reported,
    /// The report did not land (the reason, for stderr).
    Failed(String),
}

/// The `session_id` out of the hook's stdin JSON, trimmed; `None` for
/// anything that is not an object carrying a non-empty string id.
pub fn parse_session_id(input: &str) -> Option<String> {
    let value: Value = serde_json::from_str(input.trim()).ok()?;
    let id = value.get("session_id")?.as_str()?.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// `CHAPPA_AI_PROCESS_ID` as a process id: a plain non-negative integer,
/// anything else means the hook was not started by the app.
pub fn parse_process_id(raw: Option<&str>) -> Option<u64> {
    raw?.trim().parse().ok()
}

/// One hook run against `client` for `process_id`, reading the hook JSON
/// from `input`. Pure apart from the one request, so it is testable without
/// touching the process environment.
pub fn run(process_id: Option<u64>, client: &Client, input: &mut dyn Read) -> HookOutcome {
    let Some(process_id) = process_id else {
        return HookOutcome::Skipped;
    };
    if let Target::EndpointFile(path) = &client.target {
        if !Path::new(path).is_file() {
            return HookOutcome::Skipped;
        }
    }
    let mut text = String::new();
    if input.take(MAX_INPUT).read_to_string(&mut text).is_err() {
        return HookOutcome::NoSessionId;
    }
    let Some(session_id) = parse_session_id(&text) else {
        return HookOutcome::NoSessionId;
    };
    let body = json!({"session_id": session_id, "provider": PROVIDER});
    match client.request("POST", &format!("/processes/{process_id}/session"), Some(&body)) {
        Ok(_) => HookOutcome::Reported,
        Err(err) => HookOutcome::Failed(err.to_string()),
    }
}

/// The subcommand's entry point: environment → [`run`] under the overall
/// deadline. Returns normally in every case (the caller exits 0).
pub fn main() {
    let process_id = parse_process_id(std::env::var("CHAPPA_AI_PROCESS_ID").ok().as_deref());
    if process_id.is_none() {
        return;
    }
    let client = Client::from_env().with_timeouts(CONNECT_TIMEOUT, READ_TIMEOUT);
    let (tx, rx) = mpsc::channel();
    // The work runs on its own thread so a stalled stdin or server cannot
    // hold the agent's startup past the deadline: returning from `main`
    // ends the process with the worker still blocked.
    std::thread::spawn(move || {
        let outcome = run(process_id, &client, &mut std::io::stdin().lock());
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(TOTAL_DEADLINE) {
        Ok(HookOutcome::Skipped | HookOutcome::Reported) => {}
        Ok(HookOutcome::NoSessionId) => {
            eprintln!("chappa-ai-mcp {SUBCOMMAND}: the hook input carried no session_id; nothing reported");
        }
        Ok(HookOutcome::Failed(why)) => eprintln!("chappa-ai-mcp {SUBCOMMAND}: {why}"),
        Err(_) => eprintln!(
            "chappa-ai-mcp {SUBCOMMAND}: gave up after {} s; the session id was not reported",
            TOTAL_DEADLINE.as_secs()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TokenSource;

    #[test]
    fn the_session_id_is_read_from_the_hook_payload_and_unknown_fields_are_ignored() {
        let payload = r#"{"session_id":"9e93b89b-6d27-456e-8591-58f578689d2e","transcript_path":"t.jsonl",
            "cwd":"/w","hook_event_name":"SessionStart","source":"clear","model":{"id":"m"},"extra":[1,2]}"#;
        assert_eq!(parse_session_id(payload).as_deref(), Some("9e93b89b-6d27-456e-8591-58f578689d2e"));
        // Surrounding whitespace in the stream and in the id itself.
        assert_eq!(parse_session_id("\n {\"session_id\": \"  abc \"}\n").as_deref(), Some("abc"));
        for bad in ["", "not json", "[]", "\"abc\"", r#"{"session_id":""}"#, r#"{"session_id":"  "}"#,
            r#"{"session_id":42}"#, r#"{"session_id":null}"#, r#"{"hook_event_name":"SessionStart"}"#]
        {
            assert_eq!(parse_session_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_process_id_must_be_a_plain_integer() {
        assert_eq!(parse_process_id(Some("12")), Some(12));
        assert_eq!(parse_process_id(Some(" 7\n")), Some(7));
        for bad in [None, Some(""), Some("stale"), Some("-1"), Some("1.5")] {
            assert_eq!(parse_process_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn outside_the_app_nothing_is_read_or_sent() {
        // A reader that fails the test if it is touched.
        struct Untouchable;
        impl Read for Untouchable {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("stdin must not be read when the hook is skipped");
            }
        }
        let missing = std::env::temp_dir().join(format!("chappa-ai-mcp-no-endpoint-{}", std::process::id()));
        let client = Client::from_endpoint_file(missing.join("control_endpoint"), TokenSource::Literal("t".into()));
        // No process id.
        assert_eq!(run(None, &client, &mut Untouchable), HookOutcome::Skipped);
        // A process id, but no app has written its endpoint file.
        assert_eq!(run(Some(12), &client, &mut Untouchable), HookOutcome::Skipped);
    }
}
