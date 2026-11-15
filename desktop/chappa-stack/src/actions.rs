//! The stack verbs as source actions.
//!
//! Every mutating verb is planned when the Environment payload is built,
//! so its `confirm` text is the plan itself ("Pull db (postgres:16);
//! start db, migrate, api"), and an action whose plan has nothing to do is
//! not offered at all. The plan's fingerprint travels in the action's
//! `params`; when the action comes back, the instance is observed again
//! and a different fingerprint means the user confirmed something that is
//! no longer what would happen: the call is refused (409) and the view
//! shows the new plan on its next poll.
//!
//! | key | id | verb |
//! |---|---|---|
//! | none (the stack) | `up` | bring the default selection up |
//! | | `start` | start what exists, recreating what baked in a stale gateway |
//! | | `stop` | stop everything, in stop order, with grace periods |
//! | | `down` | stop and remove every container (volumes kept) |
//! | | `rebuild` | pull or build every image again and recreate |
//! | | `gc` | remove unreferenced images, prune build caches |
//! | component | `start` | bring it (and what it depends on) up |
//! | | `stop` | stop it alone |
//! | | `restart` | `compose restart` |
//! | | `rebuild` | pull or build its image again and recreate it (and what pins it) |
//! | | `mode-prebuilt` / `mode-source` / `mode-dev` | switch its mode |
//! | | `watch` | dev mode with the watch command |
//! | | `logs` | a terminal following its output (not a job) |
//! | | `open-<output>` | a link to one of its URLs |

use indexmap::IndexMap;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use stack_core::cli::engine_argv;
use stack_core::definition::{DriverKind, Mode};
use stack_core::driver::ResolvedInstance;
use stack_core::planner::{Change, Plan, Selection, Step, Verb};
use stack_core::runner::{Observation, StackRunner};

use crate::records::JobWork;

/// One endpoint action as offered right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Offered {
    pub id: String,
    /// The component; `None` for the whole stack.
    pub key: Option<String>,
    pub label: String,
    pub confirm: String,
    pub work: JobWork,
}

impl Offered {
    pub fn to_json(&self, fingerprint: &str) -> Value {
        json!({
            "kind": "endpoint",
            "id": self.id,
            "label": self.label,
            "mutates": true,
            "confirm": self.confirm,
            "params": { "fingerprint": fingerprint },
        })
    }
}

/// `Pull db (postgres:16); start db, migrate, api; recreate web (mode
/// prebuilt → source).` plus the plan's notes and what a verb does to data.
pub fn confirm_text(plan: &Plan) -> String {
    let mut groups: IndexMap<&'static str, Vec<String>> = IndexMap::new();
    for step in &plan.steps {
        let item = match (step, step.detail()) {
            (Step::Stop { .. }, _) | (_, None) => step.component().to_string(),
            // The pull's tag template, with the channel this plan uses.
            (_, Some(d)) => match &plan.channel {
                Some(c) => format!("{} ({})", step.component(), d.replace("{channel}", c)),
                None => format!("{} ({d})", step.component()),
            },
        };
        let list = groups.entry(step.verb_label()).or_default();
        if !list.contains(&item) {
            list.push(item);
        }
    }
    let mut parts: Vec<String> = groups
        .into_iter()
        .map(|(verb, items)| format!("{} {}", verb.to_lowercase(), items.join(", ")))
        .collect();
    if let Some(first) = parts.first_mut() {
        *first = capitalize(first);
    }
    let mut text = parts.join("; ");
    if !text.is_empty() {
        text.push('.');
    }
    if plan.verb == Verb::Down {
        text.push_str(" Containers are removed; volumes are kept.");
    }
    for note in &plan.notes {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&capitalize(note));
        if !note.ends_with('.') {
            text.push('.');
        }
    }
    text
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The channel an `up` resolves `{channel}` with: the one the instance
/// already recorded, else the configured default.
pub fn channel_for(obs: &Observation, default: &str) -> String {
    obs.intent
        .components
        .values()
        .find_map(|c| c.channel.clone())
        .unwrap_or_else(|| default.to_string())
}

