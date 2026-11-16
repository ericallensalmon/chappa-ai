use super::*;
use crate::definition::DataSafetyDef;

fn rules(yaml: &str) -> DataRules {
    match yaml.parse::<DataRules>() {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    }
}

fn load_err(yaml: &str) -> RulesError {
    match DataRules::parse(yaml, "stack/data-safety.yml") {
        Ok(_) => panic!("expected an error for:\n{yaml}"),
        Err(e) => e,
    }
}

fn row(table: &str, column: &str, values: &[&str]) -> SampleRow {
    SampleRow {
        table: table.into(),
        column: column.into(),
        values: values.iter().map(|v| v.to_string()).collect(),
    }
}

fn data(table: &str, column: &str) -> Location {
    Location::Data {
        table: table.into(),
        column: column.into(),
    }
}

/// (rule, table.column, count) of every finding, for compact asserts.
fn summary(findings: &[Finding]) -> Vec<(String, String, u32)> {
    findings
        .iter()
        .map(|f| (f.rule.clone(), f.location.to_string(), f.count))
        .collect()
}

fn s(rule: &str, place: &str, count: u32) -> (String, String, u32) {
    (rule.into(), place.into(), count)
}

const STARTER: &str = include_str!("../../../../stack/data-safety.starter.yml");

/// The starter file with every rule switched on: the leading `#` removed
/// from each commented line after `rules:` (rule lines are written `#  `).
fn starter_enabled() -> String {
    let mut out = String::new();
    let mut after_rules = false;
    for line in STARTER.lines() {
        if after_rules && line.starts_with("#  ") {
            out.push_str(&line[1..]);
        } else {
            out.push_str(line);
        }
        out.push('\n');
        if line == "rules:" {
            after_rules = true;
        }
    }
    out
}

// ------------------------------------------------------------ rules file

#[test]
fn a_full_rule_parses_and_defaults_apply() {
    let r = rules(
        r#"
rules:
  - id: card-number
    pattern: '\b(?:\d[ -]?){13,19}\b'
    allow: ['^4000']
    threshold: 3
    validate: luhn
    columns: ['payments\..*']
    severity: warn
  - id: iban
    validate: iban
"#,
    );
    assert_eq!(r.rules.len(), 2);
    let card = &r.rules[0];
    assert_eq!(card.id, "card-number");
    assert_eq!(card.threshold, 3);
    assert_eq!(card.validate, Some(Validator::Luhn));
    assert_eq!(card.severity, Severity::Warn);
    assert!(card.in_scope("payments", "card"));
    assert!(!card.in_scope("customers", "card"));
    let iban = r.rule("iban").unwrap();
    assert_eq!(iban.threshold, 1);
    assert_eq!(iban.severity, Severity::Bad);
    assert!(iban.in_scope("anything", "at_all"));
    assert_eq!(r.file, PathBuf::from("<inline>"));
    assert_eq!(r.hash.len(), 64);
}

#[test]
fn empty_and_all_commented_files_load_with_no_rules() {
    for text in [
        "rules:\n",
        "rules: []\n",
        "rules:\n# - id: x\n#   pattern: y\n",
    ] {
        assert!(rules(text).rules.is_empty(), "{text:?}");
    }
}

#[test]
fn load_errors_name_the_file_the_rule_and_the_problem() {
    use RulesErrorKind as K;
    // (yaml, rule id or index named, kind check, text the message must hold)
    #[allow(clippy::type_complexity)]
    let cases: &[(&str, Option<&str>, Option<usize>, fn(&K) -> bool, &str)] = &[
        (
            "rules:\n  - id: phone\n    pattern: 'x'\n    treshold: 5\n",
            Some("phone"),
            Some(0),
            |k| matches!(k, K::UnknownKey(key) if key == "treshold"),
            "unknown key `treshold`",
        ),
        (
            "rules:\n  - id: a\n    pattern: x\n  - id: b\n    pattern: y\n    alow: [z]\n",
            Some("b"),
            Some(1),
            |k| matches!(k, K::UnknownKey(key) if key == "alow"),
            "`alow`",
        ),
        (
            "rule:\n  - id: a\n",
            None,
            None,
            |k| matches!(k, K::UnknownTopLevelKey(key) if key == "rule"),
            "unknown key `rule`",
        ),
        (
            "{}\n",
            None,
            None,
            |k| matches!(k, K::Shape(_)),
            "missing `rules:`",
        ),
        (
            "rules: x\n",
            None,
            None,
            |k| matches!(k, K::Shape(_)),
            "must be a list",
        ),
        (
            "- id: a\n",
            None,
            None,
            |k| matches!(k, K::Shape(_)),
            "mapping",
        ),
        (
            "rules: [\n",
            None,
            None,
            |k| matches!(k, K::Yaml(_)),
            "not valid YAML",
        ),
        (
            "rules:\n  - pattern: x\n",
            None,
            Some(0),
            |k| matches!(k, K::MissingId),
            "rules[0]: rule has no `id`",
        ),
        (
            "rules:\n  - id: 'has space'\n    pattern: x\n",
            None,
            Some(0),
            |k| matches!(k, K::InvalidId),
            "`id` must be",
        ),
        (
            "rules:\n  - id: a\n    pattern: x\n  - id: a\n    pattern: y\n",
            Some("a"),
            Some(1),
            |k| matches!(k, K::DuplicateId),
            "duplicate rule id",
        ),
        (
            "rules:\n  - id: a\n    threshold: 2\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::NeedsPatternOrValidate),
            "needs `pattern`, `validate`",
        ),
        (
            "rules:\n  - id: a\n    pattern: [x]\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::WrongType { key, .. } if key == "pattern"),
            "`pattern` must be a string",
        ),
        (
            "rules:\n  - id: a\n    pattern: x\n    allow: '^y'\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::WrongType { key, .. } if key == "allow"),
            "a list of strings",
        ),
        (
            "rules:\n  - id: a\n    pattern: x\n    columns: [1]\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::WrongType { key, .. } if key == "columns"),
            "a list of strings",
        ),
        (
            "rules:\n  - id: a\n    validate: luhm\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::UnknownValidator(v) if v == "luhm"),
            "unknown validator `luhm`",
        ),
        (
            "rules:\n  - id: a\n    pattern: x\n    severity: high\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::UnknownSeverity(v) if v == "high"),
            "unknown severity `high`",
        ),
        (
            "rules:\n  - id: a\n    pattern: x\n    threshold: 0\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::ZeroThreshold),
            "at least 1",
        ),
        (
            "rules:\n  - id: a\n    pattern: x\n    threshold: -2\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::WrongType { key, .. } if key == "threshold"),
            "positive whole number",
        ),
        (
            "rules:\n  - id: a\n    pattern: x\n    threshold: '5'\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::WrongType { key, .. } if key == "threshold"),
            "positive whole number",
        ),
        (
            "rules:\n  - id: a\n    pattern: '(unclosed'\n",
            Some("a"),
            Some(0),
            |k| matches!(k, K::BadRegex { key, .. } if key == "pattern"),
            "`pattern` is not a valid pattern",
        ),
    ];
    for (yaml, rule, index, kind, text) in cases {
        let e = load_err(yaml);
        assert_eq!(e.rule.as_deref(), *rule, "{yaml}");
        assert_eq!(e.index, *index, "{yaml}");
        assert!(kind(&e.kind), "{yaml} -> {:?}", e.kind);
        let shown = e.to_string();
        assert!(shown.starts_with("stack/data-safety.yml"), "{shown}");
        assert!(shown.contains(text), "{shown}");
        if let Some(id) = rule {
            assert!(shown.contains(&format!("rule `{id}`")), "{shown}");
        }
    }
}

