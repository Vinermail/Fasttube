//! Desktop integration: the tray icon, one window per user, autostart,
//! notifications on track change and the global hotkeys.

use crate::AppState;
use tauri::menu::MenuBuilder;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
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
            "show" => show_window(app),
            "quit" => app.exit(0),
            action => crate::media_action(app, action),
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
    let installed = autostart_enabled(app);

    if installed != enabled {
        let result = if enabled {
            manager.enable()
        } else {
            manager.disable()
        };
        if let Err(e) = result {
            eprintln!("[integration] cannot change autostart: {e}");
        }
        return;
    }
    if !enabled {
        return;
    }

    // The entry is already there and wants to be: the only thing that can be
    // stale is its command line. Builds before the tray registered the exe
    // without `--minimized`, so the app would open a window at login. Rewrite
    // the entry once — the user's choice stays as it is, only the argument of
    // the same registration is brought up to date.
    match autostart_command(app) {
        Some(command) if lacks_minimized(&command) => {
            if let Err(e) = manager.disable() {
                eprintln!("[integration] cannot rewrite autostart: {e}");
                return;
            }
            if let Err(e) = manager.enable() {
                eprintln!("[integration] cannot rewrite autostart: {e}");
                return;
            }
            eprintln!("[integration] autostart entry rewritten with --minimized");
        }
        Some(_) => {}
        None => eprintln!("[integration] the autostart entry could not be read"),
    }
}

/// Whether a registry command line starts the app without `--minimized`.
/// Compared by tokens, not by `contains`: a path may carry the word itself.
pub fn lacks_minimized(command: &str) -> bool {
    !command
        .split_whitespace()
        .any(|token| token.eq_ignore_ascii_case("--minimized"))
}

/// The command line of the Run entry the autostart plugin writes, if there is
/// one. The plugin names the value after the package name and writes
/// `"<exe>" <args>` into HKCU\...\Run.
#[cfg(windows)]
fn autostart_command(app: &AppHandle) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_NONE, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW,
    };

    const RUN_KEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run";

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let name = app.package_info().name.clone();
    let subkey = wide(RUN_KEY);

    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return None;
        }

        let value_name = wide(&name);
        let mut kind = REG_NONE;
        let mut size: u32 = 0;
        let mut command = None;

        if RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        ) == ERROR_SUCCESS
            && size > 1
        {
            // The size is in bytes and includes the terminating null.
            let mut buffer = vec![0u8; size as usize];
            if RegQueryValueExW(
                key,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                &mut kind,
                buffer.as_mut_ptr(),
                &mut size,
            ) == ERROR_SUCCESS
            {
                let units: Vec<u16> = buffer
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .take_while(|unit| *unit != 0)
                    .collect();
                command = Some(String::from_utf16_lossy(&units));
            }
        }

        RegCloseKey(key);
        command
    }
}

#[cfg(not(windows))]
fn autostart_command(_app: &AppHandle) -> Option<String> {
    None
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
        if let Err(e) = shortcuts.on_shortcut(combination, move |app, _shortcut, event| {
            if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                crate::media_action(app, action);
            }
        }) {
            eprintln!("[integration] hotkey {combination} is not available: {e}");
        }
    }
}

/// How long a burst of launches is collected before the paths are handed over.
/// Explorer starts one process per selected file, so ten files mean ten
/// processes a few dozen milliseconds apart.
pub const LAUNCH_WINDOW: Duration = Duration::from_millis(400);

/// Collects the paths of a burst of launches. No Tauri and no clock of its own:
/// the caller passes `now`, so the whole rule is testable.
#[derive(Debug, Default)]
pub struct LaunchBuffer {
    pending: Vec<String>,
    deadline: Option<Instant>,
}

impl LaunchBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the paths of one launch in arrival order, dropping anything already
    /// waiting. The deadline only moves when something new arrived: a burst of
    /// duplicates must not keep the list open forever.
    pub fn push(&mut self, now: Instant, window: Duration, paths: Vec<String>) -> usize {
        let mut added = 0;
        for path in paths {
            if path.is_empty() || self.pending.contains(&path) {
                continue;
            }
            self.pending.push(path);
            added += 1;
        }
        if added > 0 {
            self.deadline = Some(now + window);
        }
        added
    }

    /// The paths to hand over, if the window has closed. Clears the buffer.
    pub fn take_due(&mut self, now: Instant) -> Option<Vec<String>> {
        let deadline = self.deadline?;
        if now < deadline || self.pending.is_empty() {
            return None;
        }
        self.deadline = None;
        Some(std::mem::take(&mut self.pending))
    }

    /// How long the caller may wait before checking again; `None` means "nothing
    /// is waiting, block until the next launch".
    pub fn wait_time(&self, now: Instant) -> Option<Duration> {
        let deadline = self.deadline?;
        Some(deadline.saturating_duration_since(now))
    }

    #[cfg(test)]
    fn pending(&self) -> &[String] {
        &self.pending
    }
}

