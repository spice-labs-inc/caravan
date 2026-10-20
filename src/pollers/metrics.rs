//! On-demand pod metrics polling via the K8s metrics.k8s.io API.
//!
//! Metrics are only fetched while the Resources tab is active.
//! The UI creates a `MetricsHandle` which spawns a polling loop;
//! dropping the handle stops polling.

use anyhow::{Context, Result};
use chrono::Utc;
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{api::ListParams, Api, Client};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::{Config, EnvironmentConfig};
use crate::models::deployment::PodMetrics;
use crate::pollers::k8s::{parse_cpu_quantity, parse_memory_quantity};

/// Shared metrics store: env_name → vec of pod metrics.
pub type MetricsStore = Arc<RwLock<HashMap<String, Vec<PodMetrics>>>>;

/// Handle to an active metrics polling session. Drop to stop.
pub struct MetricsHandle {
    _cancel: watch::Sender<bool>,
    pub store: MetricsStore,
}

/// Start polling metrics for all configured environments.
/// Returns a handle; drop it to stop polling.
pub fn start_metrics_polling(runtime: &tokio::runtime::Runtime, config: &Config) -> MetricsHandle {
    let store: MetricsStore = Arc::new(RwLock::new(HashMap::new()));
    let (cancel_tx, cancel_rx) = watch::channel(false);

    for env_config in config.environments.with_clusters() {
        let store = store.clone();
        let env_name = env_config.id.clone();
        let env_config = env_config.clone();
        let mut cancel_rx = cancel_rx.clone();

        runtime.spawn(async move {
            info!(env = %env_name, "Starting metrics poller");

            let client = match create_client(&env_config).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(env = %env_name, err = %e, "Failed to create metrics client");
                    return;
                }
            };

            loop {
                match poll_pod_metrics(&client).await {
                    Ok(metrics) => {
                        info!(env = %env_name, count = metrics.len(), "Metrics poll OK");
                        if let Ok(mut s) = store.try_write() {
                            // Preserve vault namespace metrics (managed by vault metrics poller)
                            let vault_metrics: Vec<_> = s
                                .get(&env_name)
                                .map(|existing| {
                                    existing
                                        .iter()
                                        .filter(|m| m.namespace == "vault")
                                        .cloned()
                                        .collect()
                                })
                                .unwrap_or_default();
                            let mut combined = metrics;
                            combined.extend(vault_metrics);
                            s.insert(env_name.clone(), combined);
                        }
                    }
                    Err(e) => {
                        warn!(env = %env_name, err = %e, "Metrics poll failed");
                    }
                }

                // Wait 10 seconds or until cancelled
                tokio::select! {
                    _ = cancel_rx.changed() => {
                        info!(env = %env_name, "Metrics poller stopped");
                        return;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
                }
            }
        });
    }

    // Also poll metrics for vault environments (using vault kubeconfigs)
    for (env_name, vault_config) in &config.vault {
        if let Some(ref kubeconfig) = vault_config.kubeconfig {
            let store = store.clone();
            let env_name = env_name.clone();
            let env_config = EnvironmentConfig {
                kubeconfig: kubeconfig.clone(),
                ..EnvironmentConfig::new(&env_name)
            };
            let mut cancel_rx = cancel_rx.clone();

            runtime.spawn(async move {
                info!(env = %env_name, "Starting vault metrics poller");

                let client = match create_client(&env_config).await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(env = %env_name, err = %e, "Failed to create vault metrics client");
                        return;
                    }
                };

                loop {
                    match poll_pod_metrics(&client).await {
                        Ok(metrics) => {
                            // Merge vault metrics into existing env metrics
                            let vault_metrics: Vec<_> = metrics
                                .into_iter()
                                .filter(|m| m.namespace == "vault")
                                .collect();
                            if !vault_metrics.is_empty() {
                                if let Ok(mut s) = store.try_write() {
                                    let entry = s.entry(env_name.clone()).or_default();
                                    // Remove old vault metrics and add fresh ones
                                    entry.retain(|m| m.namespace != "vault");
                                    entry.extend(vault_metrics);
                                }
                            }
                        }
                        Err(e) => {
                            warn!(env = %env_name, err = %e, "Vault metrics poll failed");
                        }
                    }

                    tokio::select! {
                        _ = cancel_rx.changed() => {
                            info!(env = %env_name, "Vault metrics poller stopped");
                            return;
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
                    }
                }
            });
        }
    }

    MetricsHandle {
        _cancel: cancel_tx,
        store,
    }
}

// ── PodMetrics CRD types ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PodMetricsCrd {
    metadata: PodMetricsMetadata,
    #[serde(default)]
    containers: Vec<ContainerMetrics>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PodMetricsMetadata {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContainerMetrics {
    #[serde(default)]
    usage: ContainerUsage,
}

#[derive(Debug, Default, Deserialize)]
struct ContainerUsage {
    #[serde(default)]
    cpu: Option<String>,
    #[serde(default)]
    memory: Option<String>,
}

/// Poll all PodMetrics across all namespaces using the dynamic API.
async fn poll_pod_metrics(client: &Client) -> Result<Vec<PodMetrics>> {
    use kube::api::{ApiResource, DynamicObject, GroupVersionKind};

    let gvk = GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", "PodMetrics");
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);

    let list = api
        .list(&ListParams::default())
        .await
        .context("Failed to list PodMetrics")?;

    let mut results = Vec::new();
    for obj in list.items {
        let json_val = serde_json::to_value(&obj)?;
        let pm: PodMetricsCrd = match serde_json::from_value(json_val) {
            Ok(pm) => pm,
            Err(e) => {
                warn!(
                    name = obj.metadata.name.as_deref().unwrap_or("?"),
                    err = %e,
                    "Failed to parse PodMetrics"
                );
                continue;
            }
        };

        let mut cpu_total = 0.0_f64;
        let mut mem_total = 0_u64;

        for container in &pm.containers {
            if let Some(ref cpu) = container.usage.cpu {
                cpu_total += parse_cpu_quantity(cpu);
            }
            if let Some(ref mem) = container.usage.memory {
                mem_total += parse_memory_quantity(mem);
            }
        }

        let timestamp = pm
            .timestamp
            .as_ref()
            .and_then(|t| t.parse().ok())
            .unwrap_or_else(Utc::now);

        results.push(PodMetrics {
            name: pm.metadata.name,
            namespace: pm.metadata.namespace.unwrap_or_default(),
            cpu_millicores: cpu_total,
            memory_bytes: mem_total,
            timestamp,
        });
    }

    Ok(results)
}

/// Create a kube::Client from an EnvironmentConfig.
async fn create_client(env_config: &EnvironmentConfig) -> Result<Client> {
    let kubeconfig_path = Config::expand_path(&env_config.kubeconfig);
    let kubeconfig_path_str = kubeconfig_path
        .to_str()
        .context("Invalid kubeconfig path")?;

    let kubeconfig = Kubeconfig::read_from(kubeconfig_path_str)
        .with_context(|| format!("Failed to read kubeconfig: {}", kubeconfig_path_str))?;

    let kube_options = KubeConfigOptions {
        context: env_config.context.clone(),
        ..Default::default()
    };

    let config = kube::Config::from_custom_kubeconfig(kubeconfig, &kube_options)
        .await
        .with_context(|| format!("Failed to build K8s config from: {}", kubeconfig_path_str))?;

    Client::try_from(config).context("Failed to create K8s client")
}
