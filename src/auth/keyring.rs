use anyhow::{Context, Result};
use std::path::PathBuf;

const SERVICE_NAME: &str = crate::branding::APP_NAME;
const GITHUB_TOKEN_KEY: &str = "github-token";

/// Store a GitHub token. Uses file storage (with 0600 permissions) as the
/// primary backend, with system keyring as an additional store when available.
pub fn store_token(token: &str) -> Result<()> {
    // Always store to file (reliable across all platforms)
    store_in_file(token)?;

    // Also try keyring as a bonus
    match store_in_keyring(token) {
        Ok(()) => tracing::debug!("Also stored token in system keyring"),
        Err(e) => tracing::debug!("Keyring store skipped: {}", e),
    }

    Ok(())
}

/// Retrieve a GitHub token. Checks file first, then keyring.
pub fn retrieve_token() -> Result<Option<String>> {
    // File is primary
    match retrieve_from_file() {
        Ok(Some(token)) => {
            tracing::debug!("Retrieved GitHub token from file");
            return Ok(Some(token));
        }
        Ok(None) => {}
        Err(e) => tracing::debug!("File retrieval failed: {}", e),
    }

    // Keyring as fallback
    match retrieve_from_keyring() {
        Ok(Some(token)) => {
            tracing::debug!("Retrieved GitHub token from system keyring");
            // Migrate to file for next time
            let _ = store_in_file(&token);
            return Ok(Some(token));
        }
        Ok(None) => {}
        Err(e) => tracing::debug!("Keyring retrieval failed: {}", e),
    }

    Ok(None)
}

/// Delete the stored GitHub token from both file and keyring.
pub fn delete_token() -> Result<()> {
    let _ = delete_from_file();
    let _ = delete_from_keyring();
    tracing::info!("Deleted stored GitHub token");
    Ok(())
}

// --- Keyring backend ---

fn store_in_keyring(token: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE_NAME, GITHUB_TOKEN_KEY)
        .context("Failed to create keyring entry")?;
    entry
        .set_password(token)
        .context("Failed to store token in keyring")?;
    Ok(())
}

fn retrieve_from_keyring() -> Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE_NAME, GITHUB_TOKEN_KEY)
        .context("Failed to create keyring entry")?;
    match entry.get_password() {
        Ok(token) if !token.is_empty() => Ok(Some(token)),
        Ok(_) => {
            tracing::info!("Keyring returned empty token");
            Ok(None)
        }
        Err(keyring::Error::NoEntry) => {
            tracing::info!("Keyring: NoEntry");
            Ok(None)
        }
        Err(e) => {
            tracing::warn!("Keyring get_password error: {}", e);
            Err(anyhow::anyhow!("Keyring error: {}", e))
        }
    }
}

fn delete_from_keyring() -> Result<()> {
    let entry = keyring::Entry::new(SERVICE_NAME, GITHUB_TOKEN_KEY)
        .context("Failed to create keyring entry")?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("Keyring delete error: {}", e)),
    }
}

// --- File fallback backend ---

fn token_file_path() -> PathBuf {
    crate::config::data_dir().join("github-token")
}

fn store_in_file(token: &str) -> Result<()> {
    let path = token_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;
    }
    std::fs::write(&path, token).with_context(|| format!("Failed to write token to {:?}", path))?;

    // Set file permissions to 0600 on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set permissions on {:?}", path))?;
    }

    tracing::info!("Stored GitHub token in {:?}", path);
    Ok(())
}

fn retrieve_from_file() -> Result<Option<String>> {
    let path = token_file_path();
    if !path.exists() {
        return Ok(None);
    }
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read token from {:?}", path))?
        .trim()
        .to_string();
    if token.is_empty() {
        Ok(None)
    } else {
        Ok(Some(token))
    }
}

fn delete_from_file() -> Result<()> {
    let path = token_file_path();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("Failed to delete {:?}", path))?;
    }
    Ok(())
}

// --- Generic secrets (OS keyring only, no file fallback) ---

/// Store a secret under `key` in the OS keyring. Unlike the GitHub token this
/// never falls back to a file: callers use it for material that must not
/// land on disk in the clear.
pub fn store_secret(key: &str, value: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE_NAME, key).context("Failed to create keyring entry")?;
    entry
        .set_password(value)
        .context("Failed to store secret in keyring")?;
    Ok(())
}

pub fn retrieve_secret(key: &str) -> Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE_NAME, key).context("Failed to create keyring entry")?;
    match entry.get_password() {
        Ok(v) if !v.is_empty() => Ok(Some(v)),
        Ok(_) => Ok(None),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Keyring error: {}", e)),
    }
}

pub fn delete_secret(key: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE_NAME, key).context("Failed to create keyring entry")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("Keyring delete error: {}", e)),
    }
}
