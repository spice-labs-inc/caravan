use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::ListParams,
    config::{KubeConfigOptions, Kubeconfig},
    Api, Client as KubeClient,
};
use reqwest::Client;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

use crate::actions::unseal_keys::{self, KeySource};
use crate::actions::vault::{self as vault_actions, KeyInput, UnsealTarget, UnsealTrigger};
use crate::config::{Config, NotificationConfig, VaultConfig};
use crate::state::AppState;

/// Vault health response from GET /v1/sys/health
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct VaultHealth {
    pub initialized: bool,
    pub sealed: bool,
    pub standby: bool,
    pub performance_standby: bool,
    #[serde(default)]
    pub replication_performance_mode: String,
    #[serde(default)]
    pub replication_dr_mode: String,
    #[serde(default)]
    pub server_time_utc: Option<i64>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub cluster_name: String,
    #[serde(default)]
    pub cluster_id: String,
}

/// Vault seal status response from GET /v1/sys/seal-status
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct VaultSealStatus {
    #[serde(rename = "type")]
    pub seal_type: String,
    pub initialized: bool,
    pub sealed: bool,
    pub t: u32, // threshold
    pub n: u32, // total shares
    pub progress: u32,
    #[serde(default)]
    pub nonce: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub cluster_name: String,
    #[serde(default)]
    pub cluster_id: String,
}

/// Info about a single vault pod discovered via kube-rs
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct VaultPodInfo {
    pub name: String,
    pub ready: bool,                   // K8s readiness (NOT reliable for seal status)
    pub sealed: Option<bool>,          // Per-pod seal status from `vault status` (None = unknown)
    pub unseal_progress: Option<u32>,  // Keys submitted so far (when sealed)
    pub unseal_threshold: Option<u32>, // Keys needed to unseal
    pub phase: String,                 // Running, Pending, etc.
    pub restarts: i32,
    pub created: Option<DateTime<Utc>>,
}

/// What level of vault interaction is available for this environment
#[derive(Debug, Clone, PartialEq, Default)]
pub enum VaultCapability {
    /// HTTP + pod listing via kube-rs (kubeconfig works)
    Full,
    /// Only HTTP health/seal status (no kubeconfig or kubeconfig failed)
    HttpOnly,
    /// No vault section in config
    #[default]
    NotConfigured,
}

/// Combined vault state for one environment
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct VaultState {
    pub url: String,
    pub reachable: bool,
    pub health: Option<VaultHealth>,
    pub seal_status: Option<VaultSealStatus>,
    pub error: Option<String>,
    pub last_poll: Option<DateTime<Utc>>,
    /// Per-pod info from kube-rs (empty if HttpOnly or NotConfigured)
    pub pods: Vec<VaultPodInfo>,
    /// What we can do with this vault environment
    pub capability: VaultCapability,
    /// Expanded kubeconfig path (if configured)
    pub kubeconfig_path: Option<PathBuf>,
    /// Error from pod discovery (distinct from HTTP error)
    pub pod_error: Option<String>,
    /// When a pod was first seen sealed; cleared once every pod is unsealed
    pub sealed_since: Option<DateTime<Utc>>,
    /// Where an unseal would get its keys from right now
    pub key_source: KeySource,
    /// `[vault.<env>] auto_unseal`
    pub auto_unseal: bool,
}

impl Default for VaultState {
    fn default() -> Self {
        Self {
            url: String::new(),
            reachable: false,
            health: None,
            seal_status: None,
            error: None,
            last_poll: None,
            pods: Vec::new(),
            capability: VaultCapability::default(),
            kubeconfig_path: None,
            pod_error: None,
            sealed_since: None,
            key_source: KeySource::Manual,
            auto_unseal: false,
        }
    }
}

impl VaultState {
    pub fn sealed_pod_count(&self) -> usize {
        self.pods.iter().filter(|p| p.sealed == Some(true)).count()
    }

    /// Sealed truth: any pod reporting sealed via kubectl.
    pub fn is_sealed_by_pods(&self) -> bool {
        self.sealed_pod_count() > 0
    }

