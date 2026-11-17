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
//! | [`secret_scan`] | secret scans of the bound worktrees, their commits, build contexts and env; the `secretScan` block the app enforces on |
//! | [`env_view`] | the Environment payload from the runner's status |
//! | [`actions`] | the verbs offered, planned for their confirm text |
//! | [`hidden_output`] | the `logs` terminals' output with the generated secrets hidden |
//! | [`records`] | job records under `.work/jobs/`, exit reports, retention |
//! | [`runjob`] | `run-job`: one stored job, start to finish |
//! | [`procs`] | the process table of process components |
//! | [`background`] | registry digests, engine facts and script components' status and plan output, refreshed off the status path |
//! | [`relay`] | the gateway-only TCP relay |
//! | [`timeouts`] | the per-step time limits |
//! | [`world`] | every outside tool as a shareable trait object, real or fake |
//! | [`workspaces`] | item workspaces: Start work, worktrees, activate, archive, drift, served as overlays on a tracker's Work rows |
//! | [`local_status`] | an item's local workflow on its overlay: the status block, moves, approvals, the tracker-status banner |
//! | [`states`] | saved stack states: save (snapshot hooks), prune, resume (exists, restore, recipe replay), the automatic save before destructive verbs, the volume guard |
//! | [`seeds`] | which state seeds a work item asks for, chosen on its overlay |
//! | [`reviews`] | PR review workspaces: a PR head checked out under `reviews/`, a slot brought up from it, the panel |

pub mod actions;
pub mod background;
pub mod commands;
pub mod data_safety;
pub mod env_view;
pub mod hidden_output;
pub mod identity;
pub mod local_status;
pub mod procs;
pub mod project;
pub mod records;
pub mod relay;
pub mod reviews;
pub mod runjob;
pub mod secret_scan;
pub mod seeds;
pub mod server;
pub mod states;
pub mod timeouts;
pub mod workspaces;
pub mod world;
