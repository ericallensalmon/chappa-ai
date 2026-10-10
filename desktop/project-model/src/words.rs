//! Word splitting and quoting for command lines the user types or pastes.
//!
//! One pair of rules shared by everything that turns a list of words into a
//! line for display and back: [`quote_word`] renders, [`shell_words`] parses,
//! and `shell_words(&join_words(w)) == w` for every word list.

/// Quote one word for display when it carries whitespace or quotes (double-
/// quote style, `\"` and `\\` escaped inside; `render → parse` round-trips
/// through [`shell_words`]). A bare word — Windows paths with backslashes
/// included — is left alone: backslash is literal outside quotes.
pub fn quote_word(word: &str) -> String {
    if word.is_empty() || word.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
        let escaped = word.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        word.to_owned()
    }
}

/// Render a word list as one line, each word quoted as needed.
pub fn join_words<S: AsRef<str>>(words: &[S]) -> String {
    words.iter().map(|w| quote_word(w.as_ref())).collect::<Vec<_>>().join(" ")
}

/// Shell-words split: whitespace separates, single quotes are literal, double
/// quotes allow `\"` and `\\`. A backslash OUTSIDE quotes is a literal
/// character on every platform — this is not a POSIX shell, and the strings
/// split here are Windows command lines as often as not
/// (`D:\tools\npm\claude.cmd`); eating the backslashes mangled every path.
/// No expansion of any kind.
pub fn shell_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(n @ ('"' | '\\')) => cur.push(n),
                            Some(n) => {
                                cur.push('\\');
                                cur.push(n);
                            }
                            None => cur.push('\\'),
                        },
                        other => cur.push(other),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                cur.push(other);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_then_split_is_the_identity() {
        let cases: [&[&str]; 5] = [
            &["cargo", "test", "--no-fail-fast"],
            &[r"D:\tools\x y\tool.exe", "-v"],
            &["py", "-c", r#"print("hi")"#],
            &["echo", "", "it's"],
            &["a\\", r#"\""#, "tab\there"],
        ];
        for words in cases {
            let line = join_words(words);
            assert_eq!(shell_words(&line), words, "round-trip of {line}");
        }
    }
}
