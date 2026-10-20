//! Polls GitHub releases of the configured `[update] repo` for new versions.

use crate::config::Config;
use crate::pollers::gh_http::GhClient;
use crate::state::{AppState, LatestVersion};
use std::time::Duration;

const CHECK_INTERVAL: Duration = Duration::from_secs(3600); // every hour

/// Asset name for the current platform
fn platform_asset_name() -> &'static str {
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "caravan-macos-aarch64.dmg"
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
        "caravan-macos-x86_64.dmg"
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        "caravan-linux-x86_64.tar.gz"
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        "caravan-linux-aarch64.tar.gz"
    } else {
        "caravan-unknown"
    }
}

pub async fn start_polling(
    config: Config,
    state: AppState,
    cache: Option<std::sync::Arc<crate::cache::Cache>>,
) {
    let Some(repo) = config.update.repo.clone() else {
        tracing::info!("No [update] repo configured, update checks are off");
        return;
    };
    let token = match config.resolve_github_token() {
        Ok(Some(t)) => t,
        _ => {
            tracing::warn!("No GitHub token available, skipping update checks");
            return;
        }
    };

    // Conditional requests: an unchanged release list is a free 304.
    let client = match GhClient::new(token, cache, state.clone()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(err = %e, "Failed to build update checker client");
            return;
        }
    };

    tracing::info!(
        "Starting update checker, interval={}s",
        CHECK_INTERVAL.as_secs()
    );

    loop {
        match check_latest_release(&client, &repo).await {
            Ok(Some(version)) => {
                let current = crate::app_version();
                tracing::info!(
                    "Latest caravan release: v{} (current: v{})",
                    version.version,
                    current
                );
                state.set_latest_version(version);
            }
            Ok(None) => {
                tracing::debug!("No releases found for caravan");
            }
            Err(e) => {
                tracing::warn!("Failed to check for updates: {}", e);
            }
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

#[derive(serde::Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    published_at: Option<String>,
    body: Option<String>,
    draft: bool,
    prerelease: bool,
    assets: Vec<GhAsset>,
}

#[derive(serde::Deserialize)]
struct GhAsset {
    name: String,
    /// API URL for downloading (works for private repos with token auth)
    url: String,
    #[allow(dead_code)]
    browser_download_url: String,
}

/// Check recent releases and find the newest one that has a binary for this platform.
/// Falls back to the latest release (without download_url) if none have a matching asset.
async fn check_latest_release(
    client: &GhClient,
    repo: &str,
) -> anyhow::Result<Option<LatestVersion>> {
    let route = format!("/repos/{repo}/releases?per_page=20");
    let body = match client.get(&route).await {
        Ok(v) => v,
        Err(e) if e.to_string().contains("404") => return Ok(None),
        Err(e) => return Err(e),
    };
    let releases: Vec<GhRelease> = serde_json::from_value(body)?;
    let asset_name = platform_asset_name();
    let checksum_name = format!("{}.sha256", asset_name);

    // Find the newest release that has a binary for this platform
    let mut best_with_binary: Option<LatestVersion> = None;
    let mut latest_release: Option<LatestVersion> = None;

    for release in &releases {
        if release.draft || release.prerelease {
            continue;
        }

        let version = release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name)
            .to_string();

        let published_at = release
            .published_at
            .as_ref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        let download_url = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .map(|a| a.url.clone());

        let checksum_url = release
            .assets
            .iter()
            .find(|a| a.name == checksum_name)
            .map(|a| a.url.clone());

        let available_assets: Vec<&str> = release.assets.iter().map(|a| a.name.as_str()).collect();
        tracing::debug!(
            "Release v{}: assets={:?}, has_binary={}",
            version,
            available_assets,
            download_url.is_some()
        );

        let lv = LatestVersion {
            version,
            html_url: release.html_url.clone(),
            download_url,
            asset_name: Some(asset_name.to_string()),
            checksum_url,
            release_notes: release.body.clone(),
            published_at,
        };

        // Track the overall latest release (first non-draft/prerelease)
        if latest_release.is_none() {
            latest_release = Some(lv.clone());
        }

        // Track the newest release with a binary for this platform
        if lv.download_url.is_some() && best_with_binary.is_none() {
            best_with_binary = Some(lv);
        }

        // Stop early if we found both
        if latest_release.is_some() && best_with_binary.is_some() {
            break;
        }
    }

    // Prefer the release with a binary; fall back to latest (View release only)
    let result = best_with_binary.or(latest_release);
    Ok(result)
}
