//! The one launch gate every pty spawn passes, keyed on the folder the
//! child starts in and on who asked for it.
//!
//! - **The person's terminals** are not gated: a terminal the person opens
//!   is the person at a keyboard.
//! - **Agents the person starts** get the folder check when a folder other
//!   than the project root is named, then the data-safety and secret-scan
//!   checks (the person may launch past an unfinished scan from the app's
//!   own dialog).
//! - **Anything an agent starts**, terminal or agent alike, must start
//!   inside the calling agent's own project roots (with no folder named it
//!   starts in that project's root; an agent with no project must name a
//!   folder inside an open project), and gets the data-safety and
//!   secret-scan checks with no way past them.
//!
//! [`crate::registry::Registry::gate_spawn`] runs a gate and hands back a
//! [`GatePass`] naming the folder it cleared; the registry's spawn path
//! takes a spawn only with a pass for the same folder and caller, or runs
//! its own gate when it gets none (and refuses an agent's spawn when it has
//! none set).

use std::path::{Path, PathBuf};

use crate::caller::Caller;
use crate::projects::Projects;
use crate::registry::TermId;
use crate::roots::check_cwd;
use crate::secret_scan::ScanNotice;
use crate::settings::SettingsState;

/// What is being started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnKind {
    Terminal,
    /// An agent; `container` = its tool runs in a container, which reads
    /// the container's workspace rather than this folder (no secret scan).
    Agent {
        container: bool,
    },
}

/// One spawn as the gate judges it.
#[derive(Debug, Clone, Copy)]
pub struct GateRequest<'a> {
    pub caller: &'a Caller,
    pub kind: SpawnKind,
    /// The project the spawn is for (the caller's own, for an agent).
    pub project_id: Option<u32>,
    /// The folder asked for, as given; `None` = the default folder.
    pub working_dir: Option<&'a str>,
    /// The person's note from the launch dialog's "Launch anyway".
    pub launch_anyway: Option<&'a str>,
}

/// What a gate decided: the folder to start in, and a notice to raise.
#[derive(Debug, Clone, Default)]
pub struct GateOutcome {
    pub cwd: Option<PathBuf>,
    pub notice: Option<ScanNotice>,
}

pub trait SpawnGate: Send + Sync {
    fn check(&self, req: &GateRequest<'_>) -> Result<GateOutcome, String>;
}

/// Proof that the gate cleared a spawn in `cwd` for `caller`. Only
/// [`GatePass::issue`] makes one, and the registry's spawn path checks it
/// against the spawn it is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatePass {
    cwd: Option<PathBuf>,
    caller: Option<TermId>,
}

