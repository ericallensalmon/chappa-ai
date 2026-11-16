//! The source protocol: the identity proof against the shared vectors, the
//! bearer token on every other route, loopback-only listening, and a
//! manifest the app accepts (its rules are restated here, since the app
//! crate is not a dependency of this one).

mod common;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use chappa_stack::identity::{proof, Token};
use chappa_stack::project::ProjectConfig;
use chappa_stack::server::{self, parse_listen_addr, Source};
use common::*;
use stack_core::cli::FakeCli;
use stack_core::probe::FakeHttp;

#[test]
fn the_proof_matches_every_shared_identity_vector() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../sources/fixture/test/identity-vectors.json"),
    )
    .unwrap();
    let vectors: Vec<Value> = serde_json::from_str(&text).unwrap();
    assert!(vectors.len() >= 5);
    for v in vectors {
        let token = Token::parse(v["token"].as_str().unwrap()).unwrap();
        let (challenge, addr) = (
            v["challenge"].as_str().unwrap(),
            v["addr"].as_str().unwrap(),
        );
        assert_eq!(
            token.proof(challenge, addr),
            v["proof"].as_str().unwrap(),
            "{v}"
        );
    }
    // Any change to an input changes the proof.
    let t = Token::parse(TOKEN).unwrap();
    let c = "00".repeat(32);
    let base = t.proof(&c, "127.0.0.1:5000");
    assert_ne!(base, t.proof(&c, "127.0.0.1:5001"));
    assert_ne!(base, t.proof(&"01".repeat(32), "127.0.0.1:5000"));
    assert_ne!(base, proof(&[1u8; 32], &c, "127.0.0.1:5000"));
}

#[test]
fn every_route_but_identity_needs_the_token() {
    let p = Project::example();
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let challenge = "ab".repeat(32);
    let (status, body) = src.handle(
        "GET",
        &format!("/chappa/v1/identity?challenge={challenge}"),
        None,
        false,
        "",
    );
    assert_eq!(status, 200);
    assert_eq!(
        body["proof"],
        Token::parse(TOKEN)
            .unwrap()
            .proof(&challenge, "127.0.0.1:5000")
    );
    let (status, _) = src.handle("GET", "/chappa/v1/identity?challenge=AB", None, false, "");
    assert_eq!(status, 400);
    let wrong = format!("Bearer {}", "ff".repeat(32));
    for (method, path) in [
        ("GET", "/chappa/v1/manifest"),
        ("GET", "/api/environment"),
        ("POST", "/api/actions"),
        ("GET", "/api/jobs/j20261114T120000Z-abcdef"),
        ("POST", "/api/jobs/j20261114T120000Z-abcdef"),
    ] {
        for auth in [None, Some(wrong.as_str()), Some(TOKEN)] {
            let (status, body) = src.handle(method, path, auth, false, "{}");
            assert_eq!(status, 401, "{method} {path} with {auth:?}");
            assert_eq!(body["kind"], "unauthorized");
        }
    }
    let (status, _) = get(&src, "/chappa/v1/manifest");
    assert_eq!(status, 200);
    let (status, _) = get(&src, "/nowhere");
    assert_eq!(status, 404);
}

#[test]
fn only_loopback_addresses_are_listened_on() {
    for ok in ["127.0.0.1:0", "127.0.0.1:5000", "127.1.2.3:80", "[::1]:9"] {
        assert!(parse_listen_addr(ok).is_ok(), "{ok}");
    }
    for bad in [
        "0.0.0.0:5000",
        "[::]:5000",
        "10.0.0.5:1",
        "192.168.1.2:80",
        "localhost:5000",
        "127.0.0.1",
        "example.test:1",
    ] {
        assert!(parse_listen_addr(bad).is_err(), "{bad}");
        assert!(server::bind(bad).is_err(), "{bad}");
    }
}

/// The app's manifest rules (contract version, source fields, view names
/// and paths, endpoint path shape, link-host shape), restated.
fn app_accepts(m: &Value) -> Result<(), String> {
    let obj = m.as_object().ok_or("not an object")?;
    if obj.get("contract").and_then(Value::as_u64) != Some(1) {
        return Err("needs update".into());
    }
    let src = obj
        .get("source")
        .and_then(Value::as_object)
        .ok_or("no source")?;
    for k in ["id", "label", "version"] {
        let v = src.get(k).and_then(Value::as_str).unwrap_or("");
        if v.trim().is_empty() {
            return Err(format!("source.{k}"));
        }
    }
    let path_ok = |p: &str| {
        p.starts_with('/')
            && !p.starts_with("//")
            && !p.contains("://")
            && !p.bytes().any(|b| b <= b' ' || b == 0x7f)
            && !p.split(['/', '?', '#']).any(|s| s == "..")
    };
    if let Some(views) = obj.get("views") {
        for (name, v) in views.as_object().ok_or("views")? {
            if !["work", "prs", "attention", "environment"].contains(&name.as_str()) {
                continue;
            }
            let list = v["list"].as_str().ok_or(format!("views.{name}.list"))?;
            if !path_ok(list) {
                return Err(format!("views.{name}.list"));
            }
            if let Some(p) = v.get("pollSeconds") {
                p.as_f64().ok_or("pollSeconds")?;
            }
        }
    }
    for k in ["actions", "jobs", "workflows", "ownerUpdates"] {
        if let Some(p) = obj.get(k) {
            if !path_ok(p.as_str().ok_or(k)?) {
                return Err(k.into());
            }
        }
    }
    for h in obj
        .get("linkHosts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let h = h.as_str().ok_or("linkHosts entry")?;
        if h.trim().is_empty() || h.contains("://") || h.contains('/') {
            return Err(format!("linkHosts {h}"));
        }
        // The one wildcard is a whole port, on a host that may carry it.
        match project_model::link_hosts::any_port_host(h) {
            Some(host) => project_model::link_hosts::check_any_port_host(&host)
                .map_err(|e| format!("linkHosts {h}: {e}"))?,
            None if h.contains('*') => return Err(format!("linkHosts {h}")),
            None => {}
        }
    }
    Ok(())
}

