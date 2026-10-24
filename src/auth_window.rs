//! Isolated YouTube sign-in window.
//!
//! Signing in to YouTube needs a real Google session, so the user types the
//! password into a page we do not control. That page runs in its own
//! incognito, storage-isolated webview with no Tauri capabilities: it cannot
//! call `invoke`, and its cookies are read by us through the WebView2 cookie
//! manager (HTTP-only cookies included), never through page JavaScript.
//!
//! The captured cookie header goes straight from here to the Python sidecar
//! and is never logged nor handed to the main window's JS.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tauri::webview::Cookie;
use tauri::{AppHandle, Emitter, Manager, Url, WebviewUrl, WebviewWindowBuilder};

use crate::downloader;

pub const AUTH_WINDOW: &str = "yt-auth";
const AUTH_URL: &str = "https://music.youtube.com/";
const EVENT: &str = "yt-auth-changed";
const POLL_EVERY: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(300);

/// Bumped on every `open`. A watcher whose generation is stale stops silently,
/// so closing and reopening the window cannot leave two watchers racing.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Payload of the `yt-auth-changed` event. Deliberately carries no cookie data.
#[derive(Clone, serde::Serialize)]
pub struct AuthEvent {
    pub ok: bool,
    pub cancelled: bool,
    pub error: String,
}

/// Opens the isolated sign-in window (or focuses it when already open) and
/// starts watching it for a completed session.
pub fn open(app: &AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(AUTH_WINDOW) {
        let _ = win.set_focus();
        return Ok(());
    }

    let url = Url::parse(AUTH_URL).map_err(|e| e.to_string())?;
    let profile = profile_dir();
    let _ = std::fs::create_dir_all(&profile);

    WebviewWindowBuilder::new(app, AUTH_WINDOW, WebviewUrl::External(url))
        .title("Sign in to YouTube Music")
        .inner_size(520.0, 760.0)
        .min_inner_size(420.0, 560.0)
        // InPrivate + a throwaway profile: session cookies die with the window
        // even on WebView2 versions that ignore the InPrivate flag.
        .incognito(true)
        .data_directory(profile)
        // Google's login flow bounces between its own domains; anything else
        // is a navigation we never want to allow.
        .on_navigation(|url| is_allowed(url))
        .build()
        .map_err(|e| format!("failed to open the sign-in window: {e}"))?;

    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move { watch(app, generation).await });
    Ok(())
}

/// Throwaway WebView2 profile, kept outside the asset-protocol cache dir.
fn profile_dir() -> std::path::PathBuf {
    downloader::data_dir().join("auth-profile")
}

