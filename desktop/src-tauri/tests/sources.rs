//! Source host tests over real sockets: the identity handshake against a
//! hand-rolled server (good proof, wrong proof, an impostor that never sees
//! the token, a chunked reply), and the whole lifecycle against the example
//! source shipped in `desktop/sources/fixture` (launch with a fresh port and
//! token, identity, manifest, the trust gate, polling, a crash that keeps
//! the data, a restart with a new port and token).
//!
//! The fixture is a Node script; the end-to-end test skips itself (and says
//! so) when `node` is not on PATH, so the rest of the suite still runs.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_lib::sources::client::{SourceClient, SourceError};
use app_lib::sources::identity::{decode_token, new_token_hex, source_proof, ADDR_ENV, TOKEN_ENV};
use app_lib::sources::manifest::{parse_manifest, Endpoint};
use app_lib::sources::store::SourceStore;
use app_lib::sources::{SourceStatus, Sources, SourcesEmitter};
use serde_json::{json, Value};

// ---- a tiny source server for the handshake tests ------------------------------------

/// What the fake server saw: every request line and whether any request
/// carried an Authorization header (the one thing an impostor must never see).
#[derive(Default)]
struct Seen {
    requests: Vec<String>,
    saw_token: bool,
}

/// How the fake answers the identity route.
#[derive(Clone, Copy)]
enum Identity {
    /// The right proof for the token it was given.
    Honest,
    /// A proof computed from another token (the port was taken by a program
    /// that does not know the token).
    Impostor,
    /// Not a source at all: 404 on everything.
    NotASource,
}

fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&raw[..pos]).into_owned());
        }
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
    }
}

fn reply(stream: &mut TcpStream, status: u16, body: &str) {
    let _ = stream.write_all(
        format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes(),
    );
}

/// Start the fake on an ephemeral port. `chunked` answers the manifest with
/// chunked framing instead of a content length.
fn fake_source(token_hex: &str, identity: Identity, chunked: bool) -> (String, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let token = decode_token(token_hex).unwrap();
    let other = decode_token(&"ee".repeat(32)).unwrap();
    let addr_for_proof = addr.clone();
    let seen_thread = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let token = token.clone();
            let other = other.clone();
            let addr = addr_for_proof.clone();
            let seen = seen_thread.clone();
            std::thread::spawn(move || {
                while let Some(head) = read_request(&mut stream) {
                    let line = head.lines().next().unwrap_or("").to_owned();
                    let authorized = head.lines().any(|l| l.to_ascii_lowercase().starts_with("authorization:"));
                    {
                        let mut s = seen.lock().unwrap();
                        s.requests.push(line.clone());
                        s.saw_token |= authorized;
                    }
                    let path = line.split(' ').nth(1).unwrap_or("");
                    if let Some(challenge) = path.strip_prefix("/chappa/v1/identity?challenge=") {
                        match identity {
                            Identity::Honest => reply(&mut stream, 200, &json!({"proof": source_proof(&token, challenge, &addr)}).to_string()),
                            Identity::Impostor => reply(&mut stream, 200, &json!({"proof": source_proof(&other, challenge, &addr)}).to_string()),
                            Identity::NotASource => reply(&mut stream, 404, "{}"),
                        }
                        continue;
                    }
                    if !authorized {
                        reply(&mut stream, 401, r#"{"kind":"unauthorized","message":"A bearer token is required"}"#);
                        continue;
                    }
                    if path == "/chappa/v1/manifest" {
                        let body = json!({
                            "contract": 1,
                            "source": {"id": "fake", "label": "Fake", "version": "0"},
                            "views": {"work": {"list": "/w", "pollSeconds": 15}},
                            "linkHosts": ["example.test"]
                        })
                        .to_string();
                        if chunked {
                            let mut out = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
                            for piece in body.as_bytes().chunks(13) {
                                out.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
                                out.extend_from_slice(piece);
                                out.extend_from_slice(b"\r\n");
                            }
                            out.extend_from_slice(b"0\r\n\r\n");
                            let _ = stream.write_all(&out);
                        } else {
                            reply(&mut stream, 200, &body);
                        }
                        continue;
                    }
                    if path == "/w" {
                        reply(&mut stream, 200, r#"{"freshness":{"asOf":null},"items":[{"key":"ABC-1","title":"t","status":null}]}"#);
                        continue;
                    }
                    reply(&mut stream, 404, r#"{"kind":"not-found","message":"no"}"#);
                }
            });
        }
    });
    (addr, seen)
}

