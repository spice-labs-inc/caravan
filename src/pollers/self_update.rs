//! Downloads, verifies, and replaces the running binary with a new version.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Download, verify checksum, and replace the current binary.
/// Returns the path to the new binary on success.
pub async fn download_and_replace(
    download_url: &str,
    checksum_url: Option<&str>,
    asset_name: &str,
    github_token: &str,
) -> Result<PathBuf> {
    let client = reqwest::Client::builder()
        .user_agent("caravan-self-update")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;

    // 1. Download the binary/archive via GitHub API asset URL
    //    The API URL (https://api.github.com/repos/.../releases/assets/ID) requires
    //    Accept: application/octet-stream to get the actual binary content.
    tracing::info!("Downloading update from {}", download_url);
    let resp = client
        .get(download_url)
        .header("Authorization", format!("Bearer {}", github_token))
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .context("Failed to connect for update download")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "Failed to download update: HTTP {} — {}",
            status,
            if body.len() > 200 {
                format!("{}…", &body[..200])
            } else {
                body
            }
        );
    }

    let bytes = resp.bytes().await.context("Failed to read update bytes")?;
    tracing::info!("Downloaded {} bytes", bytes.len());

    // 2. Verify checksum if available
    if let Some(checksum_url) = checksum_url {
        tracing::info!("Downloading checksum from {}", checksum_url);
        let checksum_resp = client
            .get(checksum_url)
            .header("Authorization", format!("Bearer {}", github_token))
            .header("Accept", "application/octet-stream")
            .send()
            .await
            .context("Failed to connect for checksum download")?;

        if !checksum_resp.status().is_success() {
            bail!(
                "Failed to download checksum: HTTP {}",
                checksum_resp.status()
            );
        }

        let checksum_text = checksum_resp.text().await?;
        // Format: "<hex_sha256>  <filename>\n"
        let expected_hash = checksum_text
            .split_whitespace()
            .next()
            .context("Invalid checksum file format")?
            .to_lowercase();

        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let actual_hash = format!("{:x}", hasher.finalize());

        if actual_hash != expected_hash {
            bail!(
                "Checksum mismatch: expected {} got {}",
                expected_hash,
                actual_hash
            );
        }
        tracing::info!("Checksum verified OK");
    } else {
        tracing::warn!("No checksum available, skipping verification");
    }

    // 3. Determine the current binary path
    let current_exe = std::env::current_exe().context("Failed to get current executable path")?;
    tracing::info!("Current binary: {}", current_exe.display());

    // 4. Extract binary bytes from archive (use asset_name since API URLs don't have extensions)
    let binary_bytes = if asset_name.ends_with(".dmg") {
        extract_binary_from_dmg(&bytes)?
    } else if asset_name.ends_with(".tar.gz") {
        extract_binary_from_tar_gz(&bytes)?
    } else {
        bytes.to_vec()
    };

    // 5. Replace the binary atomically
    //    - Write new binary to a temp file next to the current one
    //    - Rename old binary to .bak
    //    - Rename new binary to the current path
    let parent = current_exe
        .parent()
        .context("Current exe has no parent directory")?;
    let temp_path = parent.join(".caravan-update-new");
    let backup_path = parent.join(".caravan-update-bak");

    // Write new binary
    std::fs::write(&temp_path, &binary_bytes).context("Failed to write new binary")?;

    // Set executable permission
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o755))
            .context("Failed to set permissions on new binary")?;
    }

    // Backup old binary
    if backup_path.exists() {
        std::fs::remove_file(&backup_path).ok();
    }
    std::fs::rename(&current_exe, &backup_path).context("Failed to backup current binary")?;

    // Move new binary into place
    if let Err(e) = std::fs::rename(&temp_path, &current_exe) {
        // Try to restore backup
        tracing::error!("Failed to install new binary: {}", e);
        let _ = std::fs::rename(&backup_path, &current_exe);
        bail!("Failed to install new binary: {}", e);
    }

    // Keep backup around — the running process may still reference the old inode on Linux.
    // It will be cleaned up on next update or can be ignored.

    tracing::info!("Update installed successfully at {}", current_exe.display());
    Ok(current_exe)
}

/// Extract the caravan binary from a .tar.gz archive.
fn extract_binary_from_tar_gz(tar_gz_bytes: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    use tar::Archive;

    let decoder = GzDecoder::new(tar_gz_bytes);
    let mut archive = Archive::new(decoder);

    for entry in archive.entries().context("Failed to read tar entries")? {
        let mut entry = entry.context("Failed to read tar entry")?;
        let path = entry.path().context("Failed to read entry path")?;
        if path.file_name().and_then(|n| n.to_str()) == Some("caravan") {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            tracing::info!("Extracted caravan from tar.gz ({} bytes)", buf.len());
            return Ok(buf);
        }
    }

    bail!("caravan binary not found in tar.gz archive")
}

/// Extract the caravan binary from a macOS DMG.
/// Mounts the DMG, copies the binary out, and unmounts.
#[cfg(target_os = "macos")]
fn extract_binary_from_dmg(dmg_bytes: &[u8]) -> Result<Vec<u8>> {
    use std::process::Command;

    // Write DMG to a temp file
    let tmp_dmg = std::env::temp_dir().join("caravan-update.dmg");
    std::fs::write(&tmp_dmg, dmg_bytes).context("Failed to write temp DMG")?;

    let volume = format!("/Volumes/{}", crate::branding::VOLUME_NAME);
    // Mount the DMG
    let mount_output = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-mountpoint", &volume])
        .arg(&tmp_dmg)
        .output()
        .context("Failed to run hdiutil attach")?;

    if !mount_output.status.success() {
        let _ = std::fs::remove_file(&tmp_dmg);
        bail!(
            "hdiutil attach failed: {}",
            String::from_utf8_lossy(&mount_output.stderr)
        );
    }

    // Read the binary from the mounted .app
    let binary_path = format!(
        "{volume}/{}.app/Contents/MacOS/{}",
        crate::branding::DISPLAY_NAME,
        crate::branding::APP_NAME
    );
    let result = std::fs::read(&binary_path).context("Failed to read binary from mounted DMG");

    // Always unmount and clean up
    let _ = Command::new("hdiutil")
        .args(["detach", &volume, "-quiet"])
        .output();
    let _ = std::fs::remove_file(&tmp_dmg);

    let bytes = result?;
    tracing::info!("Extracted binary from DMG ({} bytes)", bytes.len());
    Ok(bytes)
}

/// On Linux, DMG downloads shouldn't happen — this is a fallback.
#[cfg(not(target_os = "macos"))]
fn extract_binary_from_dmg(_dmg_bytes: &[u8]) -> Result<Vec<u8>> {
    bail!("DMG extraction is only supported on macOS");
}
