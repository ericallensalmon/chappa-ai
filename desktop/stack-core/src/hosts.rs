//! Does the stack's hostname resolve to this machine?
//!
//! A stack has one hostname (`dev.example.test`) and tells its services
//! apart by port. Application code checks the request's domain, so the name
//! is required, not cosmetic, and it is resolved through the hosts file
//! alone: no DNS. The check before `up` reads that file and says whether the
//! name maps to loopback. If it does not, the result carries the exact line
//! to add and the elevated command that adds it, as a `terminal` action for
//! the user to approve. Nothing here edits the file: unknown entries are
//! never touched, and a name that already points somewhere else is reported,
//! not overwritten.
//!
//! Parsing is the hosts-file grammar every platform shares: one entry per
//! line, `<address> <name> [<alias>…]`, `#` starts a comment anywhere on the
//! line, blank lines are skipped, names compare case-insensitively. IPv6
//! addresses may carry a zone (`fe80::1%lo0`), which is stripped before the
//! loopback test.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::PathBuf;

use crate::jobs::RunBlock;

/// The address the added line uses.
pub const LOOPBACK_V4: &str = "127.0.0.1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostsEntry {
    pub address: String,
    pub names: Vec<String>,
}

/// Every active entry of a hosts file.
pub fn parse_hosts(text: &str) -> Vec<HostsEntry> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                return None;
            }
            let mut words = line.split_whitespace();
            let address = words.next()?.to_string();
            let names: Vec<String> = words.map(str::to_string).collect();
            if names.is_empty() {
                return None;
            }
            Some(HostsEntry { address, names })
        })
        .collect()
}

/// `127.0.0.0/8` or `::1`, a zone suffix ignored.
pub fn is_loopback(address: &str) -> bool {
    let bare = address.split('%').next().unwrap_or(address);
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Where the hostname points on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "status")]
pub enum HostsStatus {
    /// Mapped to loopback (v4, v6 or both).
    Loopback { addresses: Vec<String> },
    /// Mapped only to other addresses: the line must be fixed by hand.
    Elsewhere { addresses: Vec<String> },
    /// No entry at all.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostsCheck {
    pub hostname: String,
    pub status: HostsStatus,
    /// The line that makes the name resolve, when one should be added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_to_add: Option<String>,
    /// The elevated command that adds it, when adding is the right fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<RunBlock>,
}

impl HostsCheck {
    pub fn ok(&self) -> bool {
        matches!(self.status, HostsStatus::Loopback { .. })
    }

    /// One sentence for the check's outcome.
    pub fn reason(&self) -> String {
        match &self.status {
            HostsStatus::Loopback { .. } => format!("{} resolves to loopback", self.hostname),
            HostsStatus::Elsewhere { addresses } => format!(
                "{} resolves to {} in the hosts file, not loopback; change that entry by hand",
                self.hostname,
                addresses.join(", ")
            ),
            HostsStatus::Missing => format!(
                "{} is not in the hosts file; add `{}`",
                self.hostname,
                self.line_to_add.clone().unwrap_or_default()
            ),
        }
    }
}

/// The platform whose hosts file and elevation command to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostOs {
    Windows,
    Unix,
}

impl HostOs {
    pub fn current() -> HostOs {
        if cfg!(windows) {
            HostOs::Windows
        } else {
            HostOs::Unix
        }
    }
}

/// The hosts file of a machine running `os`. The Windows path is built
/// with Windows separators whatever OS computes it, so the elevated
/// command names the same file wherever it is put together.
pub fn hosts_file_path(os: HostOs) -> PathBuf {
    match os {
        HostOs::Windows => {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            let root = root.trim_end_matches(['\\', '/']);
            PathBuf::from(format!(r"{root}\System32\drivers\etc\hosts"))
        }
        HostOs::Unix => PathBuf::from("/etc/hosts"),
    }
}

