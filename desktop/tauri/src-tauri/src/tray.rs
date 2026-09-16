//! The system tray: minimise-to-tray, unread state, and the Quit path.
//!
//! The tray is built from a menu that is **rebuilt** whenever the unread total
//! changes. Rebuilding rather than mutating one item in place keeps the code
//! free of menu-id lookups, and the menu is three items long — the cost is not a
//! cost at all.

use tauri::{
    menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Runtime,
};

/// Label of the main application window, as declared in `tauri.conf.json`.
pub const MAIN_WINDOW: &str = "main";

const TRAY_ID: &str = "jiuyue-tray";
const OPEN_ID: &str = "tray-open";
const UNREAD_ID: &str = "tray-unread";
const QUIT_ID: &str = "tray-quit";

/// The tray's unread line. A zero total is a real state, not an empty label.
fn unread_label(total: u64) -> String {
    match total {
        0 => "暂无未读".to_string(),
        total => format!("{total} 条未读"),
    }
}

/// The tray tooltip, which is what a user sees without opening the menu.
fn tooltip(total: u64) -> String {
    match total {
        0 => "jiuyue".to_string(),
        total => format!("jiuyue — {total} 条未读"),
    }
}

/// Build the tray menu for a given unread total.
fn menu_for<R: Runtime>(app: &AppHandle<R>, total: u64) -> tauri::Result<Menu<R>> {
    let open = MenuItem::with_id(app, OPEN_ID, "打开 jiuyue", true, None::<&str>)?;
    // The unread line is informational: it is never clickable.
    let unread = MenuItem::with_id(app, UNREAD_ID, unread_label(total), false, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT_ID, "退出", true, None::<&str>)?;

    let items: [&dyn IsMenuItem<R>; 5] = [
        &open,
        &PredefinedMenuItem::separator(app)?,
        &unread,
        &PredefinedMenuItem::separator(app)?,
        &quit,
    ];

    Menu::with_items(app, &items)
}

/// Bring the main window back from the tray.
pub fn show_main<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Create the tray icon and its menu.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let menu = menu_for(app, 0)?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(tooltip(0))
        .menu(&menu)
        // A left click restores the window instead of opening the menu; the menu
        // is the right-click action, which is the platform convention.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            OPEN_ID => show_main(app),
            QUIT_ID => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    builder.build(app)?;
    Ok(())
}

/// Reflect the account's unread total on the tray.
///
/// `0` clears the state rather than hiding it: "暂无未读" is information the
/// user can act on, while a vanished menu line is not.
pub fn set_unread<R: Runtime>(app: &AppHandle<R>, total: u64) -> tauri::Result<()> {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return Ok(());
    };

    tray.set_tooltip(Some(tooltip(total)))?;
    tray.set_menu(Some(menu_for(app, total)?))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_label_states_the_count() {
        assert_eq!(unread_label(1), "1 条未读");
        assert_eq!(unread_label(128), "128 条未读");
    }

    #[test]
    fn unread_label_is_explicit_at_zero() {
        assert_eq!(unread_label(0), "暂无未读");
    }

    #[test]
    fn tooltip_omits_the_count_at_zero() {
        assert_eq!(tooltip(0), "jiuyue");
    }

    #[test]
    fn tooltip_carries_the_count_when_there_is_one() {
        assert_eq!(tooltip(3), "jiuyue — 3 条未读");
    }
}