#[test]
fn list_and_rule_counts_are_capped() {
    let many: String = (0..=MAX_RULES)
        .map(|i| format!("  - id: r{i}\n    pattern: x\n"))
        .collect();
    let e = load_err(&format!("rules:\n{many}"));
    assert!(matches!(e.kind, RulesErrorKind::TooManyRules { .. }), "{e}");

    let allow: Vec<String> = (0..=MAX_LIST_ENTRIES).map(|i| format!("'a{i}'")).collect();
    let e = load_err(&format!(
        "rules:\n  - id: a\n    pattern: x\n    allow: [{}]\n",
        allow.join(", ")
    ));
    assert!(
        matches!(e.kind, RulesErrorKind::TooManyEntries { key: "allow", .. }),
        "{e}"
    );
}

#[test]
fn look_around_and_backreferences_are_refused_with_the_way_out() {
    let patterns = [
        r"^(?!TEST-).+",
        r"foo(?=bar)",
        r"(?<=\$)\d+",
        r"(?<!x)y",
        r"(a)\1",
        r"(?P<word>a)\k<word>",
    ];
    for p in patterns {
        for (key, yaml) in [
            (
                "pattern",
                format!("rules:\n  - id: r\n    pattern: '{p}'\n"),
            ),
            (
                "allow[0]",
                format!("rules:\n  - id: r\n    pattern: x\n    allow: ['{p}']\n"),
            ),
            (
                "columns[0]",
                format!("rules:\n  - id: r\n    pattern: x\n    columns: ['{p}']\n"),
            ),
        ] {
            let e = load_err(&yaml);
            assert!(
                matches!(&e.kind, RulesErrorKind::UnsupportedSyntax { key: k } if k == key),
                "{p} in {key}: {e}"
            );
            let shown = e.to_string();
            assert!(shown.contains("rule `r`"), "{shown}");
            assert!(shown.contains("use `allow` to exclude"), "{shown}");
        }
    }
    // the syntax that only looks similar is fine
    rules("rules:\n  - id: r\n    pattern: '(?<name>a)(?:b)(?i)c\\d'\n");
}

#[test]
fn an_oversized_regex_is_refused() {
    let e = load_err("rules:\n  - id: big\n    pattern: '\\w{1000}\\w{1000}\\w{1000}'\n");
    assert!(
        matches!(&e.kind, RulesErrorKind::RegexTooLarge { key } if key == "pattern"),
        "{e}"
    );
    assert!(e.to_string().contains("rule `big`"), "{e}");
}

#[test]
fn rules_hash_follows_the_file_text() {
    let a = rules("rules:\n  - id: a\n    pattern: x\n");
    let b = rules("rules:\n  - id: a\n    pattern: x\n");
    let c = rules("rules:\n  - id: a\n    pattern: y\n");
    assert_eq!(a.hash, b.hash);
    assert_ne!(a.hash, c.hash);
    assert_eq!(
        a.hash,
        blake3::hash(b"rules:\n  - id: a\n    pattern: x\n")
            .to_hex()
            .to_string()
    );
}

#[test]
fn load_reads_the_file_and_names_it_on_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data-safety.yml");
    std::fs::write(&path, "rules:\n  - id: a\n    pattern: x\n").unwrap();
    let r = DataRules::load(&path).unwrap();
    assert_eq!(r.file, path);
    assert_eq!(
        r.hash,
        rules("rules:\n  - id: a\n    pattern: x\n").hash,
        "same bytes, same hash"
    );

    std::fs::write(&path, "rules:\n  - id: a\n    patern: x\n").unwrap();
    let e = DataRules::load(&path).unwrap_err();
    assert!(e.to_string().contains("data-safety.yml"), "{e}");
    assert!(e.to_string().contains("`patern`"), "{e}");

    let missing = dir.path().join("nope.yml");
    let e = DataRules::load(&missing).unwrap_err();
    assert!(matches!(e.kind, RulesErrorKind::Io(_)), "{e}");
    assert!(e.to_string().contains("nope.yml"), "{e}");

    std::fs::write(&path, vec![b'#'; RULES_FILE_CAP + 1]).unwrap();
    let e = DataRules::load(&path).unwrap_err();
    assert!(matches!(e.kind, RulesErrorKind::TooLarge { .. }), "{e}");
}