/// `127.0.0.1 <hostname>`.
pub fn line_to_add(hostname: &str) -> String {
    format!("{LOOPBACK_V4} {hostname}")
}

/// The elevated command that appends the line. Elevation needs a shell by
/// nature (UAC on Windows, `sudo` elsewhere); the argv itself is spawned
/// without one, and what the elevated shell runs is shown in full in the
/// confirm dialog. The hostname is restricted to the key alphabet plus
/// dots before it is interpolated, so it cannot carry quoting.
pub fn fix_command(hostname: &str, os: HostOs) -> Option<RunBlock> {
    if !is_safe_hostname(hostname) {
        return None;
    }
    let line = line_to_add(hostname);
    let path = hosts_file_path(os);
    let argv: Vec<String> = match os {
        HostOs::Windows => vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-Command".into(),
            format!(
                "Start-Process powershell.exe -Verb RunAs -Wait -ArgumentList '-NoProfile','-Command','Add-Content -Path ''{}'' -Value ''{line}'''",
                path.display()
            ),
        ],
        HostOs::Unix => vec![
            "sudo".into(),
            "sh".into(),
            "-c".into(),
            format!("printf '%s\\n' '{line}' >> {}", path.display()),
        ],
    };
    Some(
        RunBlock::new(argv, std::env::temp_dir())
            .titled(format!("add {hostname} to the hosts file")),
    )
}

/// Letters, digits, `-` and `.`: a hostname that is safe to put in a line.
pub fn is_safe_hostname(hostname: &str) -> bool {
    !hostname.is_empty()
        && hostname.len() <= 253
        && hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !hostname.starts_with('.')
        && !hostname.ends_with('.')
}

/// The check over the file's text.
pub fn check_hosts_text(text: &str, hostname: &str, os: HostOs) -> HostsCheck {
    let wanted = hostname.to_ascii_lowercase();
    let mut loopback = Vec::new();
    let mut elsewhere = Vec::new();
    for entry in parse_hosts(text) {
        if entry.names.iter().any(|n| n.to_ascii_lowercase() == wanted) {
            if is_loopback(&entry.address) {
                loopback.push(entry.address);
            } else {
                elsewhere.push(entry.address);
            }
        }
    }
    let status = if !loopback.is_empty() {
        HostsStatus::Loopback {
            addresses: loopback,
        }
    } else if !elsewhere.is_empty() {
        HostsStatus::Elsewhere {
            addresses: elsewhere,
        }
    } else {
        HostsStatus::Missing
    };
    let (line, fix) = match status {
        HostsStatus::Missing => (Some(line_to_add(hostname)), fix_command(hostname, os)),
        _ => (None, None),
    };
    HostsCheck {
        hostname: hostname.to_string(),
        status,
        line_to_add: line,
        fix,
    }
}

