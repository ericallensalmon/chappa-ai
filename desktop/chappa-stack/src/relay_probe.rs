//! "Test from a container": does a container on the relayed engine reach
//! the relay?
//!
//! Only on request (a confirm-gated action, never automatic): one throwaway
//! container (`<engine> run --rm --pull never <image> curl.exe …`) connects
//! to `<gateway>:<port>` of one listening relay port, and the outcome is
//! kept as a fact of the Relay section. The image is one the engine already
//! has: a Windows base image (`nanoserver` or `servercore`, which carry
//! `curl.exe`), found by listing the engine's images (read-only, at most
//! every [`IMAGES_TTL`], off the request path). `--pull never` makes sure
//! nothing is downloaded behind the user's back; with no such image the
//! fact says which to pull, and no action is offered. The argv is built
//! from validated parts and run without a shell.
//!
//! `curl.exe` exits 7 when the connection was refused or unreachable and 28
//! when it timed out: the relay was not reached. Any other answer (a reply,
//! or a connection closed without one, as when nothing listens behind the
//! relay) means the container got through to it.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use stack_core::cli::{engine_invocation, CliError, CliOutput, EngineCli, Invocation};
use stack_core::definition::EngineRef;

pub const TEST_ACTION_ID: &str = "relay-test";
/// How long a listing of the engine's images is reused.
pub const IMAGES_TTL: Duration = Duration::from_secs(60);
/// The whole test's time limit (container start included).
pub const TEST_TIMEOUT_SECS: u64 = 120;
/// How long the container waits for the connection.
pub const CONNECT_SECS: u64 = 5;

/// The image the test runs, from `<engine> image ls --format
/// {{.Repository}}:{{.Tag}}`: a `nanoserver` image first, else a
/// `servercore` one; untagged and oddly named entries are skipped.
pub fn pick_image(listing: &str) -> Option<String> {
    let images: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("<none>") && is_safe_image(l))
        .collect();
    for kind in ["nanoserver", "servercore"] {
        if let Some(i) = images.iter().find(|i| {
            let repo = i.rsplit_once(':').map_or(**i, |(r, _)| r);
            repo.rsplit('/').next() == Some(kind)
        }) {
            return Some(i.to_string());
        }
    }
    None
}

/// An image reference made of the reference alphabet only, never starting
/// with `-` (it goes into argv, where a leading dash would read as a flag).
pub fn is_safe_image(image: &str) -> bool {
    !image.is_empty()
        && image.len() <= 256
        && !image.starts_with('-')
        && image
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/:@".contains(&b))
}

/// The test's argv.
pub fn test_invocation(engine: &EngineRef, image: &str, target: SocketAddr) -> Invocation {
    let connect = CONNECT_SECS.to_string();
    let max = (CONNECT_SECS + 3).to_string();
    let url = format!("http://{target}/");
    let mut inv = engine_invocation(
        engine,
        &[
            "run",
            "--rm",
            "--pull",
            "never",
            image,
            "curl.exe",
            "-s",
            "-o",
            "NUL",
            "--connect-timeout",
            &connect,
            "-m",
            &max,
            &url,
        ],
    );
    inv.timeout_secs = TEST_TIMEOUT_SECS;
    inv
}

fn argv_of(inv: &Invocation) -> Vec<String> {
    std::iter::once(inv.program.clone())
        .chain(inv.args.iter().cloned())
        .collect()
}

/// What one run found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The container reached the relay; what came back.
    Reached(String),
    /// It did not; why.
    NotReached(String),
    /// The container did not run; why.
    NotRun(String),
}

fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// Judge the run's outcome by `curl.exe`'s exit code.
pub fn judge(result: Result<CliOutput, CliError>) -> Verdict {
    match result {
        Ok(out) => match out.status {
            Some(0) => Verdict::Reached("the service behind it answered".into()),
            Some(7) => {
                Verdict::NotReached("the connection was refused or could not be made".into())
            }
            Some(28) => Verdict::NotReached("the connection timed out".into()),
            Some(c) if c >= 125 => Verdict::NotRun(format!(
                "the engine could not run the container (exit {c}): {}",
                first_line(&out.stderr)
            )),
            Some(c) => Verdict::Reached(format!(
                "the connection was made, then closed without an answer (curl exit {c})"
            )),
            None => Verdict::NotRun("the container was stopped".into()),
        },
        Err(e) => Verdict::NotRun(e.to_string()),
    }
}

