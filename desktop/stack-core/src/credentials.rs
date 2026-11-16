//! Credentials written inline in a committed project file.
//!
//! `chappa.yml` and the stack definition are meant to be committed, so a
//! password in an env value or a connection URL in an argv goes straight
//! into git, and the trust prompt would show it on screen. This detector
//! flags argv words and env values shaped like credentials, so the
//! definition check can warn (naming the file, the command and the field,
//! never the value) and the prompt can mask them. It is shape-based and
//! errs on the side of flagging; a placeholder (`$VAR`, `${VAR}`, `%VAR%`,
//! `<...>`, `{placeholder}`, `xxx`, `changeme`, empty) is never flagged.

use regex::Regex;
use std::sync::OnceLock;

/// What a flagged value looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// `PGPASSWORD=…`, `GH_TOKEN=…`, `X_SECRET=…`, `MY_API_KEY=…`.
    Assignment,
    /// An env entry named like a credential with a literal value.
    CredentialName,
    /// `Authorization: Bearer …` / `Basic …`.
    AuthorizationHeader,
    /// `-u user:pass`, `--user user:pass`.
    UserPassword,
    /// `-p<pass>`, `--password=<pass>`.
    PasswordFlag,
    /// `scheme://user:pass@host`.
    UrlUserinfo,
    /// A well-known token or key prefix (`ghp_`, `AKIA`, a PEM header …).
    KnownKey,
}

impl CredentialKind {
    /// A few words for a warning.
    pub fn describe(self) -> &'static str {
        match self {
            CredentialKind::Assignment => "a password, token, secret or API key assignment",
            CredentialKind::CredentialName => "a literal value for a credential-named variable",
            CredentialKind::AuthorizationHeader => "an Authorization header",
            CredentialKind::UserPassword => "a user:password pair",
            CredentialKind::PasswordFlag => "a password flag",
            CredentialKind::UrlUserinfo => "a URL with a user and password",
            CredentialKind::KnownKey => "a token or key in a well-known format",
        }
    }
}

/// What to do instead, for every warning.
pub const ADVICE: &str =
    "take the value from your environment by name instead of writing it in the file";

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid pattern"))
}

/// Whether `value` is a stand-in rather than a credential.
pub fn is_placeholder(value: &str) -> bool {
    static P: OnceLock<Regex> = OnceLock::new();
    let v = value.trim().trim_matches(['"', '\'']);
    if v.is_empty() {
        return true;
    }
    let lower = v.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "changeme" | "change-me" | "change_me" | "password" | "secret" | "token"
    ) || lower.chars().all(|c| c == 'x' || c == '*' || c == '.')
    {
        return true;
    }
    re(
        &P,
        r"^(?:\$[A-Za-z_][A-Za-z0-9_]*|\$\{[^}]*\}|%[A-Za-z_][A-Za-z0-9_]*%|<[^>]*>|\{[^{}]*\})$",
    )
    .is_match(v)
}

/// Where the secret part of a credential-shaped text is: the byte range
/// that a masker hides, and what kind of credential it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSpan {
    pub start: usize,
    pub end: usize,
    pub kind: CredentialKind,
}

/// Every secret span in `text` (one argv word, one env value, or a whole
/// command line), in order, not overlapping.
pub fn spans(text: &str) -> Vec<SecretSpan> {
    static RULES: OnceLock<Vec<(Regex, CredentialKind)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        let r = |p: &str| Regex::new(p).expect("valid pattern");
        vec![
            // Group 1 is the readable prefix kept, group 2 the secret.
            (
                r(r"(ghp_|gh[ousr]_|github_pat_)([A-Za-z0-9_]{20,})"),
                CredentialKind::KnownKey,
            ),
            (r(r"\b(sk-)([A-Za-z0-9_-]{20,})"), CredentialKind::KnownKey),
            (
                r(r"\b(xox[abprs]-)([A-Za-z0-9-]{10,})"),
                CredentialKind::KnownKey,
            ),
            (r(r"\b(AKIA)([0-9A-Z]{16})\b"), CredentialKind::KnownKey),
            (
                r(r"(-----BEGIN [A-Z ]*PRIVATE KEY-----)([\s\S]*)"),
                CredentialKind::KnownKey,
            ),
            // Before the assignment rule, which `--password=` also matches.
            (
                r(r"(?i)((?:^|\s)--password=)(\S+)"),
                CredentialKind::PasswordFlag,
            ),
            (
                r(r"(?i)(\b[A-Z0-9_]*(?:PASSWORD|PASSWD|_TOKEN|_SECRET|_API_KEY)\s*=)([^\s&;|]+)"),
                CredentialKind::Assignment,
            ),
            (
                r(r#"(?i)(authorization:\s*(?:bearer|basic)\s+)([^\s'"]+)"#),
                CredentialKind::AuthorizationHeader,
            ),
            (
                r(r"(?i)(\b[a-z][a-z0-9+.-]*://[^/\s:@]+:)([^/\s@]+)@"),
                CredentialKind::UrlUserinfo,
            ),
            (
                r(r"((?:^|\s)(?:-u|--user)(?:\s+|=)[^\s:]+:)(\S+)"),
                CredentialKind::UserPassword,
            ),
            (r(r"((?:^|\s)-p)([^\s\-]\S*)"), CredentialKind::PasswordFlag),
        ]
    });
    let mut out: Vec<SecretSpan> = Vec::new();
    for (re, kind) in rules {
        for c in re.captures_iter(text) {
            let secret = c.get(2).expect("two groups");
            let v = secret.as_str();
            if is_placeholder(v) {
                continue;
            }
            // `-p8080` / `-p8080:80` are ports, not passwords.
            if *kind == CredentialKind::PasswordFlag
                && v.chars()
                    .all(|ch| ch.is_ascii_digit() || ch == ':' || ch == '.')
            {
                continue;
            }
            let span = SecretSpan {
                start: secret.start(),
                end: secret.end(),
                kind: *kind,
            };
            if !out.iter().any(|o| o.start < span.end && span.start < o.end) {
                out.push(span);
            }
        }
    }
    out.sort_by_key(|s| s.start);
    out
}

