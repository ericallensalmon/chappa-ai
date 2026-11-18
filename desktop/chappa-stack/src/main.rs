//! `chappa-stack serve | relay | run-job <id> | follow <file> | follow-command`; see the library
//! docs for what each does.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use chappa_stack::hidden_output::LineHider;
use chappa_stack::identity::{Token, ADDR_ENV, TOKEN_ENV};
use chappa_stack::project::ProjectConfig;
use chappa_stack::server::{self, DefinitionPin, Source};
use chappa_stack::world::{
    World, GITLEAKS_CONFIG_ARG, PROJECT_GITLEAKS_CONFIG_ARG, PROJECT_GITLEAKS_CONFIG_HASH_ARG,
};
use stack_core::safety::secrets::config::ProjectScanConfig;

const USAGE: &str = "usage:
  chappa-stack serve [--project <dir>] [--stack <name>] [--slot <n>] [--channel <name>]
                     [--trusted-definition <hash>|none] [--gitleaks-config <file>]
                     [--project-gitleaks-config <file> --project-gitleaks-config-hash <hash>|none]
                     [--addr <loopback:port> --token-file <file>]
  chappa-stack relay --engine <docker|podman>[@<context>]… --ports <ports>…
                     [--addr <loopback:port> --token-file <file>]
  chappa-stack run-job <job id> [--project <dir>] [--gitleaks-config <file>]
                     [--project-gitleaks-config <file> --project-gitleaks-config-hash <hash>|none]
  chappa-stack follow <file> [--hide-generated <work dir>]
  chappa-stack follow-command --hide-generated <work dir> -- <command…>

serve: launched by the app as a `source:` command, it reads CHAPPA_SOURCE_ADDR
and CHAPPA_SOURCE_TOKEN. With --addr it runs on its own (attached mode): the
token is read from --token-file, which is created with a fresh token when it
does not exist. The project is the working folder unless --project names one.

--trusted-definition is the stack definition hash the person trusted in the
app (`none` when nothing was): scripts, hooks, scans and jobs run only while
the definition still has it. Launched by the app without it, nothing runs;
attached without it, the definition as read at startup is the trusted one.

--gitleaks-config is the gitleaks config the app's Settings name: every
secret scan (worktrees, commits, build contexts, env files) uses it, and
every job is started with it. Without it the built-in default is used, the
same one the app uses. A config file inside a scanned folder is never read,
except the project's own:

--project-gitleaks-config is the config the project names in chappa.yml
(`secretScan.config`), with the content hash the person trusted in the app
(`none` while it is not trusted). Before every scan the file is read and
hashed again; it is used, ahead of the Settings config, only while the hash
matches and it keeps gitleaks' defaults. Otherwise the scans fall back to the
Settings config or the default, and the Environment view asks to review it.

relay: the relay alone, as a source (launched or attached like serve), for a
stack that no project defines. It listens on each engine's gateway address
on every port of --ports (single ports and ranges, comma separated, e.g.
14000,14010-14020; both flags repeat) and forwards to the same port on
loopback. Its Environment view is the Relay section, with the Windows
Firewall check and its actions.

follow and follow-command show a component's output for its `logs` terminal,
with every value generated for an instance (`{secret:<name>}`) as ••••.";

/// What the app put in this process's environment, taken out of it before
/// anything else runs, so no child of this process can inherit the token.
struct LaunchEnv {
    addr: Option<String>,
    token: Option<String>,
}

struct Args {
    rest: Vec<String>,
}

impl Args {
    fn take(&mut self, flag: &str) -> Result<Option<String>, String> {
        match self.rest.iter().position(|a| a == flag) {
            None => Ok(None),
            Some(i) if i + 1 < self.rest.len() => {
                let v = self.rest.remove(i + 1);
                self.rest.remove(i);
                Ok(Some(v))
            }
            Some(_) => Err(format!("{flag} needs a value")),
        }
    }

    fn done(&self) -> Result<(), String> {
        match self.rest.first() {
            None => Ok(()),
            Some(a) => Err(format!("unexpected argument `{a}`")),
        }
    }
}

fn main() -> ExitCode {
    // First, while this is the only thread: read the source variables and
    // remove them (and gitleaks' config overrides) from the environment
    // every child inherits.
    let launch = LaunchEnv {
        addr: stack_core::child_env::take_from_process_env(ADDR_ENV),
        token: stack_core::child_env::take_from_process_env(TOKEN_ENV),
    };
    stack_core::child_env::scrub_process_env();
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let verb = argv.remove(0);
    let mut args = Args { rest: argv };
    let result = match verb.as_str() {
        "serve" => serve(&mut args, launch),
        "relay" => relay(&mut args, launch),
        "run-job" => run_job(&mut args),
        "follow" => follow(&mut args),
        "follow-command" => follow_command(&mut args),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("chappa-stack: {e}");
            ExitCode::from(2)
        }
    }
}

fn project_dir(args: &mut Args) -> Result<PathBuf, String> {
    match args.take("--project")? {
        Some(d) => Ok(PathBuf::from(d)),
        None => std::env::current_dir().map_err(|e| format!("no working folder: {e}")),
    }
}

