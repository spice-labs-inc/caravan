use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::watch;

/// Scopes needed: repo (private repo access), read:org (org listing), read:project (Projects V2 board status)
const SCOPES: &str = "repo read:project";

/// State of the device flow authorization process.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceFlowState {
    /// Not started
    Idle,
    /// Waiting for user to enter code at verification URL
    WaitingForUser {
        user_code: String,
        verification_uri: String,
    },
    /// Polling GitHub for authorization
    Polling {
        user_code: String,
        verification_uri: String,
    },
    /// Successfully authorized
    Success { token: String, username: String },
    /// Flow failed or was denied
    Error(String),
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default = "default_interval")]
    interval: u64,
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GitHubUser {
    login: String,
}

/// Start the GitHub OAuth device flow.
/// Returns a watch receiver that can be polled for state updates.
/// The flow runs asynchronously and updates the state as it progresses.
/// `client_id` is the deployment's own GitHub OAuth App (device flow
/// enabled), from config `github.oauth_client_id`; there is no default.
pub fn start_device_flow(
    runtime: &tokio::runtime::Runtime,
    client_id: String,
) -> watch::Receiver<DeviceFlowState> {
    let (tx, rx) = watch::channel(DeviceFlowState::Idle);
    let tx = Arc::new(tx);

    runtime.spawn(run_device_flow(tx, client_id));

    rx
}

async fn run_device_flow(tx: Arc<watch::Sender<DeviceFlowState>>, client_id: String) {
    match run_device_flow_inner(&tx, &client_id).await {
        Ok(()) => {}
        Err(e) => {
            let _ = tx.send(DeviceFlowState::Error(format!("{:#}", e)));
        }
    }
}

async fn run_device_flow_inner(tx: &watch::Sender<DeviceFlowState>, client_id: &str) -> Result<()> {
    if client_id.trim().is_empty() {
        anyhow::bail!(
            "No GitHub OAuth App client id configured. Register an OAuth App with device \
             flow enabled for your organization and set github.oauth_client_id (or use \
             token_cmd / token_env instead)."
        );
    }
    let client = reqwest::Client::new();

    // Step 1: Request device code
    let resp = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[("client_id", client_id), ("scope", SCOPES)])
        .send()
        .await
        .context("Failed to request device code")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Device code request failed ({}): {}", status, body);
    }

    let device: DeviceCodeResponse = resp
        .json()
        .await
        .context("Failed to parse device code response")?;

    tracing::info!(
        "Device flow started: enter code {} at {}",
        device.user_code,
        device.verification_uri
    );

    // Open browser
    if let Err(e) = open::that(&device.verification_uri) {
        tracing::warn!("Failed to open browser: {}", e);
    }

    let _ = tx.send(DeviceFlowState::WaitingForUser {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
    });

    // Step 2: Poll for token
    let mut interval = device.interval;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(device.expires_in);

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

        if std::time::Instant::now() > deadline {
            anyhow::bail!("Device flow timed out — the code has expired. Please try again.");
        }

        let _ = tx.send(DeviceFlowState::Polling {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
        });

        let resp = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .context("Failed to poll for token")?;

        let token_resp: TokenResponse = resp
            .json()
            .await
            .context("Failed to parse token response")?;

        if let Some(token) = token_resp.access_token {
            // Verify the token by fetching the user
            let username = fetch_username(&client, &token).await.unwrap_or_else(|e| {
                tracing::warn!("Failed to fetch username: {}", e);
                "unknown".to_string()
            });

            // Store in keyring
            if let Err(e) = super::keyring::store_token(&token) {
                tracing::error!("Failed to store token: {}", e);
            }

            let _ = tx.send(DeviceFlowState::Success { token, username });
            return Ok(());
        }

        match token_resp.error.as_deref() {
            Some("authorization_pending") => {
                // Keep polling
                continue;
            }
            Some("slow_down") => {
                // Increase interval
                interval = token_resp.interval.unwrap_or(interval + 5);
                tracing::debug!("Slowing down polling to {}s", interval);
                continue;
            }
            Some("expired_token") => {
                anyhow::bail!("Device code expired. Please try again.");
            }
            Some("access_denied") => {
                anyhow::bail!("Authorization was denied by the user.");
            }
            Some(other) => {
                anyhow::bail!("GitHub OAuth error: {}", other);
            }
            None => {
                anyhow::bail!("Unexpected response from GitHub (no token, no error)");
            }
        }
    }
}

async fn fetch_username(client: &reqwest::Client, token: &str) -> Result<String> {
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", crate::branding::APP_NAME)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("Failed to fetch user")?;

    let user: GitHubUser = resp.json().await.context("Failed to parse user response")?;
    Ok(user.login)
}

/// Verify that a token is still valid by calling the GitHub API.
/// Returns the username if valid, None if invalid/expired.
pub async fn verify_token(token: &str) -> Option<String> {
    let client = reqwest::Client::new();
    (fetch_username(&client, token).await).ok()
}