impl GatePass {
    pub(crate) fn issue(cwd: Option<PathBuf>, caller: &Caller) -> Self {
        Self {
            cwd,
            caller: caller.grant().map(|g| g.process_id),
        }
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// Whether this pass covers a spawn in `cwd` by `caller`.
    pub(crate) fn covers(&self, cwd: Option<&Path>, caller: &Caller) -> bool {
        self.cwd.as_deref() == cwd && self.caller == caller.grant().map(|g| g.process_id)
    }
}

/// The app's gate: the projects' roots and sources, the security settings.
#[derive(Clone)]
pub struct AppSpawnGate {
    pub projects: Projects,
    pub settings: SettingsState,
}

/// Why an agent may not spawn in a project other than its own.
pub fn other_project_refusal(asked: u32, own: Option<u32>) -> String {
    match own {
        Some(own) => format!(
            "refused: an agent may start processes only in its own project ({own}), not project {asked}"
        ),
        None => format!(
            "refused: an agent with no project may not start processes in project {asked}"
        ),
    }
}

impl SpawnGate for AppSpawnGate {
    fn check(&self, req: &GateRequest<'_>) -> Result<GateOutcome, String> {
        let agent_caller = req.caller.is_agent();
        let working_dir = req.working_dir.map(str::trim).filter(|w| !w.is_empty());
        if !agent_caller && req.kind == SpawnKind::Terminal {
            return Ok(GateOutcome {
                cwd: working_dir.map(PathBuf::from),
                notice: None,
            });
        }
        let project_id = match req.caller.grant() {
            Some(grant) => match req.project_id {
                Some(asked) if Some(asked) != grant.project_id => {
                    return Err(other_project_refusal(asked, grant.project_id))
                }
                _ => grant.project_id,
            },
            None => req.project_id,
        };
        let project = match project_id {
            Some(id) => Some(
                self.projects
                    .project_root(id)
                    .ok_or_else(|| format!("no such project: {id}"))?,
            ),
            None => None,
        };
        let cwd: Option<PathBuf> = match (working_dir, &project) {
            (Some(dir), _) => Some(
                check_cwd(&self.projects.cwd_roots(project_id, None), dir)
                    .into_result()
                    .map_err(|why| {
                        if agent_caller {
                            format!("refused: {why}")
                        } else {
                            why
                        }
                    })?,
            ),
            (None, Some((_, root))) => Some(root.clone()),
            (None, None) if agent_caller => {
                return Err(
                    "refused: an agent with no project must name a working folder \
                            inside an open project"
                        .into(),
                )
            }
            (None, None) => None,
        };
        let Some(dir) = cwd.as_deref() else {
            return Ok(GateOutcome::default());
        };
        crate::data_safety::check_launch(&self.projects.sources(), dir)?;
        let container = matches!(req.kind, SpawnKind::Agent { container: true });
        let notice = if container {
            None
        } else {
            let project_root = project
                .as_ref()
                .map(|(_, root)| root.as_path())
                .unwrap_or(dir);
            let pref = project_id.and_then(|id| self.projects.secret_scan_pref(id));
            let configs = crate::secret_scan::ScanConfigs {
                project: self.projects.scan_config_for(project_id),
                open_roots: self.projects.open_roots(),
            };
            crate::secret_scan::gate_launch_with(
                &self.settings.snapshot(),
                pref,
                project_root,
                dir,
                // Only the person, in the app's dialog, can launch past an
                // unfinished scan.
                if agent_caller {
                    None
                } else {
                    req.launch_anyway
                },
                &configs,
            )?
        };
        Ok(GateOutcome { cwd, notice })
    }
}

/// The HTTP status for a gate refusal: a blocked folder is a conflict
/// (409), a folder outside the allowed roots or another project is
/// forbidden (403), an unknown project 404.
pub fn refusal_status(err: &str) -> u16 {
    if err.starts_with("no such") {
        404
    } else if crate::data_safety::is_refusal(err)
        || err.starts_with(crate::secret_scan::BLOCK_PREFIX)
    {
        409
    } else if err.starts_with("refused") {
        403
    } else {
        500
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blocked folder answers 409, a folder or project an agent may not
    /// use 403, an unknown project 404; the person's pass covers only the
    /// folder and caller it was issued for.
    #[test]
    fn gate_refusals_map_to_statuses() {
        assert_eq!(
            refusal_status(&format!("{}{{}}", crate::secret_scan::BLOCK_PREFIX)),
            409
        );
        assert_eq!(
            refusal_status(&format!("{}x`: blocked", crate::data_safety::REFUSAL_LEAD)),
            409
        );
        assert_eq!(refusal_status(&other_project_refusal(3, Some(1))), 403);
        assert_eq!(refusal_status("no such project: 9"), 404);
        assert_eq!(refusal_status("pty spawn failed"), 500);
        let pass = GatePass::issue(Some(PathBuf::from("/srv/app")), &Caller::Person);
        assert!(pass.covers(Some(Path::new("/srv/app")), &Caller::Person));
        assert!(!pass.covers(Some(Path::new("/srv/other")), &Caller::Person));
        assert!(!pass.covers(None, &Caller::Person));
        let agent = Caller::Agent(crate::caller::AgentGrant {
            process_id: 4,
            project_id: None,
            parent_process_id: None,
            tool: None,
            created_ms: 0,
        });
        assert!(!pass.covers(Some(Path::new("/srv/app")), &agent));
    }
}
