//! Opening a link in the user's default browser.
//!
//! Only https URLs on a short allowlist are accepted: the main window is a
//! webview, so an injected script could otherwise ask the OS to open any
//! `file:`/`javascript:`/custom-protocol target it likes (ShellExecuteW would
//! happily oblige).

/// Pages the sign-in flow may send the user to.
const ALLOWED_HOSTS: [&str; 3] = [
    "music.youtube.com",
    "accounts.google.com",
    "www.youtube.com",
];

/// Checks the URL without touching the OS (also what the tests use).
pub fn validate(url: &str) -> Result<(), String> {
    if url.chars().any(|c| c.is_control()) {
        return Err("URL contains control characters".to_string());
    }
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "only https links can be opened".to_string())?;
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| "link has no host".to_string())?;
    // Strip `user@` and `:port`, then compare the host exactly: a suffix match
    // would let `music.youtube.com.evil.test` through.
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if ALLOWED_HOSTS.contains(&host.as_str()) {
        Ok(())
    } else {
        Err(format!("refusing to open {host:?}"))
    }
}

/// Opens `url` in the default browser.
pub fn open(url: &str) -> Result<(), String> {
    validate(url)?;
    open_native(url)
}

#[cfg(windows)]
fn open_native(url: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let verb = wide("open");
    let target = wide(url);
    // SAFETY: both strings are NUL-terminated and outlive the call; no other
    // pointers are passed.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW reports success as a "instance handle" greater than 32.
    if result as isize > 32 {
        Ok(())
    } else {
        Err(format!(
            "the default browser could not be started (ShellExecuteW returned {})",
            result as isize
        ))
    }
}

#[cfg(target_os = "macos")]
fn open_native(url: &str) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot start the browser: {e}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_native(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot start the browser: {e}"))
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn allows_the_sign_in_pages() {
        assert!(validate("https://music.youtube.com/").is_ok());
        assert!(validate("https://accounts.google.com/ServiceLogin?continue=x").is_ok());
        assert!(validate("https://www.youtube.com/watch?v=abc").is_ok());
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(validate("http://music.youtube.com/").is_err());
        assert!(validate("javascript:alert(1)").is_err());
        assert!(validate("file:///C:/Windows/System32/cmd.exe").is_err());
        assert!(validate("ms-settings:privacy").is_err());
    }

    #[test]
    fn rejects_lookalike_hosts() {
        assert!(validate("https://music.youtube.com.evil.test/").is_err());
        assert!(validate("https://evil.test/music.youtube.com").is_err());
        assert!(validate("https://music.youtube.com@evil.test/").is_err());
        assert!(validate("https://music.youtube.co/").is_err());
    }

    #[test]
    fn rejects_control_characters() {
        assert!(validate("https://music.youtube.com/\n").is_err());
        assert!(validate("https://music.youtube.com/\r\nX").is_err());
    }
}
