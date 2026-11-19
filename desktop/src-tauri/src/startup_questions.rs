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

/// Shapes of a question that waits for a one-key answer, recognised on any
/// program's screen rather than by its wording. What an agent sends another
/// terminal is held while one of these (or a known question above) is on
/// screen, so an agent cannot answer a permission or confirmation prompt
/// that was put to the person.
pub const CONFIRMATION_SHAPES: &[StartupQuestion] = &[
    StartupQuestion {
        id: "numbered-choice",
        label: "a numbered choice",
        all: &[],
        any: &[],
    },
    StartupQuestion {
        id: "yes-no",
        label: "a yes/no question",
        all: &[],
        any: &[],
    },
    StartupQuestion {
        id: "allow-deny",
        label: "an allow/deny question",
        all: &[],
        any: &[],
    },
];

/// How many of the last non-blank rows the shape detector reads: a prompt
/// waiting for an answer sits at the bottom of the screen; a numbered list
/// scrolled far up is history.
pub const CONFIRMATION_ROWS: usize = 15;

fn shape(id: &str) -> &'static StartupQuestion {
    CONFIRMATION_SHAPES
        .iter()
        .find(|q| q.id == id)
        .unwrap_or(&CONFIRMATION_SHAPES[0])
}

/// The number a menu row starts with: `1. Yes`, `❯ 2) No`, `> 3. Later`.
fn menu_number(row: &str) -> Option<(u32, bool)> {
    let mut rest = row.trim_start();
    let mut cursor = false;
    if let Some(c) = rest.chars().next() {
        if matches!(c, '❯' | '›' | '>' | '▶' | '→' | '*' | '●' | '○' | '◯' | '◉') {
            cursor = true;
            rest = rest[c.len_utf8()..].trim_start();
        }
    }
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let after = &rest[digits.len()..];
    let mut chars = after.chars();
    if !matches!(chars.next(), Some('.' | ')')) {
        return None;
    }
    // `1.5` is a number, not a menu entry; the entry has text after a space.
    if !matches!(chars.next(), Some(' ' | '\t')) || chars.as_str().trim().is_empty() {
        return None;
    }
    Some((digits.parse().ok()?, cursor))
}

fn has_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// A confirmation-shaped state on `lines` (a dump of the visible grid): a
/// known startup question, or, on the last [`CONFIRMATION_ROWS`] non-blank
/// rows, a numbered choice list (`1.` and `2.` entries, or a cursor on a
/// numbered entry), a `(y/n)`-style question, or an Allow / Deny choice.
pub fn detect_confirmation(lines: &[String]) -> Option<&'static StartupQuestion> {
    if let Some(q) = detect_startup_question(lines) {
        return Some(q);
    }
    let rows: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail = &rows[rows.len().saturating_sub(CONFIRMATION_ROWS)..];
    let numbers: Vec<(u32, bool)> = tail.iter().filter_map(|r| menu_number(r)).collect();
    let has = |n: u32| numbers.iter().any(|(m, _)| *m == n);
    if numbers.iter().any(|(_, cursor)| *cursor) || (has(1) && has(2)) {
        return Some(shape("numbered-choice"));
    }
    let text = tail.join("\n").to_lowercase();
    let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if [
        "(y/n)", "[y/n]", "(yes/no)", "[yes/no]", "(y/n/a)", "[y/n/a]",
    ]
    .iter()
    .any(|n| squeezed.contains(n))
    {
        return Some(shape("yes-no"));
    }
    if has_word(&text, "allow")
        && (has_word(&text, "deny") || text.contains("don't allow") || has_word(&text, "reject"))
    {
        return Some(shape("allow-deny"));
    }
    None
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

    /// Any program's one-key question is recognised by its shape at the
    /// bottom of the screen, and ordinary output is not.
    #[test]
    fn confirmation_shapes_are_recognised_on_the_last_rows() {
        let id = |text: &str| detect_confirmation(&screen(text)).map(|q| q.id);
        assert_eq!(id(FOLDER_TRUST), Some("folder-trust"));
        assert_eq!(
            id("Run this command?\n  1. Yes\n  2. Yes, and don't ask again\n  3. No"),
            Some("numbered-choice")
        );
        assert_eq!(id("Pick one\n ❯ 1. Apply"), Some("numbered-choice"));
        assert_eq!(id("Overwrite file.txt? (y/N)"), Some("yes-no"));
        assert_eq!(id("Continue? [ Y / n ]"), Some("yes-no"));
        assert_eq!(
            id("The tool wants to read ~/.ssh\n  Allow    Deny"),
            Some("allow-deny")
        );
        // Ordinary output: a decimal, a sentence about allowing, one list item.
        assert_eq!(id("version 1.5 installed\n$ "), None);
        assert_eq!(id("this setting allows more memory\n$ "), None);
        assert_eq!(id("1. only one entry\n$ "), None);
        assert_eq!(id(""), None);
        // A choice that has scrolled far above the bottom is history.
        let mut long = String::from("1. Yes\n2. No\n");
        for n in 0..CONFIRMATION_ROWS {
            long.push_str(&format!("line {n}\n"));
        }
        assert_eq!(id(&long), None);
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
