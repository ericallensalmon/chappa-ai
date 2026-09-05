use tauri::{Manager, RunEvent};

pub mod agent_json;
pub mod agent_tools;
pub mod agents;
pub mod event_ring;
mod commands;
pub mod control_http;
pub mod coordination;
pub mod coordination_http;
#[cfg(debug_assertions)]
pub mod debug_http;
pub mod projects;
pub mod registry;
pub mod settings;
pub mod stats;
pub mod timers;
pub mod timers_http;
pub mod workspace;

use agent_tools::AgentToolsState;
use coordination::Coordination;
use projects::Projects;
use registry::{AppClosedBroadcast, Registry};
use settings::SettingsState;
use workspace::Workspace;

#[tauri::command]
fn is_debug() -> bool {
    cfg!(debug_assertions)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        // The native folder picker. Rust-side only — `pick_directory`
        // is the sole surface the webview sees (no npm @tauri-apps/plugin-dialog).
        .plugin(tauri_plugin_dialog::init())
        .manage(Registry::default())
        .manage(Projects::default())
        .manage(SettingsState::default())
        .manage(AgentToolsState::default())
        .manage(Coordination::default())
        .manage(Workspace::default())
        .setup(|app| {
            let config_dir = app.path().app_config_dir().unwrap_or_default();
            // Load `<app-config>/settings.json` BEFORE the projects
            // store: project spawns read the execution profile out of it.
            let settings = app.state::<SettingsState>();
            settings.init(config_dir.clone());
            // Point the `term://closed` broadcast at the real app
            // handle, so EVERY close (frontend ×, MCP/control close_terminal,
            // project stop, close_on_exit) reaches the open webview.
            app.state::<Registry>()
                .set_closed_broadcast(std::sync::Arc::new(AppClosedBroadcast(app.handle().clone())));
            // `<app-config>/agent_tools.json`.
            app.state::<AgentToolsState>().init(config_dir.clone());
            // `<app-config>/coordination.db` (scratchpads + todos).
            app.state::<Coordination>().init(&config_dir);
            // The workspace command store + runtime, initialized
            // AFTER settings load (auto_start commands start at launch, and a
            // spawn resolves the execution profile out of settings).
            app.state::<Workspace>()
                .init(app.handle(), config_dir.clone(), settings.inner().clone());
            // Point the projects store at the app-config dir and start the
            // 500ms-alive ticker. Idempotent: setup runs once.
            app.state::<Projects>()
                .init(app.handle(), config_dir.clone(), settings.inner().clone());
            // The timer scheduler. Started BEFORE the control
            // surface so a `timer_set` cannot arrive at an inert service, and
            // after the coordination store so `restore` can read the
            // persisted timers (missed absolute ones fire once here).
            let timers = timers::TimerService::start(
                app.state::<Registry>().inner().clone(),
                settings.inner().clone(),
                app.state::<Coordination>().inner().clone(),
            );
            // The control surface (127.0.0.1:8324, bearer token) —
            // ALL builds; the chappa-ai-mcp stdio server talks to this.
            control_http::start(
                control_http::ControlState {
                    registry: app.state::<Registry>().inner().clone(),
                    projects: app.state::<Projects>().inner().clone(),
                    settings: settings.inner().clone(),
                    agent_tools: app.state::<AgentToolsState>().inner().clone(),
                    coordination: app.state::<Coordination>().inner().clone(),
                    timers,
                    // The webview adoption broadcast rides this.
                    app: Some(app.handle().clone()),
                    // Filled in by `start` from the argument below (it is the
                    // same path; `GET /` reports it as instance identity).
                    ..control_http::ControlState::default()
                },
                config_dir,
            );
            // The one process-stats poller. It parks immediately (the
            // registry is empty at setup) and is woken by the first terminal.
            stats::spawn_poller(
                app.state::<Registry>().inner().clone(),
                app.handle().clone(),
            );
            // The one docker-exec bridge probe (parks until the
            // first docker-exec agent spawns). Each tick also runs
            // an orphan sweep over the probe's containers, gated by the
            // setting; `app_reap_emitter` surfaces any reaping as ONE
            // notification-center entry per container.
            agents::spawn_bridge_probe(
                app.state::<Registry>().inner().clone(),
                settings.inner().clone(),
                app.state::<AgentToolsState>().inner().clone(),
                agents::app_bridge_emitter(app.handle().clone()),
                agents::app_reap_emitter(app.handle().clone()),
            );
            // The STARTUP orphan sweep — once, after the agent-tool
            // store is loaded and the registry is (still) empty, every
            // container named by an ENABLED docker_exec tool is swept, so a
            // hard-killed previous run's orphaned shells are reaped on boot.
            // Gated by the setting; runs off the setup thread (docker calls).
            {
                let sweep_registry = app.state::<Registry>().inner().clone();
                let sweep_tools = app.state::<AgentToolsState>().inner().clone();
                let sweep_settings = settings.inner().clone();
                let sweep_app = app.handle().clone();
                std::thread::spawn(move || {
                    if !sweep_settings.snapshot().agent_reap_orphans {
                        return;
                    }
                    agents::reap_orphans(&sweep_registry, &sweep_tools, None, Some(&*agents::app_reap_emitter(sweep_app)));
                });
            }
            #[cfg(debug_assertions)]
            {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
                // Parity-harness surface, driven over HTTP.
                debug_http::start(app.state::<Registry>().inner().clone());
            }
            Ok(())
        });

    #[cfg(debug_assertions)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        is_debug,
        commands::create_terminal,
        commands::write_key,
        commands::paste,
        commands::mouse,
        commands::resize,
        commands::scroll,
        commands::set_display_offset,
        commands::ack,
        commands::request_full,
        commands::selection,
        commands::copy_selection,
        commands::search,
        commands::search_nav,
        commands::close_terminal,
        commands::debug_stats,
        commands::list_terminals,
        commands::attach_terminal,
        commands::spawn_shell,
        commands::os_notify,
        settings::get_settings,
        settings::set_settings,
        projects::list_projects,
        projects::add_project,
        projects::rename_project,
        projects::pick_directory,
        projects::remove_project,
        projects::open_project,
        projects::confirm_project_trust,
        projects::list_project_processes,
        projects::start_project_process,
        projects::stop_project_process,
        projects::restart_project_process,
        projects::set_notification_level,
        projects::save_project_process,
        projects::delete_project_process,
        projects::duplicate_project_processes,
        projects::set_process_favorite,
        projects::set_process_auto_rename,
        workspace::list_workspace_commands,
        workspace::save_workspace_command,
        workspace::delete_workspace_command,
        workspace::start_workspace_command,
        workspace::stop_workspace_command,
        workspace::restart_workspace_command,
        agent_tools::list_agent_tools,
        agent_tools::upsert_agent_tool,
        agent_tools::delete_agent_tool,
        agent_tools::parse_agent_command,
        agent_tools::agent_tool_template,
        agents::spawn_agent,
        agents::send_agent_input,
        agents::get_agent_events,
        coordination::list_scratchpads,
        coordination::read_scratchpad,
    ]);
    #[cfg(not(debug_assertions))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        is_debug,
        commands::create_terminal,
        commands::write_key,
        commands::paste,
        commands::mouse,
        commands::resize,
        commands::scroll,
        commands::set_display_offset,
        commands::ack,
        commands::request_full,
        commands::selection,
        commands::copy_selection,
        commands::search,
        commands::search_nav,
        commands::close_terminal,
        commands::debug_stats,
        commands::list_terminals,
        commands::attach_terminal,
        commands::spawn_shell,
        commands::os_notify,
        settings::get_settings,
        settings::set_settings,
        projects::list_projects,
        projects::add_project,
        projects::rename_project,
        projects::pick_directory,
        projects::remove_project,
        projects::open_project,
        projects::confirm_project_trust,
        projects::list_project_processes,
        projects::start_project_process,
        projects::stop_project_process,
        projects::restart_project_process,
        projects::set_notification_level,
        projects::save_project_process,
        projects::delete_project_process,
        projects::duplicate_project_processes,
        projects::set_process_favorite,
        projects::set_process_auto_rename,
        agent_tools::list_agent_tools,
        agent_tools::upsert_agent_tool,
        agent_tools::delete_agent_tool,
        agent_tools::parse_agent_command,
        agent_tools::agent_tool_template,
        agents::spawn_agent,
        agents::send_agent_input,
        agents::get_agent_events,
        coordination::list_scratchpads,
        coordination::read_scratchpad,
    ]);

    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        // Children must not outlive the app (Windows especially): on any
        // exit, stop every project process (cancelling pending respawns) and
        // shut every actor down + join its pump before the process goes.
        // Idempotent — a second Exit finds the registry empty.
        if let RunEvent::ExitRequested { .. } | RunEvent::Exit = event {
            let projects = app_handle.state::<Projects>();
            let workspace = app_handle.state::<Workspace>();
            let registry = app_handle.state::<Registry>();
            projects.shutdown(&registry);
            workspace.shutdown(&registry);
            registry.shutdown_all();
        }
    });
}