    /// Every pod answered and none is sealed.
    pub fn all_pods_unsealed(&self) -> bool {
        !self.pods.is_empty() && self.pods.iter().all(|p| p.sealed == Some(false))
    }

    /// Pod-level seal truth when pods answered, None when nothing is known.
    pub fn pod_sealed(&self) -> Option<bool> {
        if self.is_sealed_by_pods() {
            Some(true)
        } else if self.all_pods_unsealed() {
            Some(false)
        } else {
            None
        }
    }

    /// No pod data and the HTTP probe failed too.
    pub fn is_unreachable(&self) -> bool {
        self.pods.is_empty() && !self.reachable
    }

    /// Sealed by pods when there is pod data, else by the HTTP seal status.
    /// Unknown never counts as sealed.
    pub fn is_sealed(&self) -> bool {
        if !self.pods.is_empty() {
            self.is_sealed_by_pods()
        } else {
            self.seal_status.as_ref().is_some_and(|s| s.sealed)
        }
    }

    /// Threshold from the HTTP seal status, the fallback after the pod's own.
    pub fn http_threshold(&self) -> Option<u32> {
        self.seal_status.as_ref().map(|s| s.t)
    }

    /// Sealed pods as unseal targets, each with the threshold it reported.
    pub fn sealed_targets(&self) -> Vec<UnsealTarget> {
        self.pods
            .iter()
            .filter(|p| p.sealed == Some(true))
            .map(|p| UnsealTarget {
                name: p.name.clone(),
                threshold: p.unseal_threshold,
            })
            .collect()
    }

    #[allow(dead_code)]
    pub fn is_healthy(&self) -> bool {
        self.reachable && !self.is_sealed()
    }

    #[allow(dead_code)]
    pub fn unseal_progress(&self) -> Option<(u32, u32)> {
        self.seal_status.as_ref().map(|s| (s.progress, s.t))
    }

    #[allow(dead_code)]
    pub fn version(&self) -> Option<&str> {
        self.health.as_ref().map(|h| h.version.as_str())
    }

    #[allow(dead_code)]
    pub fn ha_mode(&self) -> &str {
        match &self.health {
            Some(h) if !h.sealed && !h.standby => "active",
            Some(h) if !h.sealed && h.standby => "standby",
            Some(h) if h.sealed => "sealed",
            _ => "unknown",
        }
    }
}

/// Tracks one env's seal transitions across polls so the poller fires the
/// seal notification and auto-unseal once per seal episode.
#[derive(Debug, Clone)]
pub struct SealTracker {
    prev_sealed: Option<bool>,
    pub sealed_since: Option<DateTime<Utc>>,
    auto_fired: bool,
    notified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SealObservation {
    pub became_sealed: bool,
    pub became_unsealed: bool,
    /// Fire the desktop notification for this seal
    pub notify: bool,
    /// Start an auto-unseal run for this seal
    pub auto_unseal: bool,
}

impl SealTracker {
    /// `restored` is the sealed-since time saved by a previous process; the
    /// user was told about that seal already, so it is not notified again.
    pub fn new(restored: Option<DateTime<Utc>>) -> Self {
        Self {
            prev_sealed: None,
            sealed_since: restored,
            auto_fired: false,
            notified: restored.is_some(),
        }
    }

