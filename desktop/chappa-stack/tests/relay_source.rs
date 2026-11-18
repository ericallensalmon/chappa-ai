//! `chappa-stack relay` as a source over HTTP: the identity proof, the
//! token on every other route, a manifest with the Environment view only,
//! and that view's Relay section, firewall row and banner, through fakes.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chappa_stack::identity::Token;
use chappa_stack::relay_firewall::FirewallCheck;
use chappa_stack::relay_probe::ContainerTest;
use chappa_stack::relay_source::{parse_engine, parse_ports, RelayConfig, RelaySource};
use chappa_stack::server;
use indexmap::IndexMap;
use stack_core::cli::{CliError, CliOutput, EngineCli, Invocation};
use stack_core::compose::{FakeGateways, Gateway};
use stack_core::hosts::HostOs;

/// Answers the firewall check with no rules, and an image listing with
/// nothing.
struct Quiet;

impl EngineCli for Quiet {
    fn run(&self, _inv: &Invocation) -> Result<CliOutput, CliError> {
        Ok(CliOutput {
            status: Some(0),
            stdout: r#"{"rules":[],"interfaces":[]}"#.into(),
            stderr: String::new(),
        })
    }
}

fn http(stream: &mut TcpStream, reader: &mut BufReader<TcpStream>, req: &str) -> (u16, Value) {
    stream.write_all(req.as_bytes()).unwrap();
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status: u16 = status_line.split(' ').nth(1).unwrap().parse().unwrap();
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[test]
fn the_standalone_relay_is_a_source_with_one_relay_block() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut gw = IndexMap::new();
    gw.insert("docker".to_string(), Gateway::literal("127.0.0.1"));
    let token = Token::random().unwrap();
    let (listener, bound) = server::bind("127.0.0.1:0").unwrap();
    let quiet: Arc<dyn EngineCli + Send + Sync> = Arc::new(Quiet);
    let src = RelaySource::new(
        RelayConfig::new(
            vec![parse_engine("docker").unwrap()],
            parse_ports(&port.to_string()).unwrap(),
        ),
        token.clone(),
        bound.to_string(),
        Arc::new(FakeGateways(gw)),
        FirewallCheck::new(
            HostOs::Windows,
            quiet.clone(),
            Some(r"C:\Apps\chappa-stack.exe".into()),
        ),
        ContainerTest::new(quiet),
    );
    let refresher = src.start();
    let serving = src.clone();
    std::thread::spawn(move || server::serve(listener, serving));

    let mut stream = TcpStream::connect(bound).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let challenge = "5a".repeat(32);
    let (status, body) = http(
        &mut stream,
        &mut reader,
        &format!("GET /chappa/v1/identity?challenge={challenge} HTTP/1.1\r\nHost: x\r\n\r\n"),
    );
    assert_eq!(status, 200);
    assert_eq!(body["proof"], token.proof(&challenge, &bound.to_string()));
    let (status, _) = http(
        &mut stream,
        &mut reader,
        "GET /api/environment HTTP/1.1\r\nHost: x\r\n\r\n",
    );
    assert_eq!(status, 401, "nothing but the identity without the token");

    let auth = format!("Authorization: Bearer {}", token.hex());
    let (status, m) = http(
        &mut stream,
        &mut reader,
        &format!("GET /chappa/v1/manifest HTTP/1.1\r\nHost: x\r\n{auth}\r\n\r\n"),
    );
    assert_eq!(status, 200);
    assert_eq!(m["contract"], 1);
    assert_eq!(m["source"]["id"], "relay");
    assert_eq!(
        m["views"].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["environment"]
    );

    // The firewall check runs off the request path; wait for its banner.
    let deadline = Instant::now() + Duration::from_secs(10);
    let env = loop {
        let (status, env) = http(
            &mut stream,
            &mut reader,
            &format!("GET /api/environment HTTP/1.1\r\nHost: x\r\n{auth}\r\n\r\n"),
        );
        assert_eq!(status, 200);
        if env["banners"].as_array().is_some_and(|b| !b.is_empty()) {
            break env;
        }
        assert!(Instant::now() < deadline, "no firewall banner: {env}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(env["components"], json!([]));
    let section = &env["facts"][0];
    assert_eq!(section["id"], "relay");
    let rows = section["rows"].as_array().unwrap();
    assert_eq!(rows[0]["label"], "docker");
    let listening = rows[0]["value"].as_str().unwrap();
    assert!(
        listening.starts_with(&format!("127.0.0.1 port {port} → 127.0.0.1 (same ports)")),
        "{listening}"
    );
    assert!(listening.contains("serves only 127.0.0.0/8"), "{listening}");
    assert!(rows.iter().any(|r| r["label"] == "Windows Firewall"));
    assert_eq!(
        env["banners"][0]["title"],
        "Windows Firewall has no rule for the relay"
    );
    let allow = &env["banners"][0]["actions"][0];
    assert!(allow["confirm"]
        .as_str()
        .unwrap()
        .contains(&format!("-LocalPort '{port}'")));

    // An unconfirmed or stale firewall action is refused.
    let post = |body: &str| {
        format!(
            "POST /api/actions HTTP/1.1\r\nHost: x\r\n{auth}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let (status, r) = http(
        &mut stream,
        &mut reader,
        &post(r#"{"id":"relay-firewall-allow","confirmed":false}"#),
    );
    assert_eq!(status, 409);
    assert_eq!(r["kind"], "confirmation-required");
    let (status, r) = http(
        &mut stream,
        &mut reader,
        &post(r#"{"id":"relay-firewall-allow","confirmed":true,"params":{"fingerprint":"stale"}}"#),
    );
    assert_eq!(status, 409, "{r}");
    let (status, _) = http(
        &mut stream,
        &mut reader,
        &post(r#"{"id":"up","confirmed":true}"#),
    );
    assert_eq!(status, 409);

    src.shutdown();
    refresher.join().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        TcpListener::bind(("127.0.0.1", port)).is_ok(),
        "the relay let go of its port"
    );
}
