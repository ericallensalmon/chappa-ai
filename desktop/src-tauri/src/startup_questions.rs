//! Startup and permission questions an agent CLI can put on screen before
//! it is ready for a prompt. The ready gate types a queued prompt once the
//! pty has been quiet for a moment; a dialog that waits for a keypress is
//! exactly as quiet as a ready prompt, so without this check the Enter at
//! the end of a first prompt would answer the dialog instead (and "Yes,
//! proceed" or "No, exit" is not something a prompt should decide).
//!
//! Detection reads the rendered grid text, the same way the other screen
//! checks do, against a small list of needles kept HERE and nowhere else.
//! A match holds delivery; the hold never times out into typing — at the
//! cap it gives up and the receipt says which question was on screen.

/// One known question: a stable id for receipts and tests, a short label
/// for people, and the phrases that identify it. Every needle in `all` must
/// appear (lowercase, whitespace-normalised) and at least one of `any`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupQuestion {
    pub id: &'static str,
    pub label: &'static str,
    all: &'static [&'static str],
    any: &'static [&'static str],
}

/// The list. Order matters only when two match at once (the first wins),
/// which does not happen with these phrases.
pub const STARTUP_QUESTIONS: &[StartupQuestion] = &[
    StartupQuestion {
        id: "folder-trust",
        label: "the folder-trust question",
        all: &["trust the files in this folder"],
        any: &["yes, proceed", "no, exit", "enter to confirm"],
    },
    StartupQuestion {
        id: "mcp-server-approval",
        label: "the new MCP server question",
        all: &["mcp server"],
        any: &[
            "use this and all future mcp servers",
            "use this mcp server",
            "continue without using this mcp server",
        ],
    },
    StartupQuestion {
        id: "external-claude-md",
        label: "the external CLAUDE.md import question",
        all: &["claude.md"],
        any: &["yes, import", "imports a file", "from outside the project"],
    },
    StartupQuestion {
        id: "skip-permissions",
        label: "the bypass-permissions warning",
        all: &["bypass permissions"],
        any: &["yes, i accept", "no, exit", "dangerously-skip-permissions"],
    },
];

/// Lowercase and collapse every whitespace run (box-drawing padding, line
/// wraps, the selection arrow's spaces) so a needle matches however the
/// TUI laid the sentence out.
fn normalise(lines: &[String]) -> String {
    let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    let mut pending_space = false;
    for ch in lines
        .iter()
        .flat_map(|l| l.chars().chain(std::iter::once('\n')))
    {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        for lower in ch.to_lowercase() {
            out.push(lower);
        }
    }
    out
}

/// The question on `lines` (a dump of the visible grid), if any.
pub fn detect_startup_question(lines: &[String]) -> Option<&'static StartupQuestion> {
    let text = normalise(lines);
    if text.is_empty() {
        return None;
    }
    STARTUP_QUESTIONS
        .iter()
        .find(|q| q.all.iter().all(|n| text.contains(n)) && q.any.iter().any(|n| text.contains(n)))
}