    pub fn observe(
        &mut self,
        sealed: Option<bool>,
        now: DateTime<Utc>,
        auto_enabled: bool,
    ) -> SealObservation {
        let mut obs = SealObservation::default();
        match sealed {
            None => return obs,
            Some(true) => {
                if self.sealed_since.is_none() {
                    self.sealed_since = Some(now);
                }
                if self.prev_sealed != Some(true) {
                    obs.became_sealed = true;
                    if !self.notified {
                        obs.notify = true;
                        self.notified = true;
                    }
                    if auto_enabled && !self.auto_fired {
                        obs.auto_unseal = true;
                        self.auto_fired = true;
                    }
                }
            }
            Some(false) => {
                if self.prev_sealed == Some(true) || self.sealed_since.is_some() {
                    obs.became_unsealed = true;
                }
                self.sealed_since = None;
                self.notified = false;
                self.auto_fired = false;
            }
        }
        self.prev_sealed = sealed;
        obs
    }
}

/// Try to create a kube client from a vault kubeconfig path
async fn create_kube_client(kubeconfig_path: &std::path::Path) -> anyhow::Result<KubeClient> {
    let path_str = kubeconfig_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid kubeconfig path"))?;

    let kubeconfig = Kubeconfig::read_from(path_str)
        .map_err(|e| anyhow::anyhow!("Failed to read kubeconfig {}: {}", path_str, e))?;

    let config = kube::Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build K8s config: {}", e))?;

    KubeClient::try_from(config).map_err(|e| anyhow::anyhow!("Failed to create client: {}", e))
}

/// Discover vault pods via kube-rs
async fn discover_pods(
    client: &KubeClient,
    cfg: &VaultConfig,
) -> anyhow::Result<Vec<VaultPodInfo>> {
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), &cfg.namespace);
    let lp = ListParams::default().labels(&cfg.label_selector);
    let pod_list = pods_api.list(&lp).await?;

    let mut result = Vec::new();
    for pod in pod_list {
        let name = pod.metadata.name.unwrap_or_default();
        let status = pod.status.as_ref();
        let phase = status
            .and_then(|s| s.phase.clone())
            .unwrap_or_else(|| "Unknown".to_string());

        // Check readiness: vault's readiness probe checks seal status
        let ready = status
            .and_then(|s| s.conditions.as_ref())
            .map(|conditions| {
                conditions
                    .iter()
                    .any(|c| c.type_ == "Ready" && c.status == "True")
            })
            .unwrap_or(false);

        let restarts = status
            .and_then(|s| s.container_statuses.as_ref())
            .and_then(|cs| cs.first())
            .map(|c| c.restart_count)
            .unwrap_or(0);

        let created = pod.metadata.creation_timestamp.map(|ts| ts.0);

        result.push(VaultPodInfo {
            name,
            ready,
            sealed: None, // filled in by check_pod_seal_status
            unseal_progress: None,
            unseal_threshold: None,
            phase,
            restarts,
            created,
        });
    }

    // Sort by name for consistent ordering
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

/// Check per-pod seal status via `kubectl exec vault-X -- vault status -format=json`.
/// This is the only reliable way: K8s readiness probes don't always reflect seal state.
async fn check_pod_seal_status(
    pods: &mut [VaultPodInfo],
    kubeconfig_path: &std::path::Path,
    namespace: &str,
) {
    for pod in pods.iter_mut() {
        if pod.phase != "Running" {
            pod.sealed = Some(true);
            continue;
        }
        match crate::actions::vault::get_pod_status(kubeconfig_path, namespace, &pod.name).await {
            Ok(status) => {
                pod.sealed = Some(status.sealed);
                pod.unseal_progress = Some(status.progress);
                pod.unseal_threshold = Some(status.t);
            }
            Err(e) => {
                tracing::debug!(pod = %pod.name, err = %e, "Could not check pod seal status");
                pod.sealed = None;
                pod.unseal_progress = None;
                pod.unseal_threshold = None;
            }
        }
    }
}

/// Which key source an unseal would use now. Reads the keyring, so it runs
/// off the async threads.
async fn probe_key_source(
    env: &str,
    cfg: &VaultConfig,
    remember_override: Option<bool>,
) -> KeySource {
    let (env, cfg) = (env.to_string(), cfg.clone());
    tokio::task::spawn_blocking(move || {
        unseal_keys::probe_key_source(&env, &cfg, remember_override)
    })
    .await
    .unwrap_or(KeySource::Manual)
}