fn planned(
    runner: &StackRunner<'_>,
    obs: &Observation,
    id: &str,
    key: Option<&str>,
    label: &str,
    change: Change,
) -> Option<Offered> {
    let plan = runner.plan(obs, &change).ok()?;
    let records_only = plan.steps.is_empty() && !plan.desired.is_empty();
    if plan.steps.is_empty() && !records_only {
        return None;
    }
    Some(Offered {
        id: id.to_string(),
        key: key.map(str::to_string),
        label: label.to_string(),
        confirm: {
            // A plan without its own channel pulls with the one each
            // component recorded; show that one.
            let mut shown = plan.clone();
            if shown.channel.is_none() {
                shown.channel = obs
                    .intent
                    .components
                    .get(key.unwrap_or_default())
                    .and_then(|c| c.channel.clone())
                    .or_else(|| {
                        obs.intent
                            .components
                            .values()
                            .find_map(|c| c.channel.clone())
                    });
            }
            confirm_text(&shown)
        },
        work: JobWork::Plan {
            change,
            plan: Box::new(plan),
            save: None,
            remove_volumes: false,
        },
    })
}

/// The stack-wide actions.
pub fn stack_actions(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    channel: &str,
) -> Vec<Offered> {
    let mut out = Vec::new();
    let up = Change::Up {
        selection: Selection::Default,
        modes: IndexMap::new(),
        channel: Some(channel.to_string()),
    };
    out.extend(planned(runner, obs, "up", None, "Bring up", up));
    out.extend(planned(
        runner,
        obs,
        "start",
        None,
        "Start (after a reboot)",
        StackRunner::start_change(obs),
    ));
    out.extend(planned(
        runner,
        obs,
        "stop",
        None,
        "Stop",
        Change::Stop {
            keep_groups: Vec::new(),
        },
    ));
    out.extend(planned(
        runner,
        obs,
        "down",
        None,
        "Down",
        Change::Down {
            keep_groups: Vec::new(),
        },
    ));
    out.extend(planned(
        runner,
        obs,
        "rebuild",
        None,
        "Rebuild all",
        Change::RebuildAll,
    ));
    let engines: Vec<String> = inst
        .engines()
        .into_iter()
        .filter(|e| e.kind.is_container_engine())
        .map(|e| e.name)
        .collect();
    if !engines.is_empty() {
        out.push(Offered {
            id: "gc".into(),
            key: None,
            label: "Clean up images".into(),
            confirm: format!(
                "Remove images that no running instance, kept state or pinned channel uses, and prune the build cache on {}. The job lists every image before it removes anything.",
                engines.join(", ")
            ),
            work: JobWork::Gc,
        });
    }
    out
}

/// The current mode of a component: what the observation says, else the
/// definition's default.
pub fn mode_now(runner: &StackRunner<'_>, obs: &Observation, component: &str) -> Option<Mode> {
    obs.state
        .components
        .get(component)
        .and_then(|c| c.mode)
        .or_else(|| {
            runner
                .stack
                .components
                .get(component)
                .and_then(|c| c.default_mode())
        })
}

