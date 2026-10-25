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
const TIMEOUT: Duration = Duration::from_secs(600);
/// How many times the captured session is pushed to the sidecar before the
/// window is closed for good.
const INSTALL_ATTEMPTS: usize = 3;
const INSTALL_RETRY: Duration = Duration::from_millis(2500);

/// User agent of the sign-in window, built from the WebView2 version the app is
/// actually running, so the cookies and the sidecar's later requests come from
/// the same client without pinning a Chrome version in the source.
///
/// `None` means "leave the webview's own UA alone" (the bundle then carries an
/// empty UA and the sidecar falls back to ytmusicapi's default).
fn session_user_agent() -> Option<String> {
    user_agent_for(&tauri::webview_version().ok()?)
}

/// Reduced-form UA for a WebView2 version string.
///
/// WebView2 reports the full version (155.0.4283.62) but the UA a browser
/// sends for this engine carries only the major, zero-padded:
/// `Chrome/155.0.0.0 ... Edg/155.0.0.0`. A full version there would look like
/// a spoofed client to Google.
fn user_agent_for(version: &str) -> Option<String> {
    let major = version
        .split('.')
        .next()
        .filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_digit()))?;
    Some(format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36 Edg/{major}.0.0.0"
    ))
}

/// Bumped on every `open`. A watcher whose generation is stale stops silently,
/// so closing and reopening the window cannot leave two watchers racing.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Payload of the `yt-auth-changed` event. Deliberately carries no cookie data.
#[derive(Clone, serde::Serialize)]
pub struct AuthEvent {
    pub ok: bool,
    pub cancelled: bool,
    pub error: String,
    pub state: String,
    pub account_name: String,
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

    let user_agent = session_user_agent();
    let mut builder = WebviewWindowBuilder::new(app, AUTH_WINDOW, WebviewUrl::External(url))
        .title("Sign in to YouTube Music")
        .inner_size(520.0, 760.0)
        .min_inner_size(420.0, 560.0)
        // InPrivate + a throwaway profile: session cookies die with the window
        // even on WebView2 versions that ignore the InPrivate flag.
        .incognito(true)
        .data_directory(profile)
        // Google's login flow bounces between its own domains; anything else
        // is a navigation we never want to allow.
        .on_navigation(|url| is_allowed(url));
    if let Some(ua) = user_agent.as_deref() {
        builder = builder.user_agent(ua);
    }
    builder
        .build()
        .map_err(|e| format!("failed to open the sign-in window: {e}"))?;

    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move { watch(app, generation, user_agent).await });
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
    SignedIn(crate::SessionInfo),
    Cancelled,
    Blocked,
    Failed(String),
}

async fn watch(app: AppHandle, generation: u64, user_agent: Option<String>) {
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
            outcome = Outcome::Failed(
                "sign-in timed out after 10 minutes; try again or use Advanced".to_string(),
            );
            break;
        }

        // Google refuses sign-in from some embedded browsers and lands on a
        // "rejected" page instead of failing silently.
        if let Ok(current) = win.url() {
            let href = current.as_str().to_ascii_lowercase();
            if href.contains("deniedsignin") || href.contains("signin/rejected") {
                outcome = Outcome::Blocked;
                break;
            }
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

        let Some(cookie) = cookie_header(&cookies) else {
            continue; // still on the sign-in page
        };

        let bundle = crate::secrets::AuthBundle::new(
            cookie,
            user_agent.clone().unwrap_or_default(),
            "0".to_string(),
        );
        outcome = match install_with_retries(&app, &bundle).await {
            Ok(info) => Outcome::SignedIn(info),
            Err(e) => Outcome::Failed(e),
        };
        break;
    }

    // Wipe the browsing data before the window goes away: the throwaway
    // profile is deleted right after too, but this stops a live YouTube
    // session from rotating cookies we already captured.
    if let Some(win) = app.get_webview_window(AUTH_WINDOW) {
        let _ = win.clear_all_browsing_data();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = win.close();
    }
    cleanup_profile().await;

    let event = match outcome {
        Outcome::SignedIn(info) => AuthEvent {
            ok: true,
            cancelled: false,
            error: String::new(),
            state: info.state,
            account_name: info.account_name,
        },
        Outcome::Cancelled => AuthEvent {
            ok: false,
            cancelled: true,
            error: String::new(),
            state: String::new(),
            account_name: String::new(),
        },
        Outcome::Blocked => AuthEvent {
            ok: false,
            cancelled: false,
            error: "Google refused the sign-in in the embedded browser \
                    (\"This browser or app may not be secure\"). Open Advanced \
                    and paste the request headers from a normal browser instead."
                .to_string(),
            state: String::new(),
            account_name: String::new(),
        },
        Outcome::Failed(error) => AuthEvent {
            ok: false,
            cancelled: false,
            error,
            state: String::new(),
            account_name: String::new(),
        },
    };
    let _ = app.emit(EVENT, event);
}

