//! The jiuyue desktop shell for Windows and macOS (Tauri v2).
//!
//! Two things live here that cannot live in the webview: the **system tray**
//! (minimise-to-tray, unread state, an explicit Quit) and the **Call window**
//! (a Call must run in its own OS window — a hard product requirement, not a
//! styling preference).
//!
//! Everything the frontend is allowed to ask for goes through the
//! `desktop-shell` interface; the commands here are the Rust half of that seam.

mod call_window;
mod commands;
mod tray;

/// Build and run the shell.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            commands::open_call_window,
            commands::close_call_window,
            commands::set_unread,
        ])
        .setup(|app| {
            tray::install(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != tray::MAIN_WINDOW {
                return;
            }

            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Closing the main window must not quit the app: a chat client
                // that dies on close cannot deliver notifications. The webview is
                // hidden rather than destroyed, so its realtime socket — the thing
                // that makes background notifications possible — keeps running.
                // Quitting is explicit, from the tray (product spec 141, 143).
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to run the jiuyue desktop shell");
}