/// Start vault pollers for all configured vault environments
pub async fn start_polling(
    config: Config,
    state: AppState,
    cache: Option<std::sync::Arc<crate::cache::Cache>>,
) {
    let http_client = Client::builder()
        .danger_accept_invalid_certs(true) // Vault uses self-signed TLS
        .timeout(Duration::from_secs(5))
        .build()
        .expect("Failed to create HTTP client for vault");

    let normal_interval = Duration::from_secs(config.polling.vault_interval_secs);
    let sealed_interval = Duration::from_secs(config.polling.vault_sealed_interval_secs);
    let notifications = config.notifications.clone();

    if config.vault.is_empty() {
        info!("No vault environments configured");
        return;
    }

    let restored_since = cache
        .as_ref()
        .and_then(|c| c.load_vault_sealed_since().ok())
        .unwrap_or_default();

    for (env_name, vault_cfg) in config.vault.clone() {
        let state = state.clone();
        let client = http_client.clone();
        let cache = cache.clone();
        let notifications = notifications.clone();
        let url = vault_cfg.url.clone();
        let kubeconfig_path = vault_cfg
            .kubeconfig
            .as_ref()
            .map(|p| Config::expand_path(p));
        let mut tracker = SealTracker::new(restored_since.get(&env_name).copied());

        tokio::spawn(async move {
            info!(env = %env_name, url = %url, kubeconfig = ?kubeconfig_path, "Starting Vault poller");

            // Try to create kube client if kubeconfig is configured
            let kube_client = if let Some(ref kc_path) = kubeconfig_path {
                if kc_path.exists() {
                    match create_kube_client(kc_path).await {
                        Ok(c) => {
                            info!(env = %env_name, "Vault kube client created, full capability");
                            Some(c)
                        }
                        Err(e) => {
                            warn!(env = %env_name, err = %e, "Vault kube client failed, HTTP only");
                            None
                        }
                    }
                } else {
                    warn!(env = %env_name, path = ?kc_path, "Vault kubeconfig not found, HTTP only");
                    None
                }
            } else {
                None
            };

            loop {
                let mut vault_state = poll_vault_http(&client, &url).await;

                // Set capability and kubeconfig path
                vault_state.kubeconfig_path = kubeconfig_path.clone();
                vault_state.capability = if kube_client.is_some() {
                    VaultCapability::Full
                } else {
                    VaultCapability::HttpOnly
                };
                vault_state.auto_unseal = vault_cfg.auto_unseal;

                // Discover pods if we have a kube client
                if let Some(ref kc) = kube_client {
                    // Timeout prevents hanging on stale connections after sleep
                    let discover_result = tokio::time::timeout(
                        Duration::from_secs(15),
                        discover_pods(kc, &vault_cfg),
                    )
                    .await;
                    match discover_result {
                        Ok(Ok(mut pods)) => {
                            // Check per-pod seal status via kubectl exec
                            if let Some(ref kc_path) = kubeconfig_path {
                                check_pod_seal_status(&mut pods, kc_path, &vault_cfg.namespace)
                                    .await;
                            }
                            vault_state.pods = pods;
                            vault_state.pod_error = None;
                        }
                        Ok(Err(e)) => {
                            warn!(env = %env_name, err = %e, "Failed to discover vault pods");
                            vault_state.pod_error = Some(format!("{}", e));
                        }
                        Err(_) => {
                            warn!(env = %env_name, "Vault pod discovery timed out");
                            vault_state.pod_error = Some("Pod discovery timed out".to_string());
                        }
                    }
                }

                let remember_override = state.vault_remember_override(&env_name);
                vault_state.key_source =
                    probe_key_source(&env_name, &vault_cfg, remember_override).await;

                let now = Utc::now();
                let auto_possible = vault_cfg.auto_unseal
                    && vault_state.key_source.is_automatic()
                    && vault_state.kubeconfig_path.is_some();
                let obs = tracker.observe(vault_state.pod_sealed(), now, auto_possible);
                vault_state.sealed_since = tracker.sealed_since;

                if obs.became_sealed {
                    warn!(
                        env = %env_name,
                        sealed = vault_state.sealed_pod_count(),
                        total = vault_state.pods.len(),
                        "Vault sealed"
                    );
                }
                if obs.became_unsealed {
                    info!(env = %env_name, "Vault unsealed");
                }
                if obs.notify && crate::notify::env_enabled(&notifications, &env_name) {
                    crate::notify::notify(
                        &notifications,
                        &format!("{} vault sealed", env_name),
                        &format!(
                            "{} of {} pods sealed. Keys: {}.",
                            vault_state.sealed_pod_count(),
                            vault_state.pods.len(),
                            vault_state.key_source.label()
                        ),
                    );
                }
                if obs.auto_unseal {
                    start_auto_unseal(
                        &state,
                        &env_name,
                        &vault_cfg,
                        &vault_state,
                        remember_override,
                        &notifications,
                    );
                }

                let is_sealed = vault_state.is_sealed_by_pods();

                if let Some(ref cache) = cache {
                    if let Err(e) = cache.save_vault_status(&env_name, &vault_state) {
                        tracing::warn!(env = %env_name, err = %e, "Failed to cache vault status");
                    }
                }

                state.write(|s| {
                    s.vault_states.insert(env_name.clone(), vault_state);
                });

                // Poll faster while pods are sealed (someone might be unsealing)
                let interval = if is_sealed {
                    sealed_interval
                } else {
                    normal_interval
                };
                tokio::time::sleep(interval).await;
            }
        });
    }

    // Keep alive
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// Kick off one auto-unseal run for a freshly sealed env. `spawn_unseal`
/// refuses when a run is already going, so a manual run in flight wins.
fn start_auto_unseal(
    state: &AppState,
    env_name: &str,
    vault_cfg: &VaultConfig,
    vault_state: &VaultState,
    remember_override: Option<bool>,
    notifications: &NotificationConfig,
) {
    let Some(kubeconfig) = vault_state.kubeconfig_path.clone() else {
        return;
    };
    let targets = vault_state.sealed_targets();
    if targets.is_empty() {
        return;
    }
    let fallback = vault_actions::select_threshold(None, vault_state.http_threshold());
    info!(env = %env_name, pods = targets.len(), source = vault_state.key_source.label(), "Auto-unseal starting");
    let started = vault_actions::spawn_unseal(
        &tokio::runtime::Handle::current(),
        state.clone(),
        env_name.to_string(),
        kubeconfig,
        targets,
        fallback,
        KeyInput::Resolve {
            cfg: vault_cfg.clone(),
            remember_override,
        },
        UnsealTrigger::Auto,
        false,
        Some(notifications.clone()),
    );
    if !started {
        info!(env = %env_name, "Auto-unseal skipped: an unseal run is already going");
    }
}

async fn poll_vault_http(client: &Client, url: &str) -> VaultState {
    let mut state = VaultState {
        url: url.to_string(),
        ..Default::default()
    };

    // GET /v1/sys/health returns various HTTP status codes but always JSON
    // when vault is responding. When sealed, the ingress/envoy often can't get
    // TLS certs and returns a non-JSON error; that's expected, not an error.
    match client.get(format!("{}/v1/sys/health", url)).send().await {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            match serde_json::from_str::<VaultHealth>(&body) {
                Ok(health) => {
                    state.reachable = true;
                    state.health = Some(health);
                }
                Err(_) => {
                    // Non-JSON response (e.g. proxy/LB error when vault is sealed).
                    // Pod discovery via kubectl exec is the primary path.
                    tracing::debug!(url = %url, "Vault HTTP health unavailable (likely sealed)");
                }
            }
        }
        Err(_) => {
            // Connection refused, timeout, etc. Expected when sealed.
            tracing::debug!(url = %url, "Vault HTTP unreachable (likely sealed)");
            state.last_poll = Some(Utc::now());
            return state;
        }
    }

    // GET /v1/sys/seal-status
    match client
        .get(format!("{}/v1/sys/seal-status", url))
        .send()
        .await
    {
        Ok(resp) => {
            let body = resp.text().await.unwrap_or_default();
            if let Ok(seal) = serde_json::from_str::<VaultSealStatus>(&body) {
                state.seal_status = Some(seal);
            }
        }
        Err(_) => {
            // Expected when sealed
        }
    }

    state.last_poll = Some(Utc::now());
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str, sealed: Option<bool>, threshold: Option<u32>) -> VaultPodInfo {
        VaultPodInfo {
            name: name.to_string(),
            ready: sealed == Some(false),
            sealed,
            unseal_progress: Some(0),
            unseal_threshold: threshold,
            phase: "Running".to_string(),
            restarts: 0,
            created: None,
        }
    }

    fn seal_status(sealed: bool, t: u32) -> VaultSealStatus {
        VaultSealStatus {
            seal_type: "shamir".to_string(),
            initialized: true,
            sealed,
            t,
            n: 5,
            progress: 0,
            nonce: String::new(),
            version: String::new(),
            cluster_name: String::new(),
            cluster_id: String::new(),
        }
    }

    #[test]
    fn one_sealed_pod_makes_the_env_sealed_not_unreachable() {
        let vs = VaultState {
            reachable: false,
            pods: vec![
                pod("vault-0", Some(false), Some(3)),
                pod("vault-1", Some(true), Some(3)),
                pod("vault-2", Some(false), Some(3)),
            ],
            ..Default::default()
        };
        assert!(vs.is_sealed_by_pods());
        assert!(vs.is_sealed());
        assert_eq!(vs.sealed_pod_count(), 1);
        assert!(!vs.is_unreachable());
        assert_eq!(vs.pod_sealed(), Some(true));
        let targets = vs.sealed_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, "vault-1");
        assert_eq!(targets[0].threshold, Some(3));
    }

    #[test]
    fn no_pods_and_http_down_is_unreachable_not_sealed() {
        let vs = VaultState::default();
        assert!(vs.is_unreachable());
        assert!(!vs.is_sealed());
        assert!(!vs.is_sealed_by_pods());
        assert_eq!(vs.pod_sealed(), None);
    }

    #[test]
    fn http_only_sealed_status_counts_without_pods() {
        let vs = VaultState {
            reachable: true,
            seal_status: Some(seal_status(true, 2)),
            ..Default::default()
        };
        assert!(vs.is_sealed());
        assert!(!vs.is_sealed_by_pods());
        assert!(!vs.is_unreachable());
        assert_eq!(vs.http_threshold(), Some(2));
    }

    #[test]
    fn unknown_pods_are_neither_sealed_nor_unsealed() {
        let vs = VaultState {
            reachable: true,
            pods: vec![
                pod("vault-0", None, None),
                pod("vault-1", Some(false), Some(3)),
            ],
            ..Default::default()
        };
        assert_eq!(vs.pod_sealed(), None);
        assert!(!vs.is_sealed());
        assert!(!vs.all_pods_unsealed());
    }

    #[test]
    fn tracker_fires_notify_and_auto_once_per_seal_episode() {
        let t0 = Utc::now();
        let mut tr = SealTracker::new(None);
        assert_eq!(
            tr.observe(Some(false), t0, true),
            SealObservation::default()
        );
        let obs = tr.observe(Some(true), t0, true);
        assert!(obs.became_sealed && obs.notify && obs.auto_unseal);
        assert_eq!(tr.sealed_since, Some(t0));
        // still sealed: nothing fires again, sealed_since stays
        let later = t0 + chrono::Duration::seconds(30);
        assert_eq!(
            tr.observe(Some(true), later, true),
            SealObservation::default()
        );
        assert_eq!(tr.sealed_since, Some(t0));
        // unknown poll in between changes nothing
        assert_eq!(tr.observe(None, later, true), SealObservation::default());
        assert_eq!(tr.sealed_since, Some(t0));
        // unsealed clears, then a new seal fires again
        let obs = tr.observe(Some(false), later, true);
        assert!(obs.became_unsealed);
        assert_eq!(tr.sealed_since, None);
        let obs = tr.observe(Some(true), later, true);
        assert!(obs.notify && obs.auto_unseal);
    }

    #[test]
    fn tracker_startup_with_sealed_pods_fires_auto_but_restored_seal_is_not_renotified() {
        let t0 = Utc::now();
        let earlier = t0 - chrono::Duration::minutes(10);
        let mut tr = SealTracker::new(Some(earlier));
        let obs = tr.observe(Some(true), t0, true);
        assert!(obs.became_sealed);
        assert!(!obs.notify, "restored seal was already notified");
        assert!(obs.auto_unseal);
        assert_eq!(tr.sealed_since, Some(earlier), "restored clock kept");

        let mut fresh = SealTracker::new(None);
        let obs = fresh.observe(Some(true), t0, false);
        assert!(obs.notify);
        assert!(!obs.auto_unseal, "auto disabled");
        assert_eq!(
            fresh.observe(Some(true), t0, true),
            SealObservation::default()
        );
    }
}