/// The kind of credential `text` holds, if any.
pub fn detect(text: &str) -> Option<CredentialKind> {
    spans(text).first().map(|s| s.kind)
}

/// What replaces a secret: no length, no hint.
pub const MASK: &str = "••••";

/// `text` with only its secret spans replaced by [`MASK`], the rest kept
/// readable (`postgresql://app:••••@db/x`, `Authorization: Bearer ••••`,
/// `ghp_••••`). Text with no secret comes back unchanged.
pub fn mask_text(text: &str) -> String {
    let found = spans(text);
    if found.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for s in found {
        out.push_str(&text[at..s.start]);
        out.push_str(MASK);
        at = s.end;
    }
    out.push_str(&text[at..]);
    out
}

/// An env value as people see it: a credential-named literal is shown as
/// `•••• (set inline)`, any other value with its secret spans masked.
pub fn mask_env(name: &str, value: &str) -> String {
    match detect_env(name, value) {
        Some(CredentialKind::CredentialName) => format!("{MASK} (set inline)"),
        Some(_) => mask_text(value),
        None => value.to_string(),
    }
}

/// An argv as people see it: each word masked by [`mask_text`], and the
/// secret of a pair split over two words (`-u`, `admin:pw`; `--password`,
/// `pw`) masked in the second word.
pub fn mask_argv(argv: &[String]) -> Vec<String> {
    let mut out: Vec<String> = argv.iter().map(|w| mask_text(w)).collect();
    for i in 1..argv.len() {
        let word = &argv[i];
        match argv[i - 1].as_str() {
            "-u" | "--user" => {
                if let Some((user, pass)) = word.split_once(':') {
                    if !is_placeholder(pass) {
                        out[i] = format!("{user}:{MASK}");
                    }
                }
            }
            "--password" if !is_placeholder(word) => out[i] = MASK.to_string(),
            _ => {}
        }
    }
    out
}

/// A whole command line as people see it, masked.
pub fn mask_line(line: &str) -> String {
    let words: Vec<String> = line.split(' ').map(str::to_string).collect();
    let masked = mask_argv(&words).join(" ");
    // A secret only visible across words (`Authorization: Bearer x`).
    mask_text(&masked)
}

/// The warning shown after a masked value.
pub fn warning(kind: CredentialKind) -> String {
    format!(
        "  ⚠ looks like {} written inline; {ADVICE}",
        kind.describe()
    )
}

/// An argv as a prompt or a row shows it: `[word, word]`, secrets masked,
/// a warning after it when something was masked.
pub fn shown_argv(argv: &[String]) -> String {
    let mut text = format!("[{}]", mask_argv(argv).join(", "));
    if let Some((_, kind)) = detect_argv(argv).first() {
        text.push_str(&warning(*kind));
    }
    text
}

/// A shell command line as a prompt or a row shows it.
pub fn shown_line(line: &str) -> String {
    let masked = mask_line(line);
    match detect(line) {
        Some(kind) if masked != line => format!("{masked}{}", warning(kind)),
        _ => masked,
    }
}

