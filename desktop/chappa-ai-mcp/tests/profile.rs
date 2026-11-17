//! The MCP server finds the app through `CHAPPA_PROFILE_DIR` the same way
//! the app places its files, so an agent started by a separate profile talks
//! to that profile's app and never reads the everyday profile's token.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chappa_ai_mcp::profile::PROFILE_DIR_ENV;
use chappa_ai_mcp::{ENDPOINT_FILE, TOKEN_FILE};

/// Every variable that could point the server somewhere else.
const TARGET_ENVS: [&str; 5] = [
    "CHAPPA_AI_CONTROL_URL",
    "CHAPPA_AI_CONTROL_ENDPOINT_FILE",
    "CHAPPA_AI_CONTROL_TOKEN",
    "CHAPPA_AI_CONTROL_TOKEN_FILE",
    PROFILE_DIR_ENV,
];

fn server(profile: Option<&Path>) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chappa-ai-mcp"));
    for name in TARGET_ENVS {
        cmd.env_remove(name);
    }
    if let Some(dir) = profile {
        cmd.env(PROFILE_DIR_ENV, dir);
    }
    cmd
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "chappa-ai-mcp-profile-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `path` as it appears inside a JSON string.
fn json_text(path: &Path) -> String {
    let quoted = serde_json::to_string(&path.display().to_string()).unwrap();
    quoted[1..quoted.len() - 1].to_owned()
}

#[test]
fn help_names_the_profiles_token_and_endpoint_files() {
    let dir = scratch("help");
    let out = server(Some(&dir)).arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    let config = dir.join("config");
    assert!(
        text.contains(&config.join(TOKEN_FILE).display().to_string()),
        "{text}"
    );
    assert!(
        text.contains(&config.join(ENDPOINT_FILE).display().to_string()),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_profile_help_names_the_platform_folder() {
    let dir = scratch("unset");
    let out = server(None).arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains(chappa_ai_mcp::APP_IDENTIFIER), "{text}");
    assert!(!text.contains(&dir.display().to_string()), "{text}");
    let expected = chappa_ai_mcp::profile::platform_config_dir().join(TOKEN_FILE);
    // This test process may itself run under a profile; the child does not.
    if std::env::var_os(PROFILE_DIR_ENV).is_none() {
        assert_eq!(chappa_ai_mcp::default_token_path(), expected);
    }
    assert!(text.contains(&expected.display().to_string()), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_tool_call_looks_for_the_app_in_the_profile() {
    let dir = scratch("call");
    let mut child = server(Some(&dir))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        let lines = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_processes","arguments":{}}}"#,
        ];
        for line in lines {
            writeln!(stdin, "{line}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    // The token is read first; either file it names is the profile's.
    let config = dir.join("config");
    let named = [config.join(TOKEN_FILE), config.join(ENDPOINT_FILE)];
    assert!(named.iter().any(|p| text.contains(&json_text(p))), "{text}");
    let platform = chappa_ai_mcp::profile::platform_config_dir();
    assert!(!text.contains(&json_text(&platform)), "{text}");
    // Nothing was created on the way: the server only reads.
    assert!(!dir.join("config").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
