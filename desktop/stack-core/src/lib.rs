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
//! | [`safety`] | data-safety and secret-scan types whose findings can never carry a value |
//!
//! Nothing here spawns an engine. The only process this crate ever runs is
//! `git` (see [`provenance::GitCli`]), argv only, and only when asked.
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

pub mod definition;
pub mod driver;
pub mod intent;
pub mod placeholders;
pub mod planner;
pub mod ports;
pub mod provenance;
pub mod safety;
