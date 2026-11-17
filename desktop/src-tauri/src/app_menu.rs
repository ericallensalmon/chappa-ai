//! The macOS menu bar.
//!
//! Without a menu of its own the app would get the framework's default one,
//! whose File ▸ Close Window and Window ▸ Close Window answer Cmd+W. The app
//! has a single window, so closing it quits, and Cmd+W sits right next to
//! the Cmd+Shift+W that closes a pane. This menu is the same as the default
//! (the app menu with About, Hide and Quit, Edit, View, Window, Help)
//! except that no item closes the window: File holds Close Pane on Cmd+W,
//! which the webview performs. Cmd+Q stays Quit. Other systems keep no menu
//! bar.

/// The id of File ▸ Close Pane.
pub const CLOSE_PANE_ID: &str = "close-pane";

/// What Close Pane emits to the webview, which closes the active pane with
/// the same confirmation as any other close.
pub const CLOSE_PANE_EVENT: &str = "menu://close-pane";

/// The menu bar, installed with `Builder::menu`.
#[cfg(target_os = "macos")]
pub fn build<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<tauri::menu::Menu<R>> {
    use tauri::menu::{
        AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu, HELP_SUBMENU_ID,
        WINDOW_SUBMENU_ID,
    };

    let info = app.package_info();
    let about = AboutMetadata {
        name: Some(info.name.clone()),
        version: Some(info.version.to_string()),
        ..Default::default()
    };
    let app_menu = Submenu::with_items(
        app,
        info.name.clone(),
        true,
        &[
            &PredefinedMenuItem::about(app, None, Some(about))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::services(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::show_all(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::quit(app, None)?,
        ],
    )?;
    let file = Submenu::with_items(
        app,
        "File",
        true,
        &[&MenuItem::with_id(
            app,
            CLOSE_PANE_ID,
            "Close Pane",
            true,
            Some("CmdOrCtrl+W"),
        )?],
    )?;
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;
    let view = Submenu::with_items(
        app,
        "View",
        true,
        &[&PredefinedMenuItem::fullscreen(app, None)?],
    )?;
    let window = Submenu::with_id_and_items(
        app,
        WINDOW_SUBMENU_ID,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
        ],
    )?;
    let help = Submenu::with_id_and_items(app, HELP_SUBMENU_ID, "Help", true, &[])?;
    Menu::with_items(app, &[&app_menu, &file, &edit, &view, &window, &help])
}

/// Forward Close Pane to the webview.
#[cfg(target_os = "macos")]
pub fn on_event<R: tauri::Runtime>(app: &tauri::AppHandle<R>, event: tauri::menu::MenuEvent) {
    use tauri::Emitter;
    if event.id() == CLOSE_PANE_ID {
        if let Err(err) = app.emit(CLOSE_PANE_EVENT, ()) {
            log::warn!("[menu] cannot forward Close Pane: {err}");
        }
    }
}