/// `--gitleaks-config <file>`: an empty value is no config.
fn gitleaks_config(args: &mut Args) -> Result<Option<PathBuf>, String> {
    Ok(args
        .take(GITLEAKS_CONFIG_ARG)?
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .map(PathBuf::from))
}

/// `--project-gitleaks-config <file> --project-gitleaks-config-hash
/// <hash|none>`: the project's own config and the hash it was trusted at.
fn project_scan_config(args: &mut Args) -> Result<ProjectScanConfig, String> {
    let path = args
        .take(PROJECT_GITLEAKS_CONFIG_ARG)?
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .map(PathBuf::from);
    let hash = args.take(PROJECT_GITLEAKS_CONFIG_HASH_ARG)?;
    ProjectScanConfig::from_args(path, hash.as_deref())
}

fn serve(args: &mut Args, launch: LaunchEnv) -> Result<u8, String> {
    let mut config = ProjectConfig::new(project_dir(args)?);
    config.stack = args.take("--stack")?;
    if let Some(s) = args.take("--slot")? {
        config.slot = s
            .parse()
            .map_err(|_| format!("--slot `{s}` is not a number"))?;
    }
    if let Some(c) = args.take("--channel")? {
        config.channel = c;
    }
    let addr_flag = args.take("--addr")?;
    let token_file = args.take("--token-file")?;
    let pin_flag = args
        .take("--trusted-definition")?
        .map(|t| DefinitionPin::parse(&t))
        .transpose()?;
    let gitleaks_config = gitleaks_config(args)?;
    let project_scan = project_scan_config(args)?;
    args.done()?;
    let (addr, token, attached) = address_and_token(addr_flag, token_file, launch)?;
    let pin = match pin_flag {
        Some(p) => p,
        // Launched by the app, which always says what was trusted: without
        // it nothing was.
        None if !attached => DefinitionPin::Nothing,
        // Started by hand: what the person started it on.
        None => match stack_core::trust::definition_trust(&config.dir) {
            Ok(Some(t)) => DefinitionPin::Trusted(t.hash),
            _ => DefinitionPin::Nothing,
        },
    };
    sweep_scratch(&config.dir);
    let (http, bound) = server::bind(&addr)?;
    // Launched: the proof covers the address string the app chose. Attached:
    // the address actually bound (a `:0` request becomes a real port).
    let proof_addr = if attached { bound.to_string() } else { addr };
    let world = World::real()
        .with_gitleaks_config(gitleaks_config)
        .with_project_scan_config(project_scan);
    let source = Source::new(world.clone(), config, token, proof_addr.clone());
    if pin == DefinitionPin::Nothing {
        eprintln!("chappa-stack: no trusted stack definition; read-only until it is trusted");
    }
    source.set_definition_pin(pin);
    let settled = source.settle_jobs();
    if !settled.is_empty() {
        eprintln!(
            "chappa-stack: {} job(s) whose runner ended before them marked interrupted",
            settled.len()
        );
    }
    let pruned = source.prune_jobs();
    if !pruned.is_empty() {
        eprintln!("chappa-stack: pruned {} old job record(s)", pruned.len());
    }
    let refresher = source.background.start(world);
    eprintln!("chappa-stack: listening on {proof_addr}");
    server::serve(http, source.clone());
    source.shutdown();
    let _ = refresher.join();
    Ok(0)
}

/// Where to listen and the token: from `--addr` and `--token-file`
/// (attached), else from what the app put in the environment (launched).
fn address_and_token(
    addr_flag: Option<String>,
    token_file: Option<String>,
    launch: LaunchEnv,
) -> Result<(String, Token, bool), String> {
    match addr_flag {
        Some(addr) => {
            let file = token_file.ok_or("--addr needs --token-file")?;
            Ok((addr, token_from_file(&PathBuf::from(file))?, true))
        }
        None => {
            let addr = launch
                .addr
                .ok_or_else(|| format!("{ADDR_ENV} is not set (or pass --addr)"))?;
            let token = Token::parse(&launch.token.unwrap_or_default())
                .map_err(|e| format!("{TOKEN_ENV}: {e}"))?;
            Ok((addr, token, false))
        }
    }
}

