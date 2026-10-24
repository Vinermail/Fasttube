//! Encrypted-at-rest storage for the YouTube session bundle.
//!
//! On Windows the bundle is sealed with DPAPI (`CryptProtectData`) under a
//! fixed application entropy value: the ciphertext can only be opened by the
//! same Windows user on the same machine, and the plaintext cookies never hit
//! the disk. Nothing in this module logs secret material.
//!
//! Other platforms: `secrets` returns an error. The app is Windows-only today
//! (LOCALAPPDATA layout, cpal output), and silently writing cookies unencrypted
//! there would be worse than refusing.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// File magic, so a stray/foreign file is rejected instead of fed to DPAPI.
const MAGIC: &[u8; 4] = b"FTB1";
/// Extra entropy: a blob copied from another app cannot be unprotected here.
const ENTROPY: &[u8] = b"fasttube-auth-bundle-v1";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthBundle {
    /// Raw `Cookie:` header value captured from the login webview.
    pub cookie: String,
    pub user_agent: String,
    pub x_goog_authuser: String,
    /// Unix seconds.
    pub captured_at: i64,
}

impl AuthBundle {
    pub fn new(cookie: String, user_agent: String, x_goog_authuser: String) -> Self {
        Self {
            cookie,
            user_agent,
            x_goog_authuser,
            captured_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        }
    }
}

pub fn path() -> PathBuf {
    crate::downloader::data_dir().join("auth.bin")
}

pub fn save(bundle: &AuthBundle) -> Result<(), String> {
    save_to(&path(), bundle)
}

/// Returns `None` when there is no bundle or it cannot be opened (wrong user,
/// corrupted file, foreign magic). Callers treat that as "signed out".
pub fn load() -> Option<AuthBundle> {
    load_from(&path())
}

pub fn clear() -> Result<(), String> {
    let path = path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

fn save_to(path: &Path, bundle: &AuthBundle) -> Result<(), String> {
    let json = serde_json::to_vec(bundle).map_err(|e| format!("serialize bundle: {e}"))?;
    let sealed = protect(&json)?;

    let mut blob = Vec::with_capacity(MAGIC.len() + sealed.len());
    blob.extend_from_slice(MAGIC);
    blob.extend_from_slice(&sealed);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    // Write-then-rename so a crash cannot leave a half-written auth.bin.
    let tmp = path.with_extension("bin.tmp");
    {
        let mut file =
            std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        file.write_all(&blob)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("replace {}: {e}", path.display()))?;
    Ok(())
}

fn load_from(path: &Path) -> Option<AuthBundle> {
    let blob = std::fs::read(path).ok()?;
    let sealed = blob.strip_prefix(&MAGIC[..])?;
    let json = unprotect(sealed).ok()?;
    serde_json::from_slice(&json).ok()
}

// ------------------------------------------------------------------ Windows

#[cfg(windows)]
fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB, CryptProtectData,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptProtectData failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData as *mut core::ffi::c_void);
    }
    Ok(bytes)
}

#[cfg(windows)]
fn unprotect(sealed: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB, CryptUnprotectData,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: sealed.len() as u32,
        pbData: sealed.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptUnprotectData failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        LocalFree(output.pbData as *mut core::ffi::c_void);
    }
    Ok(bytes)
}

#[cfg(not(windows))]
fn protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
    Err("encrypted secret storage is implemented for Windows (DPAPI) only".to_string())
}

#[cfg(not(windows))]
fn unprotect(_sealed: &[u8]) -> Result<Vec<u8>, String> {
    Err("encrypted secret storage is implemented for Windows (DPAPI) only".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fasttube-secrets-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("auth.bin")
    }

    fn bundle() -> AuthBundle {
        AuthBundle {
            cookie: "__Secure-3PAPISID=test-value; SID=other".to_string(),
            user_agent: "Mozilla/5.0 test".to_string(),
            x_goog_authuser: "0".to_string(),
            captured_at: 1_700_000_000,
        }
    }

    #[test]
    fn bundle_round_trips_through_dpapi() {
        let path = temp_path("roundtrip");
        save_to(&path, &bundle()).unwrap();
        assert_eq!(load_from(&path).unwrap(), bundle());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn file_does_not_contain_plaintext_cookie() {
        let path = temp_path("opaque");
        save_to(&path, &bundle()).unwrap();
        let raw = std::fs::read(&path).unwrap();
        for needle in [b"__Secure-3PAPISID".as_slice(), b"test-value".as_slice()] {
            assert!(
                !raw.windows(needle.len()).any(|w| w == needle),
                "plaintext cookie leaked into auth.bin"
            );
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn foreign_or_truncated_file_is_ignored() {
        let path = temp_path("junk");
        std::fs::write(&path, b"not a fasttube bundle").unwrap();
        assert!(load_from(&path).is_none());
        std::fs::write(&path, b"FTB1garbage").unwrap();
        assert!(load_from(&path).is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