/// The last run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRun {
    pub target: SocketAddr,
    pub image: String,
    pub verdict: Verdict,
}

impl TestRun {
    pub fn summary(&self) -> String {
        match &self.verdict {
            Verdict::Reached(how) => format!("passed: a container reached {} ({how})", self.target),
            Verdict::NotReached(why) => format!(
                "failed: a container could not reach {} ({why})",
                self.target
            ),
            Verdict::NotRun(why) => format!("did not run: {why}"),
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// The image picked from the last listing (`Err`: the listing failed).
    image: Option<Result<Option<String>, String>>,
    listed: Option<Instant>,
    listing: bool,
    running: bool,
    last: Option<TestRun>,
}

/// The test of one relay.
pub struct ContainerTest {
    cli: Arc<dyn EngineCli + Send + Sync>,
    inner: Arc<Mutex<Inner>>,
}

impl ContainerTest {
    pub fn new(cli: Arc<dyn EngineCli + Send + Sync>) -> ContainerTest {
        ContainerTest {
            cli,
            inner: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// List `engine`'s images again when the last listing is older than
    /// [`IMAGES_TTL`] (on a thread; the view shows the result next time).
    pub fn refresh(&self, engine: &EngineRef) {
        let mut inner = self.lock();
        if inner.listing || inner.listed.is_some_and(|t| t.elapsed() < IMAGES_TTL) {
            return;
        }
        inner.listing = true;
        inner.listed = Some(Instant::now());
        drop(inner);
        let cli = self.cli.clone();
        let state = self.inner.clone();
        let mut inv = engine_invocation(
            engine,
            &["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"],
        );
        inv.timeout_secs = 30;
        thread::spawn(move || {
            let picked = match cli.run(&inv) {
                Ok(out) if out.ok() => Ok(pick_image(&out.stdout)),
                Ok(out) => Err(first_line(&out.stderr).to_string()),
                Err(e) => Err(e.to_string()),
            };
            let mut inner = state.lock().unwrap_or_else(|p| p.into_inner());
            inner.image = Some(picked);
            inner.listing = false;
        });
    }

    fn command(&self, engine: &EngineRef, target: SocketAddr) -> Option<Invocation> {
        match &self.lock().image {
            Some(Ok(Some(image))) => Some(test_invocation(engine, image, target)),
            _ => None,
        }
    }

    fn fingerprint(inv: &Invocation) -> String {
        let mut h = Sha256::new();
        for a in argv_of(inv) {
            h.update(a.as_bytes());
            h.update([0u8]);
        }
        h.finalize()
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The action, when an image is there to run.
    pub fn action(&self, engine: &EngineRef, target: SocketAddr) -> Option<Value> {
        let inv = self.command(engine, target)?;
        let mut a = json!({
            "kind": "endpoint",
            "id": TEST_ACTION_ID,
            "label": "Test from a container",
            "mutates": true,
            "confirm": format!(
                "Runs a throwaway container on {} that connects to the relay at {target} and is removed afterwards. The image is one the engine already has; nothing is pulled. It runs:\n\n{}",
                engine.name,
                argv_of(&inv).join(" ")
            ),
            "params": { "fingerprint": Self::fingerprint(&inv) },
        });
        if self.lock().running {
            a["disabled"] = "The test is running".into();
        }
        Some(a)
    }

    /// The facts row.
    pub fn row(&self) -> Value {
        let inner = self.lock();
        let value = if inner.running {
            "running…".to_string()
        } else if let Some(run) = &inner.last {
            run.summary()
        } else {
            match &inner.image {
                None => "looking for an image to run…".to_string(),
                Some(Err(e)) => format!("cannot list the engine's images: {e}"),
                Some(Ok(None)) => "no Windows base image (nanoserver or servercore) is on the engine; pull one to test, nothing is pulled for you".to_string(),
                Some(Ok(Some(i))) => format!("not run yet (would use {i})"),
            }
        };
        let mut row = json!({ "label": "Test from a container", "value": value });
        let tone = match inner.last.as_ref().map(|r| &r.verdict) {
            Some(Verdict::Reached(_)) if !inner.running => Some(("passed", "ok")),
            Some(Verdict::NotReached(_)) if !inner.running => Some(("failed", "bad")),
            _ => None,
        };
        if let Some((text, tone)) = tone {
            row["badges"] = json!([{ "text": text, "tone": tone }]);
        }
        row
    }

    /// Run the test the confirm showed (same `fingerprint`), on a thread.
    pub fn run(
        &self,
        engine: &EngineRef,
        target: SocketAddr,
        fingerprint: Option<&str>,
    ) -> Result<String, (u16, String)> {
        let Some(inv) = self.command(engine, target) else {
            return Err((
                409,
                "there is no image on the engine to run the test with".to_string(),
            ));
        };
        if fingerprint != Some(Self::fingerprint(&inv).as_str()) {
            return Err((
                409,
                format!(
                    "the test changed since it was shown; it would now run: {}",
                    argv_of(&inv).join(" ")
                ),
            ));
        }
        {
            let mut inner = self.lock();
            if inner.running {
                return Err((409, "the test is already running".to_string()));
            }
            inner.running = true;
        }
        let cli = self.cli.clone();
        let state = self.inner.clone();
        let image = inv.args.get(4).cloned().unwrap_or_default();
        thread::spawn(move || {
            let verdict = judge(cli.run(&inv));
            let mut inner = state.lock().unwrap_or_else(|p| p.into_inner());
            inner.running = false;
            inner.last = Some(TestRun {
                target,
                image,
                verdict,
            });
        });
        Ok(format!(
            "Testing from a container: it connects to {target}; the Relay section shows the outcome."
        ))
    }
}

/// Put the test's row and action into the facts section `relay`.
pub fn decorate(v: &mut Value, test: &ContainerTest, engine: &EngineRef, target: SocketAddr) {
    test.refresh(engine);
    let row = test.row();
    let action = test.action(engine, target);
    let Some(section) = v
        .get_mut("facts")
        .and_then(Value::as_array_mut)
        .and_then(|f| f.iter_mut().find(|s| s["id"] == "relay"))
    else {
        return;
    };
    if let Some(rows) = section.get_mut("rows").and_then(Value::as_array_mut) {
        rows.push(row);
    }
    if let Some(a) = action {
        match section.get_mut("actions").and_then(Value::as_array_mut) {
            Some(list) => list.push(a),
            None => section["actions"] = json!([a]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stack_core::driver::EngineKind;

    fn docker() -> EngineRef {
        EngineRef {
            name: "docker".into(),
            kind: EngineKind::Docker,
            context: None,
        }
    }

    fn out(status: i32, stderr: &str) -> Result<CliOutput, CliError> {
        Ok(CliOutput {
            status: Some(status),
            stdout: String::new(),
            stderr: stderr.into(),
        })
    }

    #[test]
    fn the_image_is_a_windows_base_image_already_on_the_engine() {
        let listing = "postgres:16\n<none>:<none>\nmcr.microsoft.com/windows/servercore:ltsc2022\nmcr.microsoft.com/windows/nanoserver:ltsc2022\n";
        assert_eq!(
            pick_image(listing).as_deref(),
            Some("mcr.microsoft.com/windows/nanoserver:ltsc2022")
        );
        assert_eq!(
            pick_image("mcr.microsoft.com/windows/servercore:ltsc2022").as_deref(),
            Some("mcr.microsoft.com/windows/servercore:ltsc2022")
        );
        assert_eq!(pick_image("example/not-nanoserver:1\npostgres:16"), None);
        assert_eq!(pick_image("-nanoserver:1"), None);
        assert_eq!(pick_image(""), None);
        assert!(!is_safe_image("a b") && !is_safe_image("a;b") && !is_safe_image("--rm"));
    }

    #[test]
    fn the_test_never_pulls_and_names_the_relay_address() {
        let target: SocketAddr = "172.29.80.1:14001".parse().unwrap();
        let inv = test_invocation(
            &docker(),
            "mcr.microsoft.com/windows/nanoserver:ltsc2022",
            target,
        );
        assert_eq!(inv.program, "docker");
        assert_eq!(
            argv_of(&inv).join(" "),
            "docker run --rm --pull never mcr.microsoft.com/windows/nanoserver:ltsc2022 curl.exe -s -o NUL --connect-timeout 5 -m 8 http://172.29.80.1:14001/"
        );
        let ctx = EngineRef {
            context: Some("remote".into()),
            ..docker()
        };
        assert_eq!(
            test_invocation(&ctx, "x", target).args[..2],
            ["--context", "remote"].map(String::from)
        );
    }

    #[test]
    fn curl_exit_codes_tell_reached_from_not_reached() {
        assert!(matches!(judge(out(0, "")), Verdict::Reached(_)));
        assert!(
            matches!(judge(out(52, "")), Verdict::Reached(ref w) if w.contains("closed without an answer"))
        );
        assert!(matches!(judge(out(7, "")), Verdict::NotReached(ref w) if w.contains("refused")));
        assert!(
            matches!(judge(out(28, "")), Verdict::NotReached(ref w) if w.contains("timed out"))
        );
        assert!(
            matches!(judge(out(125, "Unable to find image")), Verdict::NotRun(ref w) if w.contains("Unable to find image"))
        );
        let spawn = Err(CliError::Spawn {
            program: "docker".into(),
            message: "not found".into(),
        });
        assert!(matches!(judge(spawn), Verdict::NotRun(_)));
    }

    /// Lists `listing` as the images and answers a test run with `status`.
    struct Engine {
        listing: String,
        status: i32,
        calls: Mutex<Vec<Invocation>>,
    }

    impl EngineCli for Engine {
        fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
            self.calls.lock().unwrap().push(inv.clone());
            let stdout = if inv.args.first().map(String::as_str) == Some("image") {
                self.listing.clone()
            } else {
                String::new()
            };
            let status = if inv.args.first().map(String::as_str) == Some("run") {
                self.status
            } else {
                0
            };
            Ok(CliOutput {
                status: Some(status),
                stdout,
                stderr: String::new(),
            })
        }
    }

    fn wait(test: &ContainerTest, done: impl Fn(&Inner) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&test.lock()) {
            assert!(Instant::now() < deadline, "never finished");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_test_runs_only_as_confirmed_and_reports_as_a_fact() {
        let target: SocketAddr = "172.29.80.1:14001".parse().unwrap();
        let none = Arc::new(Engine {
            listing: "postgres:16\n".into(),
            status: 0,
            calls: Mutex::default(),
        });
        let t = ContainerTest::new(none.clone());
        t.refresh(&docker());
        wait(&t, |i| i.image.is_some());
        assert!(t.action(&docker(), target).is_none(), "no image, no action");
        assert!(t.row()["value"]
            .as_str()
            .unwrap()
            .contains("pull one to test"));
        assert!(t.run(&docker(), target, None).is_err());

        let engine = Arc::new(Engine {
            listing: "mcr.microsoft.com/windows/nanoserver:ltsc2022\n".into(),
            status: 7,
            calls: Mutex::default(),
        });
        let t = ContainerTest::new(engine.clone());
        t.refresh(&docker());
        t.refresh(&docker());
        wait(&t, |i| i.image.is_some());
        assert_eq!(engine.calls.lock().unwrap().len(), 1, "listed once per TTL");
        let a = t.action(&docker(), target).unwrap();
        assert_eq!(a["mutates"], true);
        assert!(a["confirm"].as_str().unwrap().contains("--pull never"));
        let fp = a["params"]["fingerprint"].as_str().unwrap().to_string();
        assert!(t.run(&docker(), target, Some("stale")).is_err());
        assert_eq!(engine.calls.lock().unwrap().len(), 1, "nothing ran");
        t.run(&docker(), target, Some(&fp)).unwrap();
        wait(&t, |i| !i.running && i.last.is_some());
        let row = t.row();
        assert!(row["value"]
            .as_str()
            .unwrap()
            .starts_with("failed: a container could not reach 172.29.80.1:14001"));
        assert_eq!(row["badges"][0]["tone"], "bad");

        let mut payload = json!({ "facts": [{ "id": "relay", "rows": [] }] });
        decorate(&mut payload, &t, &docker(), target);
        assert_eq!(
            payload["facts"][0]["rows"][0]["label"],
            "Test from a container"
        );
        assert_eq!(payload["facts"][0]["actions"][0]["id"], TEST_ACTION_ID);
    }
}
