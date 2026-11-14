//! Path patterns for `.dockerignore` files and for caller excludes.
//!
//! One matcher serves both, with the common glob forms:
//!
//! - `*` matches any run of characters within one path segment, `?` one
//!   character, `[abc]` / `[a-z]` / `[!a-z]` (or `[^a-z]`) a class, and `\`
//!   escapes the next character;
//! - a `**` segment matches any number of segments, zero included;
//! - a leading `/` or `./` and a trailing `/` are ignored, `#` starts a
//!   comment line, blank lines are skipped;
//! - `!pattern` re-includes what an earlier pattern excluded, and the last
//!   pattern that matches decides;
//! - a pattern that matches a directory matches everything beneath it.
//!
//! The two sources differ in one way. In a `.dockerignore` every pattern is
//! anchored at the context root, as the image build reads it (`*.log`
//! matches `a.log`, not `logs/a.log`). Caller excludes follow the gitignore
//! habit instead: a pattern with no `/` inside it matches at any depth
//! (`*.log` matches `logs/a.log` too), and one with a `/` is anchored.
//!
//! Not supported: per-Dockerfile ignore files (`Dockerfile.dockerignore`)
//! and the build's special case that always sends the Dockerfile itself.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    AnyDepth,
    Glob(Vec<char>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    segments: Vec<Segment>,
    negate: bool,
}

impl Pattern {
    fn parse(line: &str, anywhere: bool) -> Option<Pattern> {
        let mut text = line.trim();
        if text.is_empty() || text.starts_with('#') {
            return None;
        }
        let negate = if let Some(rest) = text.strip_prefix('!') {
            text = rest.trim_start();
            true
        } else {
            false
        };
        let rooted = text.starts_with('/');
        let parts: Vec<&str> = text
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        if parts.is_empty() {
            return None;
        }
        let mut segments = Vec::new();
        if anywhere && !rooted && parts.len() == 1 {
            segments.push(Segment::AnyDepth);
        }
        for part in parts {
            if part == "**" {
                if segments.last() != Some(&Segment::AnyDepth) {
                    segments.push(Segment::AnyDepth);
                }
            } else {
                segments.push(Segment::Glob(part.chars().collect()));
            }
        }
        Some(Pattern { segments, negate })
    }

    /// True when the pattern matches `path` or one of its parent directories.
    fn matches(&self, path: &[&str]) -> bool {
        (1..=path.len()).any(|k| match_segments(&self.segments, &path[..k]))
    }
}

fn match_segments(pattern: &[Segment], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((Segment::AnyDepth, rest)) => {
            match_segments(rest, path) || (!path.is_empty() && match_segments(pattern, &path[1..]))
        }
        Some((Segment::Glob(glob), rest)) => match path.split_first() {
            Some((first, tail)) => {
                let name: Vec<char> = first.chars().collect();
                match_glob(glob, &name) && match_segments(rest, tail)
            }
            None => false,
        },
    }
}

/// Glob match within one segment. Backtracks only to the last `*`, so the
/// cost is bounded by pattern length times name length.
fn match_glob(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() {
            let step = match pattern[p] {
                '*' => {
                    star = Some((p, n));
                    p += 1;
                    continue;
                }
                '?' => Some(1),
                '[' => match match_class(pattern, p, name[n]) {
                    Some((true, next)) => Some(next - p),
                    Some((false, _)) => None,
                    // an unterminated class is a literal bracket
                    None => (name[n] == '[').then_some(1),
                },
                '\\' if p + 1 < pattern.len() => (pattern[p + 1] == name[n]).then_some(2),
                c => (c == name[n]).then_some(1),
            };
            if let Some(advance) = step {
                p += advance;
                n += 1;
                continue;
            }
        }
        match star {
            Some((sp, sn)) => {
                p = sp + 1;
                n = sn + 1;
                star = Some((sp, sn + 1));
            }
            None => return false,
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Match `c` against the class starting at `pattern[start] == '['`.
/// Returns whether it matched and the index after the closing `]`, or
/// `None` when the class is not terminated.
fn match_class(pattern: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negate = matches!(pattern.get(i), Some('!') | Some('^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pattern.len() {
        let lo = pattern[i];
        if lo == ']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        if pattern.get(i + 1) == Some(&'-') && pattern.get(i + 2).is_some_and(|h| *h != ']') {
            let hi = pattern[i + 2];
            if lo <= c && c <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if lo == c {
                matched = true;
            }
            i += 1;
        }
    }
    None
}

/// An ordered list of patterns; the last one that matches a path decides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatternSet {
    patterns: Vec<Pattern>,
}

