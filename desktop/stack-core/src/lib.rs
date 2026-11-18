//! Core of the stack runner: everything about a per-slot development stack
//! that can be decided without talking to an engine.
//!
//! A *stack* is a set of components (an API, a web app, a database, some
//! cloud infrastructure) that a project brings up together. One stack runs
//! in several *slots* at once, each slot a fully independent *instance*
//! (`<stack>-<slot>`) with its own ports, data and containers, so that two
//! people or two agents never share state. This crate holds the pure parts:
//!
//! | module | owns |
//! |---|---|
//! | [`definition`] | the `stacks:` document: parse, defaults, validation with typed diagnostics |
//! | [`placeholders`] | `{slot}`, `{port:api}`, `{host}` … in definition strings |
//! | [`ports`] | the slot port arithmetic and host-wide collision detection |
//! | [`planner`] | turns a requested change into an ordered, fingerprinted plan of steps |
//! | [`provenance`] | what a component was built from, how stale that is, and image-label mapping |
//! | [`driver`] | the engine-facing traits and the nine component states, with fakes for tests |
//! | [`intent`] | the per-instance desired-state record and the active-item file under `.work/` |
//! | [`instance_secrets`] | `{secret:<name>}`: random values generated per instance and kept under `.work/`, so no password is written into a committed file |
//! | [`safety`] | data-safety and secret-scan types whose findings can never carry a value, the data-safety rule engine ([`safety::data`]) and the secret-scan runner ([`safety::secrets`]) |
//! | [`cli`] | the one [`cli::EngineCli`] trait every engine call goes through (argv, env, cwd, timeout, captured output), the real runner and a recording fake |
//! | [`jobs`] | run blocks and the steps an applied plan is made of |
//! | [`hosts`] | the hosts-file check for the stack hostname and the elevated fix |
//! | [`probe`] | probe rules (`health` / `web` / `any`) over an HTTP trait |
//! | [`images`] | content-keyed source builds, prebuilt digest resolution, catalog/resolve hooks, the provenance cache |
//! | [`compose`] | the compose driver: projects, the per-instance override file, verbs as argv, `ps`/`inspect` parsing, the gateway rule |
//! | [`process`] | the process driver: run blocks with the instance environment |
//! | [`script`] | the script driver: a verb per script, argv resolved with the bound item's worktrees, the status JSON, last outcomes, plan previews for confirms |
//! | [`checks`] | the checks before `up`: engine reachable and OS mode, memory, ports, registry login |
//! | [`runner`] | the verbs end to end: observe, plan, apply into steps, status, gc, activate |
//! | [`workspace`] | item workspaces: folder names from tracker keys, branch patterns, remote identity, the path-length check, the allow file, `item.json` |
//! | [`states`] | saved stack states: the format, listing, pruning without deleting a snapshot another state keeps, the job ids they refer to, the snapshots in a volume, the differences from what runs now |
//! | [`trust`] | the hash over everything a stack definition can run, which the app trusts and the runner checks, and the prompt's list of it |
//! | [`credentials`] | credential-shaped argv words and env values written inline in a committed file, and how the prompt masks them |
//! | [`child_env`] | the variables no child process may inherit (a source's token and address, gitleaks' config overrides) |
//! | [`owner_only`] | files and folders only the current user can open: mode bits on Unix, a single access entry on Windows |
//! | [`workdir`] | the work folder's own `.gitignore` and the sweep of stale scanner scratch |
//! | [`workflow`] | an item's local status flow: moves, human gates, pending approvals, rounds, history, the tracker mismatch, the status block the app shows |
//!
//! Engines are CLIs run argv-only through [`cli::EngineCli`]; a verb hands
//! a long step (a build, a pull, an `up`, a dev server) back as a run block
//! for the host to run as a job, and asks only quick questions in place.
//! Besides those, the processes this crate runs are `git` (see
//! [`provenance::GitCli`]) and, for secret scanning, the configured scanner
//! (gitleaks or a hook), argv only and only when asked.
//!
//! # Where the definition lives
//!
//! The `stacks:` map is part of the project's `chappa.yml`. When it grows
//! large it may move, whole, into `chappa.stack.yml` beside it: that file
//! holds the same `stacks:` document and nothing else. The two are
//! exclusive. If `chappa.stack.yml` exists, `chappa.yml` must not also carry
//! a `stacks:` key, and [`definition::load_from_project`] refuses the
//! project naming both rather than guessing which one wins. The project
//! file parser keeps unknown top-level keys verbatim, so a `stacks:` key in
//! `chappa.yml` survives every write-back of the process list untouched.
//!
//! # Placeholders and escaping
//!
//! Strings in the definition may contain `{name}` or `{name:argument}`
//! placeholders. A literal brace is written doubled (`{{` and `}}`). An
//! unknown placeholder is an error that names it, never silently kept, so a
//! typo in a URL template fails at validation instead of at runtime.

pub mod checks;
pub mod child_env;
pub mod cli;
pub mod compose;
pub mod credentials;
pub mod definition;
pub mod driver;
pub mod hosts;
pub mod images;
pub mod instance_secrets;
pub mod intent;
pub mod jobs;
pub mod owner_only;
pub mod placeholders;
pub mod planner;
pub mod ports;
pub mod probe;
pub mod process;
pub mod provenance;
pub mod runner;
pub mod safety;
pub mod script;
pub mod states;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod trust;
pub mod workdir;
pub mod workflow;
pub mod workspace;
