//! The commands the `desktop-shell` interface invokes.
//!
//! Each one is a thin translation of a capability into a shell action; the
//! behaviour lives in `call_window` and `tray`, which are unit-tested without a
//! running webview.

use tauri::AppHandle;

use crate::call_window::CallWindowRequest;

/// Open (or focus) the Call window for a Conversation.
#[tauri::command]
pub fn open_call_window(app: AppHandle, request: CallWindowRequest) -> Result<String, String> {
    crate::call_window::open(&app, &request)
}

/// Close a Call window previously opened through this shell.
#[tauri::command]
pub fn close_call_window(app: AppHandle, label: String) -> Result<bool, String> {
    crate::call_window::close(&app, &label)
}

/// Reflect the account's unread total on the tray.
#[tauri::command]
pub fn set_unread(app: AppHandle, count: u64) -> Result<(), String> {
    crate::tray::set_unread(&app, count).map_err(|error| error.to_string())
}