/// Read this machine's hosts file and check it. An unreadable file counts
/// as missing the name (the fix is still offered).
pub fn check_hosts_file(hostname: &str) -> HostsCheck {
    let os = HostOs::current();
    let text = std::fs::read_to_string(hosts_file_path(os)).unwrap_or_default();
    check_hosts_text(&text, hostname, os)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Copyright (c) 1993-2009 Microsoft Corp.
#
#\t127.0.0.1       localhost
\t127.0.0.1\tlocalhost
::1             localhost ip6-localhost
fe80::1%lo0\tlocalhost
10.0.0.5   intranet.example   # office box
127.0.0.1 Dev.Example.Test other.example.test
   # 127.0.0.1 commented.example.test

192.168.1.20 elsewhere.example.test
";

    #[test]
    fn parses_entries_skipping_comments_and_blank_lines() {
        let entries = parse_hosts(SAMPLE);
        let names: Vec<(String, Vec<String>)> = entries
            .iter()
            .map(|e| (e.address.clone(), e.names.clone()))
            .collect();
        assert_eq!(names.len(), 6);
        assert_eq!(names[0], ("127.0.0.1".into(), vec!["localhost".into()]));
        assert_eq!(
            names[1],
            (
                "::1".into(),
                vec!["localhost".into(), "ip6-localhost".into()]
            )
        );
        assert_eq!(names[2].0, "fe80::1%lo0");
        assert_eq!(
            names[3],
            ("10.0.0.5".into(), vec!["intranet.example".into()])
        );
        assert!(entries
            .iter()
            .all(|e| !e.names.iter().any(|n| n == "commented.example.test")));
    }

    #[test]
    fn loopback_detection_covers_v4_range_v6_and_zones() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("127.255.0.9"));
        assert!(is_loopback("::1"));
        assert!(is_loopback("::1%lo0"));
        assert!(!is_loopback("fe80::1%lo0"));
        assert!(!is_loopback("128.0.0.1"));
        assert!(!is_loopback("10.0.0.5"));
        assert!(!is_loopback("not-an-ip"));
    }

    #[test]
    fn check_is_case_insensitive_and_distinguishes_missing_from_elsewhere() {
        let ok = check_hosts_text(SAMPLE, "dev.example.test", HostOs::Unix);
        assert!(ok.ok());
        assert_eq!(
            ok.status,
            HostsStatus::Loopback {
                addresses: vec!["127.0.0.1".into()]
            }
        );
        assert!(ok.fix.is_none() && ok.line_to_add.is_none());

        let v6 = check_hosts_text("::1 six.example.test\n", "six.example.test", HostOs::Unix);
        assert!(v6.ok());

        let elsewhere = check_hosts_text(SAMPLE, "elsewhere.example.test", HostOs::Unix);
        assert_eq!(
            elsewhere.status,
            HostsStatus::Elsewhere {
                addresses: vec!["192.168.1.20".into()]
            }
        );
        assert!(elsewhere.fix.is_none(), "never offer to add a second line");
        assert!(elsewhere.reason().contains("by hand"));

        let missing = check_hosts_text(SAMPLE, "commented.example.test", HostOs::Unix);
        assert_eq!(missing.status, HostsStatus::Missing);
        assert_eq!(
            missing.line_to_add.as_deref(),
            Some("127.0.0.1 commented.example.test")
        );
        assert!(missing
            .reason()
            .contains("add `127.0.0.1 commented.example.test`"));
        let fix = missing.fix.unwrap();
        assert_eq!(fix.argv[..3], ["sudo", "sh", "-c"].map(String::from));
        assert!(fix.argv[3].contains("'127.0.0.1 commented.example.test' >> /etc/hosts"));
    }

    #[test]
    fn windows_fix_elevates_through_runas_and_names_the_system_hosts_file() {
        let fix = fix_command("dev.example.test", HostOs::Windows).unwrap();
        assert_eq!(fix.argv[0], "powershell.exe");
        assert_eq!(fix.argv[1..3], ["-NoProfile", "-Command"].map(String::from));
        assert!(fix.argv[3].contains("-Verb RunAs"));
        assert!(fix.argv[3].contains("drivers\\etc\\hosts"));
        assert!(fix.argv[3].contains("127.0.0.1 dev.example.test"));
        assert_eq!(
            fix.title.as_deref(),
            Some("add dev.example.test to the hosts file")
        );
    }

    #[test]
    fn unsafe_hostnames_get_no_command() {
        assert!(fix_command("dev.example.test", HostOs::Unix).is_some());
        assert!(fix_command("bad'name", HostOs::Unix).is_none());
        assert!(fix_command("has space", HostOs::Unix).is_none());
        assert!(fix_command(".leading", HostOs::Unix).is_none());
        assert!(fix_command("", HostOs::Unix).is_none());
        let check = check_hosts_text("", "bad'name", HostOs::Unix);
        assert_eq!(check.status, HostsStatus::Missing);
        assert!(check.fix.is_none());
        assert!(check.line_to_add.is_some());
    }
}
