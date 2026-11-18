//! The MCP server finds the app through `CHAPPA_PROFILE_DIR` the same way
//! the app places its files, so an agent started by a separate profile talks
//! to that profile's app and never reads the everyday profile's token.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chappa_ai_mcp::profile::PROFILE_DIR_ENV;
use chappa_ai_mcp::{identity_proof, Endpoint, ENDPOINT_FILE, TOKEN_FILE};

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

/// One request's line and headers off a keep-alive connection (the server
/// sends no bodies on GET); `None` when the peer hung up.
fn read_head(reader: &mut BufReader<std::net::TcpStream>) -> Option<Vec<String>> {
    let mut head = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end().to_owned();
        if line.is_empty() {
            return Some(head);
        }
        head.push(line);
    }
}

fn reply(stream: &mut std::net::TcpStream, status: u16, body: &str, close: bool) {
    let connection = if close { "Connection: close\r\n" } else { "" };
    let text = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{connection}\r\n{body}",
        body.len()
    );
    stream.write_all(text.as_bytes()).unwrap();
}

/// A stand-in app on a port the OS chose, as a profile's app binds: it
/// proves its identity like the app and answers `GET /processes` (with the
/// right token only) with one row named `marker`.
fn start_profile_app(token: &str, nonce: &str, marker: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (token, nonce, bound) = (token.to_owned(), nonce.to_owned(), addr.clone());
    let rows = serde_json::json!([{"id": 1, "name": marker}]).to_string();
    let auth = format!("Authorization: Bearer {token}");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            while let Some(head) = read_head(&mut reader) {
                let path = head[0].split_whitespace().nth(1).unwrap_or("").to_owned();
                if let Some(challenge) = path.strip_prefix("/identity?challenge=") {
                    let proof = identity_proof(&token, challenge, &bound, &nonce);
                    let body = serde_json::json!({"addr": bound, "nonce": nonce, "proof": proof});
                    reply(&mut stream, 200, &body.to_string(), false);
                    continue;
                }
                let authed = head.contains(&auth);
                match (authed, path.as_str()) {
                    (true, "/processes") => reply(&mut stream, 200, &rows, true),
                    (true, _) => reply(&mut stream, 404, r#"{"error":"unknown"}"#, true),
                    (false, _) => reply(&mut stream, 401, r#"{"error":"unauthorized"}"#, true),
                }
                break;
            }
        }
    });
    addr
}

#[test]
fn a_tool_call_reaches_the_app_on_the_port_the_profile_published() {
    let dir = scratch("reach");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let token = "profile-token";
    let addr = start_profile_app(token, "profile-run", "profile-row");
    assert!(!addr.ends_with(":8324"), "{addr}");
    std::fs::write(config.join(TOKEN_FILE), token).unwrap();
    let ep = Endpoint {
        addr: addr.clone(),
        nonce: "profile-run".into(),
        pid: Some(1),
    };
    std::fs::write(config.join(ENDPOINT_FILE), ep.to_json()).unwrap();

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
    let answer = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"] == 2)
        .unwrap_or_else(|| panic!("no answer to the tool call: {text}"));
    assert_ne!(answer["result"]["isError"], true, "{answer}");
    assert!(answer.to_string().contains("profile-row"), "{answer}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn help_registers_the_server_by_its_own_full_path() {
    let out = server(None).arg("--help").output().unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    let me = PathBuf::from(env!("CARGO_BIN_EXE_chappa-ai-mcp"));
    let line = text
        .lines()
        .find(|l| l.starts_with("Usage: claude mcp add chappa-ai -- "))
        .unwrap_or_else(|| panic!("{text}"));
    let shown = line
        .strip_prefix("Usage: claude mcp add chappa-ai -- \"")
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("{line}"));
    let shown = Path::new(shown);
    assert!(shown.is_absolute(), "{line}");
    assert_eq!(
        shown.canonicalize().unwrap(),
        me.canonicalize().unwrap(),
        "{line}"
    );
}
