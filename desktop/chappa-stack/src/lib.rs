//! `chappa-stack`: the stack runner as a source the app launches.
//!
//! A project whose `chappa.yml` defines `stacks:` adds one command with
//! `source: {}` that runs `chappa-stack serve`. The app hands it a loopback
//! address and a token, checks its identity, and polls its Environment
//! view; the stack's verbs come back as actions. A mutating action is one
//! job: the plan the user confirmed is stored under a job id, and the pane
//! the app opens runs `chappa-stack run-job <id>`, which executes the whole
//! plan in order with its output live. Without a pane (a headless caller),
//! the source runs the same job itself.
//!
//! | module | owns |
//! |---|---|
//! | [`identity`] | the HMAC identity proof and the constant-time bearer check |
//! | [`server`] | the routes, loopback-only bind, the request pool |
//! | [`project`] | which project, stack and slot; the definition read fresh per request |
//! | [`commands`] | `chappa.yml` `processes:` as the process driver's command table |
//! | [`data_safety`] | data-safety hooks run, judged and recorded; the `dataSafety` block the app enforces on |
//! | [`env_view`] | the Environment payload from the runner's status |
//! | [`actions`] | the verbs offered, planned for their confirm text |
//! | [`records`] | job records under `.work/jobs/`, exit reports, retention |
//! | [`runjob`] | `run-job`: one stored job, start to finish |
//! | [`procs`] | the process table of process components |
//! | [`background`] | registry digests and engine facts, refreshed off the status path |
//! | [`relay`] | the gateway-only TCP relay |
//! | [`timeouts`] | the per-step time limits |
//! | [`world`] | every outside tool as a shareable trait object, real or fake |

pub mod actions;
pub mod background;
pub mod commands;
pub mod data_safety;
pub mod env_view;
pub mod identity;
pub mod procs;
pub mod project;
pub mod records;
pub mod relay;
pub mod runjob;
pub mod server;
pub mod timeouts;
pub mod world;