// ------------------------------------------------------------------ scan

#[test]
fn thresholds_apply_per_rule_table_and_column() {
    let r = rules(
        r#"
rules:
  - id: digits
    pattern: '\d{4}'
    threshold: 3
  - id: word
    pattern: 'secret'
"#,
    );
    let out = r
        .scan(&[
            row("customers", "a", &["1234", "x5678", "9012y", "none"]),
            row("customers", "b", &["1234", "5678"]),
            // a second row for the same column adds to the same count
            row("orders", "ref", &["1111", "2222"]),
            row("orders", "ref", &["3333"]),
            row("orders", "note", &["a secret", "none"]),
        ])
        .unwrap();
    assert_eq!(
        summary(&out.findings),
        vec![
            s("digits", "customers.a", 3),
            s("digits", "orders.ref", 3),
            s("word", "orders.note", 1),
        ]
    );
    assert_eq!(out.scanned_values, 11);
    assert_eq!(out.truncated_values, 0);
    // a threshold not reached is no finding at all, not a zero
    assert!(!out
        .findings
        .iter()
        .any(|f| f.location == data("customers", "b")));
}

#[test]
fn a_value_counts_once_however_many_matches_it_holds() {
    let r = rules("rules:\n  - id: d\n    pattern: '\\d'\n");
    let out = r.scan(&[row("t", "c", &["1 2 3 4", "5"])]).unwrap();
    assert_eq!(summary(&out.findings), vec![s("d", "t.c", 2)]);
}

#[test]
fn allow_lists_match_the_matched_text_not_the_whole_value() {
    let r = rules(
        r#"
rules:
  - id: email
    pattern: '[^@\s]+@[^@\s]+\.[a-z]{2,}'
    allow: ['@example\.(com|org|net)$', '@test\.invalid$']
  - id: ids
    pattern: '.+'
    allow: ['^TEST-']
"#,
    );
    let cases: &[(&str, &str, bool)] = &[
        // (rule, value, counted)
        ("email", "alex@example.com", false),
        ("email", "sam@test.invalid", false),
        ("email", concat!("alex", "@mail.co"), true),
        // the allowed address does not excuse the real one beside it
        ("email", concat!("alex@example.com, sam", "@mail.co"), true),
        // `$` anchors to the end of the match, so this one is not allowed
        ("email", concat!("alex", "@example.com.mail.co"), true),
        ("email", "no address here", false),
        ("ids", "TEST-0001", false),
        ("ids", "CUST-0001", true),
        // `^` anchors to the start of the match, which is the whole value here
        ("ids", "X-TEST-1", true),
    ];
    for (rule, value, counted) in cases {
        assert_eq!(
            r.rule(rule).unwrap().matches(value),
            *counted,
            "{rule} on {value:?}"
        );
    }
}

#[test]
fn column_scopes_match_the_whole_table_column_name() {
    let r = rules(
        r#"
rules:
  - id: ext
    columns: ['customers\..*_external_id', 'orders\.ref']
    pattern: '.+'
"#,
    );
    let rule = r.rule("ext").unwrap();
    let cases: &[(&str, &str, bool)] = &[
        ("customers", "crm_external_id", true),
        ("customers", "_external_id", true),
        ("customers", "crm_external_id_old", false),
        ("old_customers", "crm_external_id", false),
        ("orders", "ref", true),
        ("orders", "ref2", false),
        ("xorders", "ref", false),
    ];
    for (t, c, want) in cases {
        assert_eq!(rule.in_scope(t, c), *want, "{t}.{c}");
    }
    let out = r
        .scan(&[
            row("customers", "crm_external_id", &["CUST-1"]),
            row("customers", "email", &["CUST-2"]),
        ])
        .unwrap();
    assert_eq!(
        summary(&out.findings),
        vec![s("ext", "customers.crm_external_id", 1)]
    );
}

#[test]
fn validators_filter_pattern_matches_and_stand_alone() {
    let r = rules(
        r#"
rules:
  - id: card
    pattern: '\b(?:\d[ -]?){13,19}\b'
    validate: luhn
  - id: iban
    validate: iban
  - id: mail
    pattern: '[^@\s<>,;]+@[^@\s<>,;]+'
    validate: email-mx-shape
"#,
    );
    let cases: &[(&str, &str, bool)] = &[
        ("card", "4111 1111 1111 1111", true),
        ("card", "paid with 5555-5555-5555-4444 today", true),
        ("card", "4111 1111 1111 1112", false),
        ("card", "order 1234567890123", false),
        // without a pattern the whole value is checked, trimmed
        ("iban", " GB82 WEST 1234 5698 7654 32 ", true),
        ("iban", "GB82 WEST 1234 5698 7654 33", false),
        ("iban", "iban GB82 WEST 1234 5698 7654 32", false),
        ("iban", "", false),
        ("mail", concat!("Alex <alex", "@mail.co>"), true),
        ("mail", "Alex <alex@example.com>", false),
        ("mail", "alex@shop.test; sam@host.local", false),
    ];
    for (rule, value, want) in cases {
        assert_eq!(
            r.rule(rule).unwrap().matches(value),
            *want,
            "{rule} on {value:?}"
        );
    }
}

#[test]
fn empty_matches_never_count() {
    let r = rules("rules:\n  - id: any\n    pattern: 'x*'\n  - id: all\n    pattern: '.*'\n");
    let out = r.scan(&[row("t", "c", &["", "abc"])]).unwrap();
    // `x*` only ever matches empty; `.*` matches "abc" but not ""
    assert_eq!(summary(&out.findings), vec![s("all", "t.c", 1)]);
}

