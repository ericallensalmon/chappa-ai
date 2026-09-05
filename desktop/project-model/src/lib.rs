//! Project data model: the per-project descriptor (`chappa.yml`)
//! parsing/validation, the stored-projects list, and the process-lifecycle
//! rules that make chappa-ai desktop's project file concrete.
//!
//! This crate is the container-testable core: no `tauri` dependency, pure
//! serde/fs logic, everything `cargo test -p project-model` can exercise headless
//! on Linux. The Tauri glue in `src-tauri` is deliberately thin on top of it
//! and is verified on the host (it needs GTK headers we don't have here).
//!
//! Schema authority: the example `chappa.yml` at the repo root. Where the
//! yml is ambiguous, field semantics follow the observed behavior and the
//! MCP `help()` output;
//! inferences are called out in doc comments.

pub mod atomic;
pub mod backoff;
pub mod hash;
pub mod native;
pub mod projects;
pub mod settings;
pub mod status;
pub mod watcher;
pub mod workspace;
pub mod writeback;
pub mod yml;