impl PatternSet {
    /// Patterns read the way an image build reads `.dockerignore`: one per
    /// line, anchored at the context root.
    pub fn dockerignore(text: &str) -> Self {
        Self {
            patterns: text
                .lines()
                .filter_map(|l| Pattern::parse(l, false))
                .collect(),
        }
    }

    /// Caller excludes: a pattern without an inner `/` matches at any depth.
    pub fn excludes<S: AsRef<str>>(patterns: &[S]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .filter_map(|p| Pattern::parse(p.as_ref(), true))
                .collect(),
        }
    }

    /// Both sets, `self` first, so a pattern in `other` can override it.
    pub fn then(mut self, other: PatternSet) -> Self {
        self.patterns.extend(other.patterns);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether any pattern re-includes. Without one, an excluded directory
    /// can be skipped whole instead of walked.
    pub fn has_negations(&self) -> bool {
        self.patterns.iter().any(|p| p.negate)
    }

    /// Whether `path` (relative, `/`-separated) is excluded.
    pub fn is_excluded(&self, path: &str) -> bool {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return false;
        }
        let mut excluded = false;
        for p in &self.patterns {
            if p.matches(&segments) {
                excluded = !p.negate;
            }
        }
        excluded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(p: &str, n: &str) -> bool {
        let p: Vec<char> = p.chars().collect();
        let n: Vec<char> = n.chars().collect();
        match_glob(&p, &n)
    }

    #[test]
    fn glob_forms() {
        let table = [
            ("*", "anything", true),
            ("*", "", true),
            ("*.log", "a.log", true),
            ("*.log", "a.log.1", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("[ab]x", "bx", true),
            ("[ab]x", "cx", false),
            ("[a-c]x", "cx", true),
            ("[!a-c]x", "cx", false),
            ("[^a-c]x", "dx", true),
            ("[]]", "]", true),
            ("[a-]", "-", true),
            ("a[", "a[", true),
            ("\\*", "*", true),
            ("\\*", "a", false),
            ("*a*b*", "xxaYYbZZ", true),
            ("*a*b*", "xxbYYaZZ", false),
            ("é*", "été", true),
        ];
        for (p, n, want) in table {
            assert_eq!(glob(p, n), want, "{p} vs {n}");
        }
    }

    #[test]
    fn dockerignore_is_anchored_and_last_match_wins() {
        let set = PatternSet::dockerignore(
            "# comment\n\n*.log\n/secrets/\n./build\n**/*.pem\ndocs/**/draft*\n!keep.log\n",
        );
        let table = [
            ("a.log", true),
            ("logs/a.log", false), // anchored: `*.log` is one segment at the root
            ("keep.log", false),   // re-included by the later `!`
            ("secrets/key.txt", true),
            ("secrets", true),
            ("build/out/app", true),
            ("cert.pem", true),
            ("deep/er/cert.pem", true),
            ("docs/draft-1.md", true),
            ("docs/a/b/draft.md", true),
            ("docs/final.md", false),
            ("src/main.rs", false),
        ];
        for (path, want) in table {
            assert_eq!(set.is_excluded(path), want, "{path}");
        }
        assert!(set.has_negations());
    }

    #[test]
    fn negation_can_re_include_inside_an_excluded_directory() {
        let set = PatternSet::dockerignore("config\n!config/public.json\n");
        assert!(set.is_excluded("config/secret.json"));
        assert!(!set.is_excluded("config/public.json"));
    }

    #[test]
    fn excludes_match_at_any_depth_unless_they_contain_a_slash() {
        let set = PatternSet::excludes(&["*.log", "fixtures/big", "/top.txt", "vendor/"]);
        let table = [
            ("a.log", true),
            ("x/y/a.log", true),
            ("fixtures/big/one.bin", true),
            ("sub/fixtures/big/one.bin", false),
            ("top.txt", true),
            ("sub/top.txt", false),
            ("vendor/lib.js", true),
            ("deep/vendor/lib.js", true),
            ("src/lib.rs", false),
        ];
        for (path, want) in table {
            assert_eq!(set.is_excluded(path), want, "{path}");
        }
        assert!(!set.has_negations());
        assert!(PatternSet::excludes::<&str>(&[]).is_empty());
    }

    #[test]
    fn degenerate_patterns_are_ignored() {
        let set = PatternSet::dockerignore("/\n./\n!\n   \n");
        assert!(set.is_empty(), "{set:?}");
        assert!(!set.is_excluded("anything"));
    }
}
