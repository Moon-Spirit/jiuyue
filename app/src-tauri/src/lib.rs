//! JiuYue desktop shell.
//!
//! Exposes Tauri commands that persist the refresh token OUTSIDE the webview's
//! localStorage: on Windows the token is DPAPI-encrypted (`CryptProtectData`,
//! current-user scope) before it touches disk; other platforms fall back to a
//! plain file with 0600 permissions (weaker guarantee, documented below).
//! Token material is never logged anywhere in this crate.

use std::fs;
use std::path::{Path, PathBuf};
use tauri::Manager;

/// File name inside the OS app-data directory (`app_data_dir()`); the
/// directory itself is resolved per-platform, never hardcoded.
const TOKEN_FILE_NAME: &str = "tokens.bin";

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

fn token_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve app data dir: {error}"))?;
    Ok(dir.join(TOKEN_FILE_NAME))
}

fn ensure_parent_dir(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(parent) => fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create app data dir: {error}")),
        None => Err("token file path has no parent directory".to_string()),
    }
}

// ---------------------------------------------------------------------------
// At-rest protection primitives
// ---------------------------------------------------------------------------

/// Windows: DPAPI `CryptProtectData` scoped to the current user. The stored
/// bytes can only be decrypted by the same Windows account, so the file is
/// inert when copied or read by any other user/process context.
#[cfg(windows)]
mod at_rest {
    use base64::Engine as _;
    use windows::core::w;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// Encrypt `plain` with DPAPI and return base64 text safe to write to disk.
    pub(super) fn protect_to_base64(plain: &[u8]) -> Result<String, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        unsafe {
            CryptProtectData(
                &input,
                w!("jiuyue.refresh"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .map_err(|error| format!("CryptProtectData failed: {error}"))?;
        }
        // Copy out before releasing the DPAPI-allocated buffer.
        let encrypted =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            LocalFree(Some(HLOCAL(output.pbData.cast())));
        }
        Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
    }

    /// Reverse of [`protect_to_base64`].
    pub(super) fn unprotect_from_base64(stored: &[u8]) -> Result<Vec<u8>, String> {
        let encrypted = base64::engine::general_purpose::STANDARD
            .decode(stored)
            .map_err(|error| format!("stored token blob is not valid base64: {error}"))?;
        let input = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .map_err(|error| format!("CryptUnprotectData failed: {error}"))?;
        }
        let plain =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            LocalFree(Some(HLOCAL(output.pbData.cast())));
        }
        Ok(plain)
    }
}

/// Non-Windows fallback: stores the token verbatim (base64-wrapped) in a file
/// with 0600 permissions. WEAKER GUARANTEE than DPAPI: any process running as
/// the same OS user can read it. Parity hardening (macOS Keychain /
/// libsecret) is future work; this exists only so the crate stays
/// cross-platform-compilable.
#[cfg(not(windows))]
mod at_rest {
    use base64::Engine as _;

    pub(super) fn protect_to_base64(plain: &[u8]) -> Result<String, String> {
        Ok(base64::engine::general_purpose::STANDARD.encode(plain))
    }

    pub(super) fn unprotect_from_base64(stored: &[u8]) -> Result<Vec<u8>, String> {
        base64::engine::general_purpose::STANDARD
            .decode(stored)
            .map_err(|error| format!("stored token blob is not valid base64: {error}"))
    }
}

#[cfg(windows)]
fn write_token_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    // The file lives under %APPDATA%\app.jiuyue.desktop, whose profile
    // directories carry a user-only DACL by default; contents are additionally
    // DPAPI ciphertext, so no plaintext ever reaches disk regardless of ACLs.
    fs::write(path, bytes).map_err(|error| format!("failed to write token file: {error}"))
}

#[cfg(not(windows))]
fn write_token_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let mut file =
        fs::File::create(path).map_err(|error| format!("failed to write token file: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("failed to write token file: {error}"))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("failed to restrict token file permissions: {error}"))
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Persist the refresh token: DPAPI-encrypt (Windows), then base64 into
/// `{app_data_dir}/tokens.bin`.
#[tauri::command]
fn secure_store(app: tauri::AppHandle, token: String) -> Result<(), String> {
    let path = token_file_path(&app)?;
    ensure_parent_dir(&path)?;
    let stored = at_rest::protect_to_base64(token.as_bytes())?;
    write_token_file(&path, stored.as_bytes())
}

/// Load the refresh token previously written by [`secure_store`], if any.
#[tauri::command]
fn secure_load(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = token_file_path(&app)?;
    if !path.exists() {
        return Ok(None);
    }
    let stored = fs::read(&path).map_err(|error| format!("failed to read token file: {error}"))?;
    let plain = at_rest::unprotect_from_base64(&stored)?;
    String::from_utf8(plain)
        .map(Some)
        .map_err(|_| "stored token is not valid UTF-8".to_string())
}

/// Delete the persisted refresh token (logout). Missing file counts as cleared.
#[tauri::command]
fn secure_clear(app: tauri::AppHandle) -> Result<(), String> {
    let path = token_file_path(&app)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to remove token file: {error}")),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            secure_store,
            secure_load,
            secure_clear
        ])
        .run(tauri::generate_context!())
        .expect("failed to run the JiuYue desktop shell");
}