#[test]
fn the_manifest_is_one_the_app_accepts_and_links_only_the_stack_hostname() {
    let p = Project::example();
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, m) = get(&src, "/chappa/v1/manifest");
    app_accepts(&m).unwrap();
    assert_eq!(m["source"]["id"], "stack:local");
    assert_eq!(m["views"]["environment"]["list"], "/api/environment");
    assert_eq!(m["actions"], "/api/actions");
    assert_eq!(m["jobs"], "/api/jobs/{id}");
    assert_eq!(m["ownerUpdates"], "/api/work/owner");
    assert!(m.get("roots").is_none());
    let hosts: Vec<&str> = m["linkHosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap())
        .collect();
    // The stack hostname on any port: api and web publish http URLs there,
    // and a status script may report more on ports nobody declared.
    assert_eq!(hosts, ["dev.example.test:*"]);
    // The restated rules do refuse what the app refuses.
    assert!(app_accepts(&json!({"contract": 2})).is_err());
    let mut bad = m.clone();
    bad["jobs"] = "http://elsewhere/jobs".into();
    assert!(app_accepts(&bad).is_err());
    bad = m.clone();
    bad["linkHosts"] = json!(["*.example.test"]);
    assert!(app_accepts(&bad).is_err());
    bad["linkHosts"] = json!(["test:*"]);
    assert!(app_accepts(&bad).is_err());
}

fn link_hosts_of(yml: &str) -> Vec<String> {
    let p = Project::with(yml, &[]);
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, m) = get(&src, "/chappa/v1/manifest");
    app_accepts(&m).unwrap();
    m["linkHosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap().to_string())
        .collect()
}

/// Without a hostname the stack links to this machine on any port; a
/// hostname the app takes no port wildcard on (one label) keeps one entry
/// per output port, so the manifest stays one the app accepts.
#[test]
fn link_hosts_take_any_port_where_the_app_allows_it_and_ports_elsewhere() {
    let script = |hostname: &str| {
        format!("stacks:\n  app:\n{hostname}    components:\n      app:\n        driver: script\n        up: [node, up.js]\n        ports: 2\n        outputs: {{ url: \"http://{{hostname}}:{{port:app:1}}\" }}\n")
    };
    assert_eq!(link_hosts_of(&script("")), ["127.0.0.1:*", "localhost:*"]);
    assert_eq!(
        link_hosts_of(&script("    hostname: dev.example.test\n")),
        ["dev.example.test:*"]
    );
    let single = link_hosts_of(&script("    hostname: devbox\n"));
    assert_eq!(single.len(), 1, "{single:?}");
    assert!(
        single[0].starts_with("devbox:") && !single[0].ends_with(":*"),
        "{single:?}"
    );
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
fn over_http_the_token_follows_the_proof_on_the_same_connection() {
    let p = Project::example();
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let (listener, bound) = server::bind("127.0.0.1:0").unwrap();
    let src = Source::new(
        f.world.clone(),
        ProjectConfig::new(&p.dir),
        Token::parse(TOKEN).unwrap(),
        bound.to_string(),
    );
    std::thread::spawn(move || server::serve(listener, src));
    let mut stream = TcpStream::connect(bound).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let challenge = "5a".repeat(32);
    let (status, body) = http(
        &mut stream,
        &mut reader,
        &format!("GET /chappa/v1/identity?challenge={challenge} HTTP/1.1\r\nHost: x\r\n\r\n"),
    );
    assert_eq!(status, 200);
    assert_eq!(
        body["proof"],
        Token::parse(TOKEN)
            .unwrap()
            .proof(&challenge, &bound.to_string())
    );
    let (status, m) = http(
        &mut stream,
        &mut reader,
        &format!(
            "GET /chappa/v1/manifest HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\n\r\n",
            bearer()
        ),
    );
    assert_eq!(status, 200);
    assert_eq!(m["contract"], 1);
    let (status, _) = http(
        &mut stream,
        &mut reader,
        "GET /api/environment HTTP/1.1\r\nHost: x\r\n\r\n",
    );
    assert_eq!(status, 401);
}