/// The endpoint actions on one component.
pub fn component_actions(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    component: &str,
    channel: &str,
) -> Vec<Offered> {
    let mut out = Vec::new();
    let Some(def) = runner.stack.components.get(component) else {
        return out;
    };
    let one = vec![component.to_string()];
    out.extend(planned(
        runner,
        obs,
        "start",
        Some(component),
        "Start",
        Change::Up {
            selection: Selection::Components(one.clone()),
            modes: IndexMap::new(),
            channel: Some(channel.to_string()),
        },
    ));
    out.extend(planned(
        runner,
        obs,
        "stop",
        Some(component),
        "Stop",
        Change::StopComponents { components: one },
    ));
    let current = mode_now(runner, obs, component);
    let running = obs.state.component(component).running;
    let as_process = StackRunner::runs_as_process(inst, component, current);
    if running && !as_process && def.driver == DriverKind::Compose {
        let grace = runner.stack.grace_seconds(component);
        out.push(Offered {
            id: "restart".into(),
            key: Some(component.to_string()),
            label: "Restart".into(),
            confirm: format!("Restart {component} (stop with a {grace} s grace, then start)."),
            work: JobWork::Restart {
                component: component.to_string(),
            },
        });
    }
    if matches!(current, Some(Mode::Prebuilt | Mode::Source))
        && obs.state.component(component).exists
    {
        out.extend(planned(
            runner,
            obs,
            "rebuild",
            Some(component),
            "Rebuild",
            Change::Rebuild {
                component: component.to_string(),
            },
        ));
    }
    if def.modes.len() > 1 {
        for mode in [Mode::Prebuilt, Mode::Source, Mode::Dev] {
            if !def.has_mode(mode) || Some(mode) == current {
                continue;
            }
            out.extend(planned(
                runner,
                obs,
                &format!("mode-{mode}"),
                Some(component),
                &format!("Switch to {mode}"),
                Change::SwitchMode {
                    component: component.to_string(),
                    mode,
                    git_ref: None,
                },
            ));
        }
    }
    if def.has_mode(Mode::Dev) && def.watch_command().is_some() {
        out.extend(planned(
            runner,
            obs,
            "watch",
            Some(component),
            "Watch",
            Change::Watch {
                component: component.to_string(),
                git_ref: None,
            },
        ));
    }
    out
}

/// Every endpoint action offered now, the stack's first.
pub fn all_actions(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    channel: &str,
) -> Vec<Offered> {
    let mut out = stack_actions(runner, inst, obs, channel);
    for name in inst.components.keys() {
        out.extend(component_actions(runner, inst, obs, name, channel));
    }
    out
}

/// The `logs` terminal action: `<engine> logs -f <container>` for a
/// container, or this program following a process component's log file.
pub fn logs_action(
    inst: &ResolvedInstance,
    obs: &Observation,
    component: &str,
    project_dir: &Path,
    process_log: Option<&PathBuf>,
    exe: &Path,
) -> Option<Value> {
    let c = inst.components.get(component)?;
    let command: Vec<String> = match (process_log, obs.ps.get(component)) {
        (Some(log), _) => vec![
            exe.display().to_string(),
            "follow".into(),
            log.display().to_string(),
        ],
        (None, Some(ps)) if !ps.name.is_empty() => {
            let mut argv = engine_argv(&c.engine);
            argv.extend(["logs".to_string(), "-f".to_string(), ps.name.clone()]);
            argv
        }
        _ => return None,
    };
    Some(json!({
        "kind": "terminal",
        "id": "logs",
        "label": "Logs",
        "cwd": project_dir.display().to_string(),
        "command": command,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stack_core::planner::RecreateReason;

    #[test]
    fn confirm_text_names_each_step_kind_once_in_order() {
        let mut plan = Plan::empty("app-0", Verb::Up, "f");
        plan.steps = vec![
            Step::Pull {
                component: "db".into(),
                image: "postgres".into(),
                tag: Some("16".into()),
            },
            Step::Start {
                component: "api".into(),
            },
            Step::Start {
                component: "db".into(),
            },
            Step::Recreate {
                component: "web".into(),
                reason: RecreateReason::Rebuilt,
            },
        ];
        assert_eq!(
            confirm_text(&plan),
            "Pull db (postgres:16); start api, db; recreate web (rebuilt)."
        );
        let mut down = Plan::empty("app-0", Verb::Down, "f");
        down.steps = vec![
            Step::Stop {
                component: "db".into(),
                grace_seconds: 30,
            },
            Step::Remove {
                component: "db".into(),
            },
        ];
        down.notes.push("api was already stopped".into());
        assert_eq!(
            confirm_text(&down),
            "Stop db; remove db. Containers are removed; volumes are kept. Api was already stopped."
        );
    }
}