/// The receipt reason for a prompt that was still held when the gate's cap
/// passed: `held: waiting on <label>`.
pub fn held_reason(question: &StartupQuestion) -> String {
    format!("held: waiting on {}", question.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    /// The folder-trust dialog as Claude Code renders it in a folder it has
    /// not seen before (the question the first prompt's Enter used to answer).
    const FOLDER_TRUST: &str = "\
 Do you trust the files in this folder?

 C:\\code\\worktrees\\feature-x

 Claude Code may read files in this folder. Reading untrusted files may lead
 Claude Code to behave in unexpected ways.

 With your permission Claude Code may execute files in this folder. Executing
 untrusted code is unsafe.

 ❯ 1. Yes, proceed
   2. No, exit

 Enter to confirm · Esc to exit";

    const MCP_APPROVAL: &str = "\
 New MCP server found in .mcp.json: docs

 MCP servers may execute code or access system resources. All tool calls
 require approval. Learn more in the MCP documentation.

 ❯ 1. Use this and all future MCP servers in this project
   2. Use this MCP server
   3. Continue without using this MCP server

 Enter to confirm · Esc to reject";

    const CLAUDE_MD_IMPORT: &str = "\
 This project's CLAUDE.md imports a file from outside the project:

   ../shared/CLAUDE.md

 Importing files from outside the project could lead Claude Code to behave
 in unexpected ways.

 ❯ 1. Yes, import it
   2. No, skip it

 Enter to confirm · Esc to exit";

    const BYPASS_PERMISSIONS: &str = "\
 WARNING: Claude Code running in Bypass Permissions mode

 In Bypass Permissions mode, Claude Code will not ask for your approval
 before running potentially dangerous commands.
 This mode should only be used in a sandboxed container/VM that has
 restricted internet access and can easily be restored if damaged.

 By proceeding, you accept all responsibility for actions taken while
 running in Bypass Permissions mode.

   1. No, exit
 ❯ 2. Yes, I accept

 Enter to confirm · Esc to exit";

    #[test]
    fn each_recorded_question_is_recognised_by_id() {
        for (text, id) in [
            (FOLDER_TRUST, "folder-trust"),
            (MCP_APPROVAL, "mcp-server-approval"),
            (CLAUDE_MD_IMPORT, "external-claude-md"),
            (BYPASS_PERMISSIONS, "skip-permissions"),
        ] {
            let q = detect_startup_question(&screen(text))
                .unwrap_or_else(|| panic!("{id} not detected"));
            assert_eq!(q.id, id);
            assert_eq!(held_reason(q), format!("held: waiting on {}", q.label));
        }
    }

    #[test]
    fn a_ready_prompt_an_ordinary_transcript_and_an_empty_screen_are_not_questions() {
        let ready = screen(
            "╭──────────────────────────────────────╮\n\
             │ ✻ Welcome to Claude Code!            │\n\
             │   /help for help, /status for status │\n\
             │   cwd: C:\\code\\app                   │\n\
             ╰──────────────────────────────────────╯\n\
             \n\
             > ",
        );
        assert!(detect_startup_question(&ready).is_none());
        // Talking ABOUT the dialogs is not being asked one: each needle set
        // needs the question's own phrasing, not a single keyword.
        let chatter = screen(
            "I read the CLAUDE.md and the .mcp.json; the mcp server config looks fine.\n\
             Nothing here asks you to trust the folder.",
        );
        assert!(detect_startup_question(&chatter).is_none());
        assert!(detect_startup_question(&[]).is_none());
        assert!(detect_startup_question(&screen("   \n\t\n")).is_none());
    }

    #[test]
    fn detection_survives_wrapping_case_and_padding() {
        // A narrow pane wraps the sentence across lines and box padding
        // adds runs of spaces; capitalisation varies between versions.
        let wrapped = screen(
            "   DO YOU TRUST THE FILES\n\
             \t   IN THIS FOLDER?\n\
             \n\
             ❯ 1.   Yes,   proceed",
        );
        assert_eq!(
            detect_startup_question(&wrapped).unwrap().id,
            "folder-trust"
        );
        assert_eq!(normalise(&screen("  A  b\n\n c ")), "a b c");
    }

    #[test]
    fn the_list_names_the_four_known_questions_once_each() {
        let ids: Vec<&str> = STARTUP_QUESTIONS.iter().map(|q| q.id).collect();
        assert_eq!(
            ids,
            [
                "folder-trust",
                "mcp-server-approval",
                "external-claude-md",
                "skip-permissions"
            ]
        );
        for q in STARTUP_QUESTIONS {
            assert!(!q.all.is_empty() && !q.any.is_empty(), "{}", q.id);
            assert!(
                q.all.iter().chain(q.any).all(|n| *n == n.to_lowercase()),
                "{}: needles are lowercase",
                q.id
            );
        }
    }
}