fn is_allowed(url: &Url) -> bool {
    if url.scheme() == "about" {
        return true;
    }
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    const DOMAINS: [&str; 6] = [
        "youtube.com",
        "google.com",
        "googleusercontent.com",
        "gstatic.com",
        "ytimg.com",
        "ggpht.com",
    ];
    DOMAINS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

enum Outcome {
    SignedIn,
    Cancelled,
    Failed(String),
}

async fn watch(app: AppHandle, generation: u64) {
    let Ok(url) = Url::parse(AUTH_URL) else {
        return;
    };
    let started = Instant::now();
    let mut outcome = Outcome::Cancelled;

    loop {
        tokio::time::sleep(POLL_EVERY).await;
        if GENERATION.load(Ordering::SeqCst) != generation {
            return; // a newer window took over
        }

        let Some(win) = app.get_webview_window(AUTH_WINDOW) else {
            // Closed by the user before we saw a session.
            break;
        };
        if started.elapsed() > TIMEOUT {
            outcome = Outcome::Failed("sign-in timed out".to_string());
            break;
        }

        // `cookies_for_url` blocks on the event loop; this runs off the main
        // thread, which is what WebView2 requires (wry#583).
        let cookies = match win.cookies_for_url(url.clone()) {
            Ok(cookies) => cookies,
            Err(e) => {
                eprintln!("[auth] could not read cookies: {e}");
                continue;
            }
        };

        let Some(auth_text) = auth_text(&cookies) else {
            continue; // still on the sign-in page
        };

        outcome = match crate::grpc::set_browser_auth(&auth_text).await {
            Ok(resp) if resp.ok => Outcome::SignedIn,
            Ok(resp) => Outcome::Failed(resp.error),
            Err(e) => Outcome::Failed(format!("sidecar error: {e}")),
        };
        break;
    }

    if let Some(win) = app.get_webview_window(AUTH_WINDOW) {
        let _ = win.close();
    }
    cleanup_profile().await;

    let event = match outcome {
        Outcome::SignedIn => {
            // Make the next status poll show "Signed in" instead of waiting for
            // the sidecar health check to notice.
            mark_auth(&app, true);
            AuthEvent {
                ok: true,
                cancelled: false,
                error: String::new(),
            }
        }
        Outcome::Cancelled => AuthEvent {
            ok: false,
            cancelled: true,
            error: String::new(),
        },
        Outcome::Failed(error) => AuthEvent {
            ok: false,
            cancelled: false,
            error,
        },
    };
    let _ = app.emit(EVENT, event);
}

/// Builds the header block the sidecar expects, or `None` while the session
/// is not complete. The gate is `__Secure-3PAPISID`: it is only set on a
/// signed-in youtube.com session and it is the value ytmusicapi hashes into
/// `Authorization`.
fn auth_text(cookies: &[Cookie<'static>]) -> Option<String> {
    // The same name can exist for several domains; prefer the youtube.com one.
    let mut by_name: HashMap<&str, (u8, &str)> = HashMap::new();
    for cookie in cookies {
        let name = cookie.name();
        let rank = domain_rank(cookie.domain().unwrap_or(""));
        let better = by_name
            .get(name)
            .is_none_or(|(current, _)| rank > *current);
        if better {
            by_name.insert(name, (rank, cookie.value()));
        }
    }
    by_name.get("__Secure-3PAPISID")?;

    let mut names: Vec<&str> = by_name.keys().copied().collect();
    names.sort_unstable();
    let cookie_header = names
        .iter()
        .map(|n| format!("{n}={}", by_name[n].1))
        .collect::<Vec<_>>()
        .join("; ");

    Some(format!(
        "Cookie: {cookie_header}\n\
         X-Goog-AuthUser: 0\n\
         Origin: https://music.youtube.com\n\
         X-Origin: https://music.youtube.com\n\
         Referer: https://music.youtube.com/"
    ))
}

fn domain_rank(domain: &str) -> u8 {
    if domain == "youtube.com" || domain.ends_with(".youtube.com") {
        2
    } else if domain == "google.com" || domain.ends_with(".google.com") {
        1
    } else {
        0
    }
}

/// Keeps the shared status in step with a session change so the UI does not
/// flip back to "Not signed in" until the next health poll.
fn mark_auth(app: &AppHandle, ok: bool) {
    if let Some(state) = app.try_state::<crate::AppState>()
        && let Ok(mut status) = state.api_status.write()
    {
        status.auth_ok = ok;
    }
}

/// Removes the throwaway profile once the webview is gone. WebView2 may hold
/// files for a moment, so retry briefly; a leftover directory is harmless
/// (cookies there were in-memory anyway), so failures are not fatal.
async fn cleanup_profile() {
    let dir = profile_dir();
    for _ in 0..5 {
        if !dir.exists() {
            return;
        }
        if std::fs::remove_dir_all(&dir).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    eprintln!("[auth] could not remove {}", dir.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &'static str, value: &'static str, domain: &'static str) -> Cookie<'static> {
        Cookie::build((name, value)).domain(domain).build()
    }

    #[test]
    fn auth_text_requires_sapisid() {
        let cookies = vec![cookie("SID", "s", ".youtube.com")];
        assert!(auth_text(&cookies).is_none());
    }

    #[test]
    fn auth_text_prefers_youtube_domain_and_sorts_names() {
        let cookies = vec![
            cookie("__Secure-3PAPISID", "yt-value", ".youtube.com"),
            cookie("__Secure-3PAPISID", "google-value", ".google.com"),
            cookie("SID", "sid", ".youtube.com"),
        ];
        let text = auth_text(&cookies).expect("session");
        assert!(text.contains("__Secure-3PAPISID=yt-value"));
        assert!(text.contains("SID=sid"));
        assert!(text.contains("X-Goog-AuthUser: 0"));
        assert!(text.starts_with("Cookie: "));
    }

    #[test]
    fn navigation_allowlist() {
        let ok = |u: &str| is_allowed(&Url::parse(u).unwrap());
        assert!(ok("https://music.youtube.com/"));
        assert!(ok("https://accounts.google.com/v3/signin"));
        assert!(ok("https://consent.youtube.com/m"));
        assert!(ok("https://www.gstatic.com/x"));
        assert!(!ok("http://music.youtube.com/"));
        assert!(!ok("https://evil.example.com/"));
        assert!(!ok("https://youtube.com.evil.example/"));
    }
}