#[test]
fn over_cap_samples_are_refused_whole() {
    let r = rules("rules:\n  - id: v\n    pattern: 'v'\n");
    let at_cap: Vec<&str> = vec!["v"; SAMPLE_VALUE_CAP];
    let ok = r.scan(&[row("t", "c", &at_cap)]).unwrap();
    assert_eq!(ok.scanned_values, SAMPLE_VALUE_CAP);

    let half: Vec<&str> = vec!["v"; SAMPLE_VALUE_CAP / 2 + 1];
    let e = r
        .scan(&[row("t", "a", &half), row("t", "b", &half)])
        .unwrap_err();
    assert!(
        matches!(e, SafetyError::TooManyValues { values } if values == SAMPLE_VALUE_CAP + 2),
        "{e}"
    );

    let big = "v".repeat(SAMPLE_BYTE_CAP);
    let e = r.scan(&[row("t", "c", &[&big])]).unwrap_err();
    assert!(matches!(e, SafetyError::TooManyBytes { .. }), "{e}");
    assert!(!e.to_string().contains("vvvv"), "{e}");
}

#[test]
fn long_values_are_truncated_for_matching_and_counted() {
    let r = rules("rules:\n  - id: m\n    pattern: 'MARK'\n");
    let early = format!("MARK{}", "x".repeat(MATCH_VALUE_CAP));
    let late = format!("{}MARK", "x".repeat(MATCH_VALUE_CAP));
    let exact = "x".repeat(MATCH_VALUE_CAP);
    // a multi-byte character straddling the cut must not split
    let straddle = format!("{}é{}", "x".repeat(MATCH_VALUE_CAP - 1), "MARK");
    let out = r
        .scan(&[row("t", "c", &[&early, &late, &exact, &straddle, "MARK"])])
        .unwrap();
    assert_eq!(out.truncated_values, 3);
    assert_eq!(out.scanned_values, 5);
    assert_eq!(summary(&out.findings), vec![s("m", "t.c", 2)]);
    assert_eq!(clip(&straddle).len(), MATCH_VALUE_CAP - 1);
}

#[test]
fn unsafe_table_or_column_names_refuse_the_sample() {
    let r = rules("rules:\n  - id: m\n    pattern: 'x'\n");
    let long = "n".repeat(MAX_NAME_LEN + 1);
    let cases: &[(&str, &str, &str)] = &[
        ("", "c", "table name is empty"),
        ("t", "  ", "column name is empty"),
        ("t\u{1b}[31m", "c", "control characters"),
        ("t", &long, "longer than"),
    ];
    for (t, c, msg) in cases {
        let e = r
            .scan(&[row("ok", "ok", &["x"]), row(t, c, &["x"])])
            .unwrap_err();
        let shown = e.to_string();
        assert!(shown.contains("row 1"), "{shown}");
        assert!(shown.contains(msg), "{shown}");
        assert!(!shown.contains("nnnnnnnn"), "{shown}");
        assert!(!shown.contains("[31m"), "{shown}");
    }
}

// --------------------------------------------------------- hook findings

fn hook_finding(rule: &str, table: &str, column: &str, count: u32, sev: Severity) -> Finding {
    Finding {
        rule: rule.into(),
        location: data(table, column),
        count,
        severity: sev,
    }
}

#[test]
fn hook_findings_pass_through_merged_and_checked() {
    let out = accept_hook_findings(vec![
        hook_finding(
            "email-real-domain",
            "customers",
            "email",
            12,
            Severity::Warn,
        ),
        hook_finding("phone", "customers", "phone", 0, Severity::Bad),
        hook_finding("email-real-domain", "customers", "email", 3, Severity::Bad),
        hook_finding("email-real-domain", "orders", "email", 1, Severity::Warn),
    ])
    .unwrap();
    assert_eq!(
        summary(&out),
        vec![
            s("email-real-domain", "customers.email", 15),
            s("email-real-domain", "orders.email", 1),
        ]
    );
    assert_eq!(out[0].severity, Severity::Bad, "the worse severity is kept");

    let source = Finding {
        rule: "r".into(),
        location: Location::Source {
            file: "a.env".into(),
            line: None,
            commit: None,
        },
        count: 1,
        severity: Severity::Bad,
    };
    let refused: Vec<(Vec<Finding>, &str)> = vec![
        (vec![source], "table and column"),
        (
            vec![hook_finding(
                concat!("alex", "@mail.co"),
                "t",
                "c",
                1,
                Severity::Bad,
            )],
            "rule id",
        ),
        (
            vec![hook_finding(
                "r",
                &"n".repeat(MAX_NAME_LEN + 1),
                "c",
                1,
                Severity::Bad,
            )],
            "table name is longer",
        ),
        (
            vec![hook_finding("r", "t", "c\n", 1, Severity::Bad)],
            "column name contains control",
        ),
        (
            vec![hook_finding("r", "t", "c", 1, Severity::Bad); MAX_HOOK_FINDINGS + 1],
            "over the cap",
        ),
    ];
    for (findings, msg) in refused {
        let e = accept_hook_findings(findings).unwrap_err().to_string();
        assert!(e.contains(msg), "{e}");
        assert!(!e.contains(concat!("alex", "@mail.co")), "{e}");
    }
}

#[test]
fn hook_findings_with_unknown_severity_or_bad_counts_do_not_parse() {
    for stdout in [
        r#"{"findings":[{"rule":"r","table":"t","column":"c","count":1,"severity":"high"}]}"#,
        r#"{"findings":[{"rule":"r","table":"t","column":"c","count":-1,"severity":"bad"}]}"#,
        r#"{"findings":[{"rule":"r","table":"t","column":"c","count":"4111111111111111","severity":"bad"}]}"#,
    ] {
        let v = evaluate(None, HookRun::NotDefined, HookRun::Stdout(stdout)).unwrap();
        assert!(!v.clean);
        assert!(matches!(v.sample, SampleStatus::Refused { .. }), "{v:?}");
        assert!(!format!("{v:?}").contains("4111"), "{v:?}");
    }
}