/// Appends without repeating: the slot may still hold paths the frontend has
/// not taken yet, and a second launch naming the same file must not queue it
/// twice.
pub fn append_unique(slot: &mut Vec<String>, paths: Vec<String>) -> usize {
    let mut added = 0;
    for path in paths {
        if path.is_empty() || slot.iter().any(|p| p == &path) {
            continue;
        }
        slot.push(path);
        added += 1;
    }
    added
}

/// Hands a finished burst to the frontend: the paths are appended to the slot
/// the page drains, then the page is told there is something to take.
fn deliver(app: &AppHandle, paths: Vec<String>) {
    let state = app.state::<crate::AppState>();
    let added = append_unique(&mut state.open_files.lock(), paths);
    if added > 0 {
        let _ = app.emit("open-files", ());
    }
}

/// The thread that owns the buffer. It waits for launches and for the end of
/// the current window, whichever comes first.
pub fn spawn_launch_collector(app: AppHandle, rx: Receiver<Vec<String>>) {
    let _ = std::thread::Builder::new()
        .name("launch-collector".to_string())
        .spawn(move || {
            let mut buffer = LaunchBuffer::new();
            loop {
                // A long wait stands for "until the next launch": the channel
                // wakes the thread before that anyway.
                let wait = buffer
                    .wait_time(Instant::now())
                    .unwrap_or(Duration::from_secs(3600));
                match rx.recv_timeout(wait) {
                    Ok(paths) => {
                        buffer.push(Instant::now(), LAUNCH_WINDOW, paths);
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(paths) = buffer.take_due(Instant::now()) {
                            deliver(&app, paths);
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
}

/// Files and folders passed on the command line (double click, "Open with",
/// a second launch). Every existing media file, in order: selecting ten songs
/// in Explorer and pressing Enter must queue ten songs, not play one.
pub fn media_arguments(args: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    for arg in args.iter().skip(1) {
        let trimmed = arg.trim_matches('"');
        if trimmed.starts_with('-') {
            continue;
        }
        let path = std::path::Path::new(trimmed);
        if path.is_file() && is_media(path) && !found.iter().any(|f: &String| f == trimmed) {
            found.push(trimmed.to_string());
        }
    }
    found
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
    fn a_burst_of_launches_becomes_one_list_in_arrival_order() {
        let window = Duration::from_millis(400);
        let t0 = Instant::now();
        let mut buffer = LaunchBuffer::new();

        assert_eq!(buffer.push(t0, window, vec!["a.mp3".into()]), 1);
        assert_eq!(
            buffer.push(t0 + Duration::from_millis(100), window, vec!["b.mp3".into()]),
            1
        );
        // The same file again from a third launch: queued once, no new window.
        assert_eq!(
            buffer.push(t0 + Duration::from_millis(200), window, vec!["a.mp3".into()]),
            0
        );
        assert_eq!(buffer.pending(), ["a.mp3", "b.mp3"]);

        // The window was extended by the second launch, not by the third.
        assert_eq!(buffer.take_due(t0 + Duration::from_millis(499)), None);
        assert_eq!(
            buffer.take_due(t0 + Duration::from_millis(500)),
            Some(vec!["a.mp3".to_string(), "b.mp3".to_string()])
        );
        assert_eq!(buffer.take_due(t0 + Duration::from_secs(2)), None);
        assert!(buffer.pending().is_empty());
    }

    #[test]
    fn a_quiet_buffer_blocks_until_a_launch_comes() {
        let buffer = LaunchBuffer::new();
        assert_eq!(buffer.wait_time(Instant::now()), None);
    }

    #[test]
    fn the_slot_never_repeats_a_path() {
        let mut slot = vec!["a.mp3".to_string()];
        assert_eq!(
            append_unique(&mut slot, vec!["a.mp3".into(), "b.mp3".into()]),
            1
        );
        assert_eq!(slot, ["a.mp3", "b.mp3"]);
    }

    #[test]
    fn an_entry_without_the_flag_needs_a_rewrite() {
        // What an older build wrote: the exe alone.
        assert!(lacks_minimized(r#""C:\Program Files\Fasttube\fasttube.exe""#));
        // What the plugin writes now.
        assert!(!lacks_minimized(
            r#""C:\Program Files\Fasttube\fasttube.exe" --minimized"#
        ));
        // The word inside a path is not the flag.
        assert!(lacks_minimized(r#""C:\a--minimized\fasttube.exe""#));
    }

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
        assert_eq!(media_arguments(&args), vec![song.to_string_lossy().to_string()]);
        assert!(media_arguments(&["fasttube.exe".to_string()]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