/// Pushes the captured session to the sidecar, retrying a couple of times.
///
/// The first attempt right after the cookie is captured can lose a race (the
/// sidecar may still be finishing the previous session, or the account endpoint
/// answers slowly), and a retry costs a few seconds instead of a full re-login.
async fn install_with_retries(
    app: &AppHandle,
    bundle: &crate::secrets::AuthBundle,
) -> Result<crate::SessionInfo, String> {
    let mut last = String::new();
    for attempt in 1..=INSTALL_ATTEMPTS {
        match crate::install_session(app, bundle.clone()).await {
            Ok(info) if info.state == "active" => return Ok(info),
            Ok(info) => {
                last = if info.error.is_empty() {
                    format!("session state: {}", info.state)
                } else {
                    info.error
                };
            }
            Err(e) => last = e,
        }
        if attempt < INSTALL_ATTEMPTS {
            eprintln!(
                "[auth] session install failed ({last}); retrying {attempt}/{INSTALL_ATTEMPTS}"
            );
            tokio::time::sleep(INSTALL_RETRY).await;
        }
    }
    Err(last)
}

/// Cookie pairs for the captured session, or `None` while the session is not
/// complete. The gate is `__Secure-3PAPISID`: it is only set on a signed-in
/// youtube.com session and it is the value ytmusicapi hashes into
/// `Authorization`.
fn cookie_header(cookies: &[Cookie<'static>]) -> Option<String> {
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
    Some(
        names
            .iter()
            .map(|n| format!("{n}={}", by_name[n].1))
            .collect::<Vec<_>>()
            .join("; "),
    )
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
    fn cookie_header_requires_sapisid() {
        let cookies = vec![cookie("SID", "s", ".youtube.com")];
        assert!(cookie_header(&cookies).is_none());
    }

    #[test]
    fn cookie_header_prefers_youtube_domain_and_sorts_names() {
        let cookies = vec![
            cookie("__Secure-3PAPISID", "yt-value", ".youtube.com"),
            cookie("__Secure-3PAPISID", "google-value", ".google.com"),
            cookie("SID", "sid", ".youtube.com"),
        ];
        let text = cookie_header(&cookies).expect("session");
        assert!(text.contains("__Secure-3PAPISID=yt-value"));
        assert!(text.contains("SID=sid"));
        assert!(!text.contains("google-value"));
        assert_eq!(text.matches("; ").count(), 1, "one separator for two pairs");
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

    #[test]
    fn user_agent_keeps_only_the_major_version() {
        let ua = user_agent_for("155.0.4283.62").expect("v4 version");
        assert!(ua.contains("Chrome/155.0.0.0 "), "{ua}");
        assert!(ua.ends_with("Edg/155.0.0.0"), "{ua}");
        assert!(!ua.contains("4283"), "build number must not leak: {ua}");
    }

    #[test]
    fn user_agent_rejects_a_version_without_a_major() {
        assert!(user_agent_for("").is_none());
        assert!(user_agent_for("WebView2/1.0").is_none());
        assert!(user_agent_for("abc.1.2.3").is_none());
    }
}