#[test]
fn an_honest_source_passes_the_handshake_and_then_gets_the_token() {
    let token = new_token_hex().unwrap();
    let (addr, seen) = fake_source(&token, Identity::Honest, false);
    let mut client = SourceClient::new(&addr, &token).unwrap();
    client.verify().expect("the proof verifies");
    assert!(!seen.lock().unwrap().saw_token, "nothing token-bearing before the proof");
    let manifest = client.manifest().unwrap();
    assert_eq!(manifest["source"]["id"], "fake");
    let seen = seen.lock().unwrap();
    assert!(seen.saw_token);
    assert!(seen.requests[0].starts_with("GET /chappa/v1/identity?challenge="), "{:?}", seen.requests);
    assert_eq!(seen.requests[1], "GET /chappa/v1/manifest HTTP/1.1");
    assert_eq!(seen.requests.len(), 2, "one keep-alive connection, no reconnect");
}

#[test]
fn an_impostor_on_the_port_never_sees_the_token() {
    let token = new_token_hex().unwrap();
    for identity in [Identity::Impostor, Identity::NotASource] {
        let (addr, seen) = fake_source(&token, identity, false);
        let mut client = SourceClient::new(&addr, &token).unwrap();
        let err = client.verify().unwrap_err();
        assert!(matches!(err, SourceError::Untrusted(_)), "{err}");
        // A fetch after a failed handshake tries the handshake again and
        // fails the same way; still no token.
        let err = client.manifest().unwrap_err();
        assert!(matches!(err, SourceError::Untrusted(_)), "{err}");
        let seen = seen.lock().unwrap();
        assert!(!seen.saw_token, "the token went out to an impostor: {:?}", seen.requests);
        assert!(seen.requests.iter().all(|r| r.contains("/chappa/v1/identity?challenge=")), "{:?}", seen.requests);
    }
}

#[test]
fn a_chunked_manifest_is_decoded() {
    let token = new_token_hex().unwrap();
    let (addr, _) = fake_source(&token, Identity::Honest, true);
    let mut client = SourceClient::new(&addr, &token).unwrap();
    let value = client.manifest().unwrap();
    let manifest = parse_manifest(&value).unwrap();
    assert_eq!(manifest.id, "fake");
    assert_eq!(manifest.views["work"].list.path(), "/w");
    // And the keep-alive connection is still good for the view.
    let work = client.get(&manifest.views["work"].list).unwrap();
    assert_eq!(work["items"][0]["key"], "ABC-1");
}

#[test]
fn the_client_refuses_non_loopback_addresses_and_bad_tokens_before_connecting() {
    let token = new_token_hex().unwrap();
    assert!(SourceClient::new("10.0.0.5:5000", &token).err().unwrap().contains("loopback"));
    assert!(SourceClient::new("localhost:5000", &token).is_err());
    assert!(SourceClient::new("127.0.0.1:5000", "nope").err().unwrap().contains("hex"));
    // Nothing listening is Unreachable, not Untrusted: the source may still
    // be starting.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut client = SourceClient::new(&format!("127.0.0.1:{port}"), &token).unwrap();
    assert!(matches!(client.verify().unwrap_err(), SourceError::Unreachable(_)));
}

#[test]
fn only_paths_the_manifest_named_can_be_requested() {
    // `Endpoint` has no public constructor: the manifest is the only way to
    // name a path, and a manifest naming a foreign host is refused whole.
    let value = json!({
        "contract": 1, "source": {"id": "x", "label": "X", "version": "1"},
        "views": {"work": {"list": "http://10.0.0.5/steal"}}
    });
    assert!(parse_manifest(&value).is_err());
    assert_eq!(Endpoint::manifest().path(), "/chappa/v1/manifest");
}

// ---- the real fixture, end to end ----------------------------------------------------

struct Collector(Mutex<Vec<Value>>);

impl SourcesEmitter for Collector {
    fn emit(&self, payload: Value) {
        self.0.lock().unwrap().push(payload);
    }
}

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../sources/fixture/index.mjs")
}