/// `relay`: the relay alone, as a source.
fn relay(args: &mut Args, launch: LaunchEnv) -> Result<u8, String> {
    use chappa_stack::relay_source::{parse_engine, parse_ports, RelayConfig, RelaySource};
    let mut engines = Vec::new();
    while let Some(e) = args.take("--engine")? {
        engines.push(parse_engine(&e)?);
    }
    let mut specs = Vec::new();
    while let Some(p) = args.take("--ports")? {
        specs.push(p);
    }
    let addr_flag = args.take("--addr")?;
    let token_file = args.take("--token-file")?;
    args.done()?;
    if engines.is_empty() {
        return Err("relay needs --engine (docker or podman)".into());
    }
    if specs.is_empty() {
        return Err("relay needs --ports".into());
    }
    let ports = parse_ports(&specs.join(","))?;
    let (addr, token, attached) = address_and_token(addr_flag, token_file, launch)?;
    let (http, bound) = server::bind(&addr)?;
    let proof_addr = if attached { bound.to_string() } else { addr };
    let cli: std::sync::Arc<dyn stack_core::cli::EngineCli + Send + Sync> =
        std::sync::Arc::new(stack_core::cli::CliRunner);
    let source = RelaySource::new(
        RelayConfig::new(engines, ports),
        token,
        proof_addr.clone(),
        std::sync::Arc::new(chappa_stack::world::CachedGateways::new(
            chappa_stack::world::CliGateways(cli.clone()),
        )),
        chappa_stack::relay_firewall::FirewallCheck::real(),
        chappa_stack::relay_probe::ContainerTest::new(cli),
    );
    let refresher = source.start();
    eprintln!("chappa-stack: relay source listening on {proof_addr}");
    server::serve(http, source.clone());
    source.shutdown();
    let _ = refresher.join();
    Ok(0)
}

/// Remove secret-scan scratch a crashed scan left in the work folder (hard
/// links to scanned files, a scratch env file).
fn sweep_scratch(project: &std::path::Path) {
    let work = std::path::absolute(project)
        .unwrap_or_else(|_| project.to_path_buf())
        .join(stack_core::definition::DEFAULT_WORK_DIR);
    let removed = stack_core::workdir::sweep_stale_scratch(&work, stack_core::workdir::STALE_AFTER);
    if !removed.is_empty() {
        eprintln!(
            "chappa-stack: removed {} stale secret-scan scratch entr{}",
            removed.len(),
            if removed.len() == 1 { "y" } else { "ies" }
        );
    }
}

fn token_from_file(path: &PathBuf) -> Result<Token, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Token::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let token = Token::random()?;
            write_owner_only(path, token.hex())?;
            eprintln!("chappa-stack: wrote a new token to {}", path.display());
            Ok(token)
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn write_owner_only(path: &PathBuf, text: &str) -> Result<(), String> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn run_job(args: &mut Args) -> Result<u8, String> {
    if args.rest.is_empty() {
        return Err("run-job needs a job id".into());
    }
    let id = args.rest.remove(0);
    let dir = project_dir(args)?;
    let gitleaks_config = gitleaks_config(args)?;
    let project_scan = project_scan_config(args)?;
    args.done()?;
    let world = World::real()
        .with_gitleaks_config(gitleaks_config)
        .with_project_scan_config(project_scan);
    let mut stdout = std::io::stdout();
    let code = chappa_stack::runjob::run_job(&world, &dir, &id, &mut stdout);
    Ok(code.clamp(0, 255) as u8)
}

/// Print a file and keep printing what is appended, like `tail -f`, for a
/// process component's `logs` action. Starts over when the file shrinks.
/// With `--hide-generated <work dir>`, the values generated for that work
/// folder's instances are shown as `••••`.
fn follow(args: &mut Args) -> Result<u8, String> {
    if args.rest.is_empty() {
        return Err("follow needs a file".into());
    }
    let path = PathBuf::from(args.rest.remove(0));
    let hide = args
        .take(chappa_stack::actions::HIDE_GENERATED_ARG)?
        .map(PathBuf::from);
    args.done()?;
    let mut hider = hide.as_deref().map(LineHider::new);
    let mut pos: u64 = 0;
    let mut out = std::io::stdout();
    loop {
        let mut read = Vec::new();
        if let Ok(mut f) = std::fs::File::open(&path) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < pos {
                pos = 0;
            }
            if len > pos && f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut buf = Vec::new();
                if f.take(len - pos).read_to_end(&mut buf).is_ok() {
                    pos += buf.len() as u64;
                    read = buf;
                }
            }
        }
        let shown = match (&mut hider, read.is_empty()) {
            (None, _) => read,
            (Some(h), false) => h.push(&read),
            (Some(h), true) => h.idle(),
        };
        if !shown.is_empty() {
            let _ = out.write_all(&shown);
            let _ = out.flush();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `follow-command --hide-generated <work dir> -- <argv…>`: run an engine's
/// `logs -f` with its output shown a line at a time, the values generated
/// for that work folder's instances shown as `••••`.
fn follow_command(args: &mut Args) -> Result<u8, String> {
    let work_dir = args
        .take(chappa_stack::actions::HIDE_GENERATED_ARG)?
        .map(PathBuf::from)
        .ok_or("follow-command needs --hide-generated <work dir>")?;
    match args.rest.first().map(String::as_str) {
        Some("--") => {
            args.rest.remove(0);
        }
        _ => return Err("follow-command needs `--` before the command".into()),
    }
    if args.rest.is_empty() {
        return Err("follow-command needs a command after `--`".into());
    }
    let argv = std::mem::take(&mut args.rest);
    let out: std::sync::Arc<std::sync::Mutex<dyn Write + Send>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::io::stdout()));
    let code = chappa_stack::hidden_output::follow_command(&argv, &work_dir, out);
    Ok(code.clamp(0, 255) as u8)
}