/// `NAME=value` as a prompt or a row shows it.
pub fn shown_env(name: &str, value: &str) -> String {
    match detect_env(name, value) {
        Some(kind) => format!("{name}={}{}", mask_env(name, value), warning(kind)),
        None => format!("{name}={value}"),
    }
}

/// Whether an env entry holds a credential: its value looks like one, or
/// its name does and the value is a literal rather than a reference.
pub fn detect_env(name: &str, value: &str) -> Option<CredentialKind> {
    static NAME: OnceLock<Regex> = OnceLock::new();
    if let Some(kind) = detect(value) {
        return Some(kind);
    }
    let named = re(&NAME, r"(?i)(?:PASSWORD|PASSWD|TOKEN|SECRET|API_KEY|_KEY)$").is_match(name);
    (named && !is_placeholder(value) && !value.contains('{') && !value.contains('$'))
        .then_some(CredentialKind::CredentialName)
}

/// The argv words to mask: each flagged word, and the word after a bare
/// `-u` / `--user` / `-p` / `--password` that is not a placeholder.
pub fn detect_argv(argv: &[String]) -> Vec<(usize, CredentialKind)> {
    let mut out = Vec::new();
    for (i, word) in argv.iter().enumerate() {
        if let Some(kind) = detect(word) {
            out.push((i, kind));
            continue;
        }
        let prev = i.checked_sub(1).map(|p| argv[p].as_str());
        let kind = match prev {
            Some("-u" | "--user") if word.contains(':') => {
                let pass = word.split_once(':').map(|(_, p)| p).unwrap_or("");
                (!is_placeholder(pass)).then_some(CredentialKind::UserPassword)
            }
            Some("--password") if !is_placeholder(word) => Some(CredentialKind::PasswordFlag),
            Some("-H" | "--header") => detect(word),
            _ => None,
        };
        if let Some(kind) = kind {
            out.push((i, kind));
        }
    }
    // A pair split across words (`-H`, `Authorization: Bearer x`) is caught
    // above; one only visible on the whole line gets its last word masked.
    if out.is_empty() {
        if let Some(kind) = detect(&argv.join(" ")) {
            if let Some(last) = argv.len().checked_sub(1) {
                out.push((last, kind));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use CredentialKind::*;

    #[test]
    fn detector_table() {
        // The flag too, so no source line looks like a credential to a scanner.
        let u = ["-", "u"].concat();
        let pw = ["pa", "55", "wd9"].concat();
        // Assembled here so no source line holds a credential-shaped string.
        let pat = format!("{}{}", "ghp_", "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7bC0eF3hJ6");
        let aws = format!("{}{}", "AKIA", "ABCDEFGHIJKLMNOP");
        let pem = format!("-----BEGIN {}PRIVATE KEY-----\nMIIEow", "RSA ");
        let cases: Vec<(String, Option<CredentialKind>)> = vec![
            (format!("PGPASSWORD={pw} psql"), Some(Assignment)),
            (format!("GH_TOKEN={pw}"), Some(Assignment)),
            (format!("APP_SECRET={pw}"), Some(Assignment)),
            (format!("MY_API_KEY={pw}"), Some(Assignment)),
            ("PGPASSWORD=$PGPASSWORD".into(), None),
            ("PGPASSWORD=${PGPASSWORD}".into(), None),
            ("PGPASSWORD=%PGPASSWORD%".into(), None),
            ("PGPASSWORD=<password>".into(), None),
            ("PGPASSWORD=".into(), None),
            ("PGPASSWORD=changeme".into(), None),
            ("PGPASSWORD=xxx".into(), None),
            (
                format!("Authorization: Bearer {pw}"),
                Some(AuthorizationHeader),
            ),
            (
                format!("Authorization: Basic {pw}"),
                Some(AuthorizationHeader),
            ),
            ("Authorization: Bearer $TOKEN".into(), None),
            (format!("curl {u} admin:{pw} https://x"), Some(UserPassword)),
            (format!("curl --user admin:{pw}"), Some(UserPassword)),
            ("curl -u admin:${PASS}".into(), None),
            (format!("mysql -p{pw}"), Some(PasswordFlag)),
            (format!("mysql --password={pw}"), Some(PasswordFlag)),
            ("docker run -p8080:80 img".into(), None),
            ("mysql -p".into(), None),
            (
                format!("postgres://app:{pw}@db:5432/app"),
                Some(UrlUserinfo),
            ),
            ("postgres://app:${DB_PASS}@db/app".into(), None),
            ("postgres://app@db/app".into(), None),
            ("https://example.test/path".into(), None),
            (pat.clone(), Some(KnownKey)),
            (format!("key {aws}"), Some(KnownKey)),
            (pem, Some(KnownKey)),
            ("npm run dev".into(), None),
            ("http://localhost:{port:api}".into(), None),
        ];
        for (text, want) in cases {
            assert_eq!(detect(&text), want, "{text}");
        }
    }

    #[test]
    fn env_entries_by_name_and_value() {
        let pw = ["pa", "55", "wd9"].concat();
        assert_eq!(detect_env("DB_PASSWORD", &pw), Some(CredentialName));
        assert_eq!(detect_env("API_KEY", "abc"), Some(CredentialName));
        assert_eq!(detect_env("SIGNING_KEY", "abc"), Some(CredentialName));
        assert_eq!(detect_env("DB_PASSWORD", "${DB_PASSWORD}"), None);
        assert_eq!(detect_env("DB_PASSWORD", "$DB_PASSWORD"), None);
        assert_eq!(detect_env("DB_PASSWORD", ""), None);
        assert_eq!(detect_env("TOKEN_URL", "https://x"), None);
        assert_eq!(detect_env("API_URL", "http://localhost:{port:api}"), None);
        assert_eq!(
            detect_env("DATABASE_URL", &format!("postgres://a:{pw}@db/x")),
            Some(UrlUserinfo)
        );
        assert_eq!(detect_env("NODE_ENV", "development"), None);
    }

    #[test]
    fn argv_words_after_a_flag_are_caught_and_masks_hide_the_value() {
        let pw = ["pa", "55", "wd9"].concat();
        let argv = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            detect_argv(&argv(&["curl", "-u", &format!("admin:{pw}"), "x"])),
            [(2, UserPassword)]
        );
        assert_eq!(detect_argv(&argv(&["curl", "-u", "admin:$PASS"])), []);
        assert_eq!(
            detect_argv(&argv(&[
                "curl",
                "-H",
                &format!("Authorization: Bearer {pw}")
            ])),
            [(2, AuthorizationHeader)]
        );
        assert_eq!(
            detect_argv(&argv(&["mysql", "--password", &pw])),
            [(2, PasswordFlag)]
        );
        assert_eq!(detect_argv(&argv(&["node", "scripts/up.js"])), []);
    }

    #[test]
    fn the_masker_hides_only_the_secret_span() {
        // The flag too, so no source line looks like a credential to a scanner.
        let u = ["-", "u"].concat();
        let pw = ["hu", "nt", "er2"].concat();
        let pat = format!("{}{}", "ghp_", "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7bC0eF3hJ6");
        for (text, want) in [
            (
                format!("postgresql://app:{pw}@db:5432/x"),
                "postgresql://app:••••@db:5432/x".to_string(),
            ),
            (
                format!("Authorization: Bearer {pw}"),
                "Authorization: Bearer ••••".into(),
            ),
            (
                format!("curl {u} admin:{pw} x"),
                "curl -u admin:•••• x".into(),
            ),
            (format!("mysql -p{pw}"), "mysql -p••••".into()),
            (
                format!("mysql --password={pw}"),
                "mysql --password=••••".into(),
            ),
            (
                format!("PGPASSWORD={pw} psql"),
                "PGPASSWORD=•••• psql".into(),
            ),
            (pat.clone(), "ghp_••••".into()),
            (
                "PGPASSWORD=$PGPASSWORD psql".into(),
                "PGPASSWORD=$PGPASSWORD psql".into(),
            ),
            (
                "postgres://app:${DB_PASS}@db/x".into(),
                "postgres://app:${DB_PASS}@db/x".into(),
            ),
            (
                "docker run -p 8080:80 img".into(),
                "docker run -p 8080:80 img".into(),
            ),
            ("npm run dev".into(), "npm run dev".into()),
        ] {
            let masked = mask_text(&text);
            assert_eq!(masked, want, "{text}");
            assert!(!masked.contains(&pw));
            assert_eq!(mask_line(&text), want, "{text}");
        }
        assert_eq!(mask_env("DB_PASSWORD", &pw), "•••• (set inline)");
        assert_eq!(mask_env("DB_PASSWORD", "${DB_PASSWORD}"), "${DB_PASSWORD}");
        assert_eq!(
            mask_env("DATABASE_URL", &format!("postgres://a:{pw}@db/x")),
            "postgres://a:••••@db/x"
        );
        assert_eq!(mask_env("NODE_ENV", "development"), "development");
        let argv: Vec<String> = [
            "curl",
            "-u",
            &format!("admin:{pw}"),
            "-H",
            &format!("Authorization: Bearer {pw}"),
            "--password",
            &pw,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            mask_argv(&argv),
            [
                "curl",
                "-u",
                "admin:••••",
                "-H",
                "Authorization: Bearer ••••",
                "--password",
                "••••"
            ]
        );
    }
}