// ------------------------------------------------------- the invariant

/// Every value in this fixture is something one of the starter rules
/// detects (or deliberately allows). None of them may show up anywhere in
/// what the engine returns.
const SENSITIVE: &[(&str, &str, &[&str])] = &[
    (
        "customers",
        "email",
        &[
            concat!("alex.real", "@mailhost.co.uk"),
            concat!("sam.person", "@shopmail.io"),
            "alex@example.com",
        ],
    ),
    (
        "customers",
        "notes",
        &[concat!(
            "call sam.person",
            "@shopmail.io re card 4111 1111 1111 1111"
        )],
    ),
    (
        "payments",
        "card",
        &["4111111111111111", "5555-5555-5555-4444", "378282246310005"],
    ),
    (
        "payments",
        "iban",
        &["GB82 WEST 1234 5698 7654 32", "DE89370400440532013000"],
    ),
    (
        "customers",
        "ssn",
        &["123-45-6789", "078-05-1120", "000-12-3456"],
    ),
    (
        "customers",
        "phone",
        &[
            "+44 20 7946 0958",
            "+1 202 918 2132",
            "(312) 948-7731",
            "+49 30 9017 6655",
            "+33 1 7039 4412",
            "+1 555 010 4477",
        ],
    ),
    (
        "customers",
        "crm_external_id",
        &["CRM-88341-ZQ", "TEST-0001"],
    ),
];

fn sensitive_rows() -> Vec<SampleRow> {
    SENSITIVE.iter().map(|(t, c, v)| row(t, c, v)).collect()
}

fn assert_no_value_in(text: &str, what: &str) {
    for (_, _, values) in SENSITIVE {
        for v in *values {
            assert!(!text.contains(v), "{what} leaks {v:?}: {text}");
            // and no long-enough piece of one either
            let compact: String = v.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            if compact.len() >= 8 {
                assert!(
                    !text.contains(&compact[..8]),
                    "{what} leaks part of {v:?}: {text}"
                );
            }
        }
    }
}

#[test]
fn no_sampled_value_ever_reaches_a_finding_or_verdict() {
    let r = rules(&starter_enabled());
    let out = r.scan(&sensitive_rows()).unwrap();
    assert!(!out.findings.is_empty());
    // every starter rule found something in the fixture
    for rule in &r.rules {
        assert!(
            out.findings.iter().any(|f| f.rule == rule.id),
            "{} found nothing: {:?}",
            rule.id,
            summary(&out.findings)
        );
    }
    for f in &out.findings {
        assert_no_value_in(&format!("{f:?}"), "Debug");
        assert_no_value_in(&serde_json::to_string(f).unwrap(), "JSON");
        assert_no_value_in(&f.key(), "key");
    }
    assert_no_value_in(&format!("{out:?}"), "outcome Debug");

    let stdout = serde_json::to_string(&sensitive_rows()).unwrap();
    let marker = r#"{"scrubbed": false, "version": "2026-03"}"#;
    let v = evaluate(Some(&r), HookRun::Stdout(marker), HookRun::Stdout(&stdout)).unwrap();
    assert!(!v.clean);
    assert_no_value_in(&format!("{v:?}"), "verdict Debug");
    assert_no_value_in(&serde_json::to_string(&v).unwrap(), "verdict JSON");
    let ack = acknowledge(&v, "Sam", "fixture data", "2026-01-01T00:00:00Z");
    assert_no_value_in(&serde_json::to_string(&ack).unwrap(), "acknowledgement");
    assert_no_value_in(
        &format!("{:?}", enforcement(None, Some(&v), Some(&ack))),
        "enforcement",
    );
}

#[test]
fn refused_samples_do_not_quote_what_they_refused() {
    let r = rules(&starter_enabled());
    let mut rows = sensitive_rows();
    let filler: Vec<&str> = vec!["4111111111111111"; SAMPLE_VALUE_CAP];
    rows.push(row("payments", "bulk", &filler));
    let stdout = serde_json::to_string(&rows).unwrap();
    let broken = stdout.replacen("\"alex.real", "alex.real", 1);
    let wrong_shape = r#"[{"table":"customers","column":"ssn","values":[123456789]}]"#;
    for text in [stdout.as_str(), broken.as_str(), wrong_shape] {
        let v = evaluate(Some(&r), HookRun::NotDefined, HookRun::Stdout(text)).unwrap();
        assert!(!v.clean);
        assert!(matches!(v.sample, SampleStatus::Refused { .. }), "{v:?}");
        assert!(v.findings.is_empty(), "refused means nothing was scanned");
        let shown = format!("{v:?} {}", serde_json::to_string(&v).unwrap());
        assert_no_value_in(&shown, "refusal");
        assert!(!shown.contains("123456789"), "{shown}");
    }
}

// ---------------------------------------------------------------- marker