fn node_available() -> bool {
    Command::new("node").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().map_or(false, |s| s.success())
}

fn spawn_fixture(addr: &str, token: &str) -> Child {
    Command::new("node")
        .arg(fixture_script())
        .env(ADDR_ENV, addr)
        .env(TOKEN_ENV, token)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the fixture source")
}

/// A child that is killed when the test ends, pass or fail.
struct Reaper(Option<Child>);

impl Drop for Reaper {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_for<T>(what: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_fixture_source_is_launched_proved_trusted_polled_and_survives_a_restart() {
    if !node_available() {
        eprintln!("skipping: `node` is not on PATH, the fixture source cannot run");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let events = Arc::new(Collector(Mutex::new(Vec::new())));
    let sources = Sources::default();
    sources.init_with(SourceStore::new(dir.path().join("sources.json")), events.clone());

    // Launch: a fresh port and token, handed to the process in its env.
    let launch = sources.prepare_launch(7, &project, "fixture-source").unwrap();
    assert!(launch.addr.starts_with("127.0.0.1:"));
    assert_eq!(launch.token.len(), 64);
    let mut child = Reaper(Some(spawn_fixture(&launch.addr, &launch.token)));
    sources.launched(&project, "fixture-source", 1);

    let source_id = {
        let listing = sources.listing();
        assert_eq!(listing.sources.len(), 1);
        assert_eq!(listing.sources[0].status, SourceStatus::Starting);
        listing.sources[0].id.clone()
    };

    // Identity + manifest → the trust gate, and nothing polled meanwhile.
    let trust = wait_for("needs review", Duration::from_secs(20), || {
        let s = &sources.listing().sources[0];
        (s.status == SourceStatus::NeedsReview).then(|| s.trust.clone().unwrap())
    });
    assert_eq!(trust.id, "fixture");
    assert_eq!(trust.link_hosts, ["dev.example.test", "registry.example"]);
    assert_eq!(trust.roots, ["/srv/example"]);
    assert_eq!(trust.views, ["attention", "environment", "prs", "work"]);
    std::thread::sleep(Duration::from_millis(300));
    let s = &sources.listing().sources[0];
    assert!(s.views.iter().all(|v| !v.has_payload), "an untrusted source is polled for nothing");
    assert_eq!(s.label, "Fixture");
    assert_eq!(s.views.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["attention", "environment", "prs", "work", "workflows"]);

    // A stale hash is refused; the shown hash is accepted.
    assert!(sources.trust_answer(&source_id, true, "0".repeat(64).as_str()).is_err());
    sources.trust_answer(&source_id, false, &trust.hash).unwrap();
    assert_eq!(sources.listing().sources[0].status, SourceStatus::NeedsReview);
    sources.trust_answer(&source_id, true, &trust.hash).unwrap();

    // Every view (and the workflows list) is fetched and cached raw.
    wait_for("every view polled", Duration::from_secs(20), || {
        let s = &sources.listing().sources[0];
        (s.status == SourceStatus::Up && s.views.iter().all(|v| v.has_payload)).then_some(())
    });
    let work = sources.view(&source_id, "work").unwrap();
    assert_eq!(work.payload.as_ref().unwrap()["items"][0]["key"], "ABC-1");
    assert!(work.freshness.as_of.is_some());
    assert!(work.freshness.errors.is_empty());
    let workflows = sources.view(&source_id, "workflows").unwrap();
    assert!(workflows.payload.unwrap().is_object());
    assert!(sources.view(&source_id, "panel").is_err(), "an endpoint the manifest did not list as a view is not cached");
    assert!(events.0.lock().unwrap().iter().any(|e| e["event"] == "view" && e["view"] == "work"));

    // Crash: the process dies; the source is down, the data stays.
    child.0.as_mut().unwrap().kill().unwrap();
    let _ = child.0.as_mut().unwrap().wait();
    sources.process_gone(&project, "fixture-source", "exited with code 1");
    let s = &sources.listing().sources[0];
    assert_eq!(s.status, SourceStatus::Down);
    assert!(s.last_error.as_deref().unwrap().contains("exited with code 1"));
    assert!(s.views.iter().all(|v| v.has_payload), "payloads survive the crash");
    assert_eq!(sources.view(&source_id, "work").unwrap().payload.unwrap()["items"][0]["key"], "ABC-1");

    // Restart: a NEW port and token, identity proved again, trust already on
    // record for the same manifest, so polling resumes without a prompt.
    let relaunch = sources.prepare_launch(7, &project, "fixture-source").unwrap();
    assert_ne!(relaunch.addr, launch.addr);
    assert_ne!(relaunch.token, launch.token);
    let _child = Reaper(Some(spawn_fixture(&relaunch.addr, &relaunch.token)));
    sources.launched(&project, "fixture-source", 2);
    assert_eq!(sources.listing().sources[0].id, source_id, "the same source, not a second entry");
    wait_for("up again after the restart", Duration::from_secs(20), || {
        let s = &sources.listing().sources[0];
        (s.status == SourceStatus::Up && s.trusted).then_some(())
    });
    assert_eq!(sources.listing().sources.len(), 1);

    // The trust record persisted next to the attached list.
    let store = SourceStore::new(dir.path().join("sources.json"));
    assert!(store.is_trusted(&SourceStore::launched_key(&project, "fixture-source"), &trust.hash));
    sources.shutdown();
}

#[test]
fn a_launched_source_that_never_answers_is_down_with_a_reason_and_its_restart_gets_a_new_port() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("p");
    let sources = Sources::default();
    sources.init_with(SourceStore::new(dir.path().join("sources.json")), Arc::new(Collector(Mutex::new(Vec::new()))));
    let launch = sources.prepare_launch(1, &project, "dead").unwrap();
    sources.launched(&project, "dead", 9);
    // Nothing binds the port; the process row then dies.
    sources.process_gone(&project, "dead", "exited with code 2");
    let s = &sources.listing().sources[0];
    assert_eq!(s.status, SourceStatus::Down);
    assert_eq!(s.label, "dead", "no manifest yet: the command name stands in");
    assert!(s.trust.is_none());
    assert!(matches!(s.origin, app_lib::sources::OriginDto::Launched { project_id: 1, .. }));
    let again = sources.prepare_launch(1, &project, "dead").unwrap();
    assert_ne!(again.token, launch.token);
    assert_eq!(sources.listing().sources.len(), 1);
    sources.shutdown();
}

#[test]
fn attaching_needs_a_loopback_address_and_a_real_token_file() {
    let dir = tempfile::tempdir().unwrap();
    let sources = Sources::default();
    sources.init_with(SourceStore::new(dir.path().join("sources.json")), Arc::new(Collector(Mutex::new(Vec::new()))));
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, new_token_hex().unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert!(sources.attach("10.0.0.5:7000", &token_file).unwrap_err().contains("loopback"));
    assert!(sources.attach("127.0.0.1:7000", &dir.path().join("missing")).is_err());
    // A loopback address nothing listens on attaches fine and reads "down"
    // with the reason; the worker keeps retrying until detached.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    sources.attach(&format!("127.0.0.1:{port}"), &token_file).unwrap();
    let id = wait_for("down", Duration::from_secs(10), || {
        let listing = sources.listing();
        let s = listing.sources.first()?;
        (s.status == SourceStatus::Down).then(|| s.id.clone())
    });
    let s = &sources.listing().sources[0];
    assert!(s.last_error.as_deref().unwrap().contains("not reachable"));
    assert!(matches!(s.origin, app_lib::sources::OriginDto::Attached { .. }));
    assert_eq!(SourceStore::new(dir.path().join("sources.json")).attached().len(), 1, "persisted");
    sources.detach(&id).unwrap();
    assert!(sources.listing().sources.is_empty());
    assert!(SourceStore::new(dir.path().join("sources.json")).attached().is_empty());
    sources.shutdown();
}

#[test]
fn hidden_views_are_remembered_and_only_real_views_qualify() {
    let dir = tempfile::tempdir().unwrap();
    let sources = Sources::default();
    sources.init_with(SourceStore::new(dir.path().join("sources.json")), Arc::new(Collector(Mutex::new(Vec::new()))));
    sources.set_view_hidden("prs", true).unwrap();
    assert!(sources.set_view_hidden("workflows", true).is_err());
    assert_eq!(sources.listing().hidden_views, ["prs"]);
    sources.set_view_hidden("prs", false).unwrap();
    assert!(sources.listing().hidden_views.is_empty());
}
