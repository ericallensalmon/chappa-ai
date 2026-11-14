//! `chappa-stack serve | run-job <id> | follow <file>`; see the library
//! docs for what each does.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use chappa_stack::identity::{Token, ADDR_ENV, TOKEN_ENV};
use chappa_stack::project::ProjectConfig;
use chappa_stack::server::{self, Source};
use chappa_stack::world::World;

const USAGE: &str = "usage:
  chappa-stack serve [--project <dir>] [--stack <name>] [--slot <n>] [--channel <name>]
                     [--addr <loopback:port> --token-file <file>]
  chappa-stack run-job <job id> [--project <dir>]
  chappa-stack follow <file>

serve: launched by the app as a `source:` command, it reads CHAPPA_SOURCE_ADDR
and CHAPPA_SOURCE_TOKEN. With --addr it runs on its own (attached mode): the
token is read from --token-file, which is created with a fresh token when it
does not exist. The project is the working folder unless --project names one.";

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
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let verb = argv.remove(0);
    let mut args = Args { rest: argv };
    let result = match verb.as_str() {
        "serve" => serve(&mut args),
        "run-job" => run_job(&mut args),
        "follow" => follow(&mut args),
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

fn serve(args: &mut Args) -> Result<u8, String> {
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
    args.done()?;
    let (addr, token, attached) = match addr_flag {
        Some(addr) => {
            let file = token_file.ok_or("--addr needs --token-file")?;
            (addr, token_from_file(&PathBuf::from(file))?, true)
        }
        None => {
            let addr = std::env::var(ADDR_ENV)
                .map_err(|_| format!("{ADDR_ENV} is not set (or pass --addr)"))?;
            let token = Token::parse(&std::env::var(TOKEN_ENV).unwrap_or_default())
                .map_err(|e| format!("{TOKEN_ENV}: {e}"))?;
            (addr, token, false)
        }
    };
    let (http, bound) = server::bind(&addr)?;
    // Launched: the proof covers the address string the app chose. Attached:
    // the address actually bound (a `:0` request becomes a real port).
    let proof_addr = if attached { bound.to_string() } else { addr };
    let world = World::real();
    let source = Source::new(world.clone(), config, token, proof_addr.clone());
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
    args.done()?;
    let world = World::real();
    let mut stdout = std::io::stdout();
    let code = chappa_stack::runjob::run_job(&world, &dir, &id, &mut stdout);
    Ok(code.clamp(0, 255) as u8)
}

/// Print a file and keep printing what is appended, like `tail -f`, for a
/// process component's `logs` action. Starts over when the file shrinks.
fn follow(args: &mut Args) -> Result<u8, String> {
    if args.rest.is_empty() {
        return Err("follow needs a file".into());
    }
    let path = PathBuf::from(args.rest.remove(0));
    args.done()?;
    let mut pos: u64 = 0;
    let mut out = std::io::stdout();
    loop {
        if let Ok(mut f) = std::fs::File::open(&path) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < pos {
                pos = 0;
            }
            if len > pos && f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut buf = Vec::new();
                if f.take(len - pos).read_to_end(&mut buf).is_ok() {
                    pos += buf.len() as u64;
                    let _ = out.write_all(&buf);
                    let _ = out.flush();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