#[test]
fn marker_cases() {
    let long = format!(r#"{{"scrubbed": true, "version": "{}"}}"#, "v".repeat(500));
    let cases: Vec<(HookRun, MarkerVerdict)> = vec![
        (HookRun::NotDefined, MarkerVerdict::NotDefined),
        (
            HookRun::Failed,
            MarkerVerdict::Unscrubbed {
                reason: UnscrubbedReason::Missing,
                version: None,
                at: None,
            },
        ),
        (
            HookRun::Stdout("  \n"),
            MarkerVerdict::Unscrubbed {
                reason: UnscrubbedReason::Missing,
                version: None,
                at: None,
            },
        ),
        (
            HookRun::Stdout(r#"{"scrubbed": true, "version": "3", "at": "2026-03-01T10:00:00Z"}"#),
            MarkerVerdict::Scrubbed {
                version: Some("3".into()),
                at: Some("2026-03-01T10:00:00Z".into()),
            },
        ),
        (
            HookRun::Stdout(r#"{"scrubbed": false, "version": "3"}"#),
            MarkerVerdict::Unscrubbed {
                reason: UnscrubbedReason::NotScrubbed,
                version: Some("3".into()),
                at: None,
            },
        ),
        (
            HookRun::Stdout("scrub ok"),
            MarkerVerdict::Unscrubbed {
                reason: UnscrubbedReason::Unparsable,
                version: None,
                at: None,
            },
        ),
        (
            HookRun::Stdout(r#"{"scrubbed": "yes"}"#),
            MarkerVerdict::Unscrubbed {
                reason: UnscrubbedReason::Unparsable,
                version: None,
                at: None,
            },
        ),
        (
            HookRun::Stdout(r#"{"findings": []}"#),
            MarkerVerdict::HookFindings,
        ),
        (
            HookRun::Stdout(&long),
            MarkerVerdict::Scrubbed {
                version: Some("v".repeat(64)),
                at: None,
            },
        ),
    ];
    for (run, want) in cases {
        let (got, findings) = evaluate_marker(run);
        assert_eq!(got, want, "{run:?}");
        assert!(findings.unwrap().is_empty());
    }
    let (m, f) = evaluate_marker(HookRun::Stdout(
        r#"{"findings":[{"rule":"r","table":"t","column":"c","count":2,"severity":"warn"}]}"#,
    ));
    assert_eq!(m, MarkerVerdict::HookFindings);
    assert_eq!(summary(&f.unwrap()), vec![s("r", "t.c", 2)]);
}

#[test]
fn verdicts_combine_marker_and_sample() {
    let r = rules("rules:\n  - id: email\n    pattern: '@mail\\.co$'\n");
    let clean_rows = r#"[{"table":"customers","column":"email","values":["alex@example.com"]}]"#;
    let dirty_rows = concat!(
        r#"[{"table":"customers","column":"email","values":["alex"#,
        r#"@mail.co"]}]"#
    );
    let scrubbed = r#"{"scrubbed": true, "version": "3"}"#;
    let hook_clean = r#"{"findings": []}"#;
    let hook_dirty =
        r#"{"findings":[{"rule":"r","table":"t","column":"c","count":4,"severity":"bad"}]}"#;
    use HookRun::{Failed, NotDefined, Stdout};
    // (rules?, marker, sample, clean, sample status key, findings)
    let cases: Vec<(bool, HookRun, HookRun, bool, &str, usize)> = vec![
        (
            true,
            Stdout(scrubbed),
            Stdout(clean_rows),
            true,
            "scanned",
            0,
        ),
        (true, NotDefined, Stdout(clean_rows), true, "scanned", 0),
        (true, Stdout(scrubbed), NotDefined, true, "not-defined", 0),
        // a missing marker is unscrubbed whatever the sample says
        (true, Failed, Stdout(clean_rows), false, "scanned", 0),
        (true, Stdout(""), Stdout(clean_rows), false, "scanned", 0),
        (
            true,
            Stdout(r#"{"scrubbed":false}"#),
            Stdout(clean_rows),
            false,
            "scanned",
            0,
        ),
        (true, Stdout("??"), NotDefined, false, "not-defined", 0),
        // findings beat a good marker
        (
            true,
            Stdout(scrubbed),
            Stdout(dirty_rows),
            false,
            "scanned",
            1,
        ),
        // the sample hook failing or refusing is not clean
        (true, Stdout(scrubbed), Failed, false, "failed", 0),
        (true, Stdout(scrubbed), Stdout("nope"), false, "refused", 0),
        // rows with no rules file: nothing was checked
        (
            false,
            Stdout(scrubbed),
            Stdout(clean_rows),
            false,
            "no-rules",
            0,
        ),
        // hook-computed findings need no rules file
        (
            false,
            NotDefined,
            Stdout(hook_clean),
            true,
            "hook-findings",
            0,
        ),
        (
            false,
            NotDefined,
            Stdout(hook_dirty),
            false,
            "hook-findings",
            1,
        ),
        (
            false,
            Stdout(hook_dirty),
            NotDefined,
            false,
            "not-defined",
            1,
        ),
        (
            true,
            Stdout(hook_dirty),
            Stdout(dirty_rows),
            false,
            "scanned",
            2,
        ),
    ];
    for (with_rules, marker, sample, clean, status, n) in cases {
        let v = evaluate(with_rules.then_some(&r), marker, sample).unwrap();
        let ctx = format!("{marker:?} / {sample:?} -> {v:?}");
        assert_eq!(v.clean, clean, "{ctx}");
        assert_eq!(v.sample.key(), status, "{ctx}");
        assert_eq!(v.findings.len(), n, "{ctx}");
        assert_eq!(v.rules_hash.is_some(), with_rules, "{ctx}");
    }
    assert_eq!(evaluate(Some(&r), NotDefined, NotDefined), None);

    let v = evaluate(Some(&r), Stdout(scrubbed), Stdout(dirty_rows)).unwrap();
    assert_eq!(v.scanned_values, 1);
    assert_eq!(v.marker.version(), Some("3"));
    let json = serde_json::to_value(&v).unwrap();
    assert_eq!(json["marker"]["state"], "scrubbed");
    assert_eq!(json["sample"]["state"], "scanned");
    assert_eq!(json["rulesHash"], r.hash.as_str());
    assert_eq!(serde_json::from_value::<DataVerdict>(json).unwrap(), v);
}

// ---------------------------------------------------------------- policy

fn def(on_finding: Policy, unscanned: Policy) -> DataSafetyDef {
    DataSafetyDef {
        rules: None,
        on_finding,
        unscanned,
    }
}

fn verdict_with(findings: Vec<Finding>, marker: MarkerVerdict, hash: &str) -> DataVerdict {
    let clean = findings.is_empty() && !marker.is_unscrubbed();
    DataVerdict {
        clean,
        findings,
        marker,
        sample: SampleStatus::Scanned,
        rules_hash: Some(hash.into()),
        scanned_values: 10,
        truncated_values: 0,
    }
}

#[test]
fn enforcement_follows_the_policy_table() {
    let dirty = verdict_with(
        vec![hook_finding(
            "email",
            "customers",
            "email",
            2,
            Severity::Bad,
        )],
        MarkerVerdict::NotDefined,
        "h1",
    );
    let unscrubbed = verdict_with(
        vec![],
        MarkerVerdict::Unscrubbed {
            reason: UnscrubbedReason::Missing,
            version: None,
            at: None,
        },
        "h1",
    );
    let clean = verdict_with(vec![], MarkerVerdict::NotDefined, "h1");
    let ack = acknowledge(&dirty, "Alex", "seeded fixtures", "2026-01-01T00:00:00Z");
    use EnforcementCause as C;
    let warn = def(Policy::Warn, Policy::Warn);
    let stop = def(Policy::Stop, Policy::BlockAgents);
    let up = def(Policy::BlockUp, Policy::Warn);
    let invalid = def(Policy::BlockBuild, Policy::BlockUp);
    #[allow(clippy::type_complexity)]
    let cases: Vec<(
        Option<&DataSafetyDef>,
        Option<&DataVerdict>,
        Option<&Acknowledgement>,
        Option<Policy>,
        C,
    )> = vec![
        // defaults: block agents on a finding, warn when unscanned
        (
            None,
            Some(&dirty),
            None,
            Some(Policy::BlockAgents),
            C::NotClean,
        ),
        (None, None, None, Some(Policy::Warn), C::Unscanned),
        (None, Some(&clean), None, None, C::Clean),
        (
            None,
            Some(&unscrubbed),
            None,
            Some(Policy::BlockAgents),
            C::NotClean,
        ),
        (
            Some(&warn),
            Some(&dirty),
            None,
            Some(Policy::Warn),
            C::NotClean,
        ),
        (
            Some(&stop),
            Some(&dirty),
            None,
            Some(Policy::Stop),
            C::NotClean,
        ),
        (
            Some(&stop),
            None,
            None,
            Some(Policy::BlockAgents),
            C::Unscanned,
        ),
        (
            Some(&up),
            Some(&unscrubbed),
            None,
            Some(Policy::BlockUp),
            C::NotClean,
        ),
        // a policy data safety does not accept falls back, never passes through
        (
            Some(&invalid),
            Some(&dirty),
            None,
            Some(Policy::BlockAgents),
            C::NotClean,
        ),
        (
            Some(&invalid),
            None,
            None,
            Some(Policy::BlockAgents),
            C::Unscanned,
        ),
        // an acknowledgement of this verdict lifts it, of another does not
        (Some(&stop), Some(&dirty), Some(&ack), None, C::Acknowledged),
        (
            Some(&stop),
            Some(&unscrubbed),
            Some(&ack),
            Some(Policy::Stop),
            C::NotClean,
        ),
        // nothing to acknowledge on an unscanned component
        (
            Some(&stop),
            None,
            Some(&ack),
            Some(Policy::BlockAgents),
            C::Unscanned,
        ),
    ];
    for (d, v, a, policy, cause) in cases {
        let e = enforcement(d, v, a);
        assert_eq!(e.policy, policy, "{d:?} {v:?} {a:?}");
        assert_eq!(e.cause, cause, "{d:?} {v:?} {a:?}");
        if let Some(p) = e.policy {
            assert!(p.allowed_for_data(), "{p}");
        }
    }
}

#[test]
fn acknowledgements_are_tied_to_exact_findings_counts_marker_and_rules() {
    let f1 = hook_finding("email", "customers", "email", 2, Severity::Bad);
    let f2 = hook_finding("phone", "customers", "phone", 9, Severity::Warn);
    let base = verdict_with(
        vec![f1.clone(), f2.clone()],
        MarkerVerdict::NotDefined,
        "h1",
    );
    let ack = acknowledge(&base, "Alex", "seeded fixtures", "2026-01-01T00:00:00Z");
    assert_eq!(ack.who, "Alex");
    assert_eq!(
        ack.covers,
        vec![
            "email in customers.email#2".to_string(),
            "phone in customers.phone#9".to_string(),
            "rules:h1".to_string()
        ]
    );
    assert!(ack_clears(&ack, &base));

    let with_count = |n: u32| Finding {
        count: n,
        ..f1.clone()
    };
    let same_counts_reordered = verdict_with(
        vec![f2.clone(), f1.clone()],
        MarkerVerdict::NotDefined,
        "h1",
    );
    let count_up = verdict_with(
        vec![with_count(40), f2.clone()],
        MarkerVerdict::NotDefined,
        "h1",
    );
    let count_down = verdict_with(
        vec![with_count(1), f2.clone()],
        MarkerVerdict::NotDefined,
        "h1",
    );
    let f3 = hook_finding("iban", "payments", "account", 1, Severity::Bad);
    let finding_added = verdict_with(
        vec![f1.clone(), f2.clone(), f3],
        MarkerVerdict::NotDefined,
        "h1",
    );
    let finding_removed = verdict_with(vec![f1.clone()], MarkerVerdict::NotDefined, "h1");
    let new_rules = verdict_with(
        vec![f1.clone(), f2.clone()],
        MarkerVerdict::NotDefined,
        "h2",
    );
    let marker_now_missing = verdict_with(
        vec![f1.clone(), f2.clone()],
        MarkerVerdict::Unscrubbed {
            reason: UnscrubbedReason::Missing,
            version: None,
            at: None,
        },
        "h1",
    );
    let clean = verdict_with(vec![], MarkerVerdict::NotDefined, "h1");
    let cases: &[(&DataVerdict, bool, &str)] = &[
        (&base, true, "the same verdict again"),
        (
            &same_counts_reordered,
            true,
            "the same counts in another order",
        ),
        (&count_up, false, "a count going up"),
        (&count_down, false, "a count going down"),
        (&finding_added, false, "a finding added"),
        (&finding_removed, false, "a finding gone"),
        (&new_rules, false, "a rules change"),
        (&marker_now_missing, false, "a marker change"),
        (&clean, false, "a clean rescan ends it"),
    ];
    for (next, survives, why) in cases {
        assert_eq!(ack_survives(&ack, next), *survives, "{why}");
        // the block lifts on exactly the same verdicts the ack survives
        if !next.clean {
            assert_eq!(ack_clears(&ack, next), *survives, "{why}");
            let e = enforcement(None, Some(next), Some(&ack));
            let lifted = e.cause == EnforcementCause::Acknowledged;
            assert_eq!(lifted, *survives, "{why}");
        }
    }

    // a marker that was acknowledged as unscrubbed, then changes reason
    let unscrubbed = |reason| {
        verdict_with(
            vec![],
            MarkerVerdict::Unscrubbed {
                reason,
                version: None,
                at: None,
            },
            "h1",
        )
    };
    let marker_ack = acknowledge(
        &unscrubbed(UnscrubbedReason::Missing),
        "Alex",
        "known",
        "2026-01-01T00:00:00Z",
    );
    assert!(ack_survives(
        &marker_ack,
        &unscrubbed(UnscrubbedReason::Missing)
    ));
    assert!(!ack_survives(
        &marker_ack,
        &unscrubbed(UnscrubbedReason::NotScrubbed)
    ));
    // a clean verdict needs nothing cleared
    assert!(clean.ack_keys().is_empty());
    assert!(ack_clears(&ack, &clean));

    let refused = DataVerdict {
        clean: false,
        findings: vec![],
        marker: MarkerVerdict::NotDefined,
        sample: SampleStatus::Refused {
            reason: "too large".into(),
        },
        rules_hash: None,
        scanned_values: 0,
        truncated_values: 0,
    };
    assert_eq!(refused.ack_keys(), vec!["rules:none", "sample:refused"]);
}

// --------------------------------------------------------------- starter

#[test]
fn the_starter_file_loads_as_shipped_and_with_every_rule_on() {
    let shipped = DataRules::parse(STARTER, "data-safety.starter.yml").unwrap();
    assert!(
        shipped.rules.is_empty(),
        "every starter rule ships commented out"
    );

    let on = DataRules::parse(&starter_enabled(), "data-safety.starter.yml").unwrap();
    let ids: Vec<&str> = on.rules.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "email-real-domain",
            "email-deliverable",
            "card-number",
            "iban",
            "us-ssn",
            "phone",
            "custom-customer-ids"
        ]
    );
    assert_eq!(on.rule("phone").unwrap().threshold, 5);
    assert_eq!(
        on.rule("card-number").unwrap().validate,
        Some(Validator::Luhn)
    );
}

#[test]
fn the_starter_rules_detect_what_they_say() {
    let r = rules(&starter_enabled());
    let cases: &[(&str, &str, bool)] = &[
        (
            "email-real-domain",
            concat!("alex", "@mailhost.co.uk"),
            true,
        ),
        (
            "email-real-domain",
            concat!("ALEX", "@MAILHOST.CO.UK"),
            true,
        ),
        ("email-real-domain", "alex@example.com", false),
        ("email-real-domain", "Sam@Example.Org", false),
        ("email-real-domain", "sam@test.invalid", false),
        (
            "email-deliverable",
            concat!("Alex <alex", "@mailhost.co.uk>"),
            true,
        ),
        ("email-deliverable", "alex@shop.test", false),
        ("card-number", "4111 1111 1111 1111", true),
        ("card-number", "4111 1111 1111 1112", false),
        ("card-number", "1234567890123", false),
        ("iban", "GB82 WEST 1234 5698 7654 32", true),
        ("iban", "GB00 WEST 1234 5698 7654 32", false),
        ("us-ssn", "078-05-1120", true),
        ("us-ssn", "000-12-3456", false),
        ("us-ssn", "666-12-3456", false),
        ("us-ssn", "912-12-3456", false),
        ("us-ssn", "123-00-4567", false),
        ("us-ssn", "123-45-0000", false),
        ("phone", "+44 20 7946 0958", true),
        ("phone", "+1 555 010 4477", false),
        ("phone", "(555) 010-4477", false),
        ("custom-customer-ids", "CRM-88341-ZQ", true),
        ("custom-customer-ids", "TEST-0001", false),
    ];
    for (rule, value, want) in cases {
        assert_eq!(
            r.rule(rule).unwrap().matches(value),
            *want,
            "{rule} on {value:?}"
        );
    }
    // the phone threshold: four real-looking numbers are noise, five are not
    let four = [
        "+44 20 7946 0958",
        "+1 202 918 2132",
        "(312) 948-7731",
        "+49 30 9017 6655",
    ];
    let out = r.scan(&[row("customers", "phone", &four)]).unwrap();
    assert!(
        !out.findings.iter().any(|f| f.rule == "phone"),
        "{:?}",
        out.findings
    );
    let mut five = four.to_vec();
    five.push("+33 1 7039 4412");
    let out = r.scan(&[row("customers", "phone", &five)]).unwrap();
    assert!(out
        .findings
        .iter()
        .any(|f| f.rule == "phone" && f.count == 5));
    // custom ids are scoped to their column
    assert!(!r
        .rule("custom-customer-ids")
        .unwrap()
        .in_scope("customers", "email"));
}
