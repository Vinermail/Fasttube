//! Desktop integration: the tray icon, one window per user, autostart,
//! notifications on track change and the global hotkeys.

use crate::AppState;
use tauri::menu::MenuBuilder;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

/// The tray icon and its menu. Left click shows or hides the window, the menu
/// carries the same commands as the player bar plus the way out.
pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let menu = MenuBuilder::new(app)
        .text("toggle", "Играть / Пауза")
        .text("next", "Следующий")
        .text("prev", "Предыдущий")
        .separator()
        .text("show", "Показать окно")
        .separator()
        .text("quit", "Выход")
        .build()?;

    let mut builder = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .tooltip("Fasttube")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle" => {
                let _ = app.emit("media-control", "toggle");
            }
            "next" => {
                let _ = app.emit("media-control", "next");
            }
            "prev" => {
                let _ = app.emit("media-control", "prev");
            }
            "show" => show_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Tray click behaviour: hide what is visible, reveal what is not.
pub fn toggle_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let visible = window.is_visible().unwrap_or(false);
    let minimized = window.is_minimized().unwrap_or(false);
    if visible && !minimized {
        let _ = window.hide();
    } else {
        show_window(app);
    }
}

/// One line in the Windows notification area when the track changes; off by
/// default, and only while the window is not in front of the user.
pub fn notify_track_change(app: &AppHandle, title: &str, artist: &str) {
    if title.is_empty() {
        return;
    }
    if !app.state::<AppState>().settings.lock().notify_track_change {
        return;
    }
    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title(title)
        .body(if artist.is_empty() { "Fasttube" } else { artist })
        .show();
}

/// Autostart follows the setting; a failure is a log line, not a crash.
pub fn apply_autostart(app: &AppHandle, enabled: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    if let Err(e) = result {
        eprintln!("[integration] cannot change autostart: {e}");
    }
}

pub fn autostart_enabled(app: &AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Register or drop the three global hotkeys. Fixed combinations, off by
/// default: they are the ones that do not clash with a browser.
pub fn apply_hotkeys(app: &AppHandle, enabled: bool) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let shortcuts = app.global_shortcut();
    let _ = shortcuts.unregister_all();
    if !enabled {
        return;
    }
    for (combination, action) in [
        ("Control+Alt+Space", "toggle"),
        ("Control+Alt+Right", "next"),
        ("Control+Alt+Left", "prev"),
    ] {
        let app = app.clone();
        if let Err(e) = shortcuts.on_shortcut(combination, move |_app, _shortcut, event| {
            if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                let _ = app.emit("media-control", action);
            }
        }) {
            eprintln!("[integration] hotkey {combination} is not available: {e}");
        }
    }
}

/// Files and folders passed on the command line (double click, "Open with",
/// a second launch). Returns the first existing media file.
pub fn media_argument(args: &[String]) -> Option<String> {
    for arg in args.iter().skip(1) {
        let trimmed = arg.trim_matches('"');
        if trimmed.starts_with('-') {
            continue;
        }
        let path = std::path::Path::new(trimmed);
        if path.is_file() && is_media(path) {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn is_media(path: &std::path::Path) -> bool {
    const EXTENSIONS: [&str; 7] = ["mp3", "flac", "m4a", "ogg", "wav", "opus", "webm"];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_media_file_is_picked_out_of_the_arguments() {
        let dir = std::env::temp_dir().join(format!("ft-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.flac");
        std::fs::write(&song, b"x").unwrap();
        let other = dir.join("cover.jpg");
        std::fs::write(&other, b"x").unwrap();

        let args = vec![
            "fasttube.exe".to_string(),
            "--flag".to_string(),
            other.to_string_lossy().to_string(),
            format!("\"{}\"", song.to_string_lossy()),
        ];
        assert_eq!(media_argument(&args), Some(song.to_string_lossy().to_string()));
        assert_eq!(media_argument(&["fasttube.exe".to_string()]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
