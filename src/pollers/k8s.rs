use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::ListParams,
    config::{KubeConfigOptions, Kubeconfig},
    Api, Client,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::config::{self as app_config, Config, EnvironmentConfig};
use crate::models::deployment::{ContainerStatusInfo, DeploymentStatus, HelmReleaseInfo, PodInfo};
use crate::state::{AppState, KustomizationInfo};

/// Entry point called from app.rs
pub async fn start_polling(
    config: Config,
    state: AppState,
    cache: Option<std::sync::Arc<crate::cache::Cache>>,
) {
    start_k8s_pollers(
        state,
        config.environments.clone(),
        config.polling.k8s_interval_secs,
        cache,
    );
    // Keep this task alive
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// Start background K8s polling for all configured environments.
/// Spawns one task per environment that polls on the configured interval.
pub fn start_k8s_pollers(
    state: AppState,
    environments: crate::config::Environments,
    interval_secs: u64,
    cache: Option<std::sync::Arc<crate::cache::Cache>>,
) {
    for env_config in environments.with_clusters().cloned() {
        let state = state.clone();
        let env_name = env_config.id.clone();
        let cache = cache.clone();
        let interval = Duration::from_secs(interval_secs);

        tokio::spawn(async move {
            info!(env = %env_name, "Starting K8s poller");

            // Build the client once, reconnect on failure
            let mut client: Option<Client> = None;

            loop {
                if client.is_none() {
                    match create_client(&env_config).await {
                        Ok(c) => {
                            info!(env = %env_name, "Connected to K8s cluster");
                            client = Some(c);
                        }
                        Err(e) => {
                            error!(env = %env_name, err = %e, "Failed to connect to K8s cluster");
                            state.set_environment_error(
                                &env_name,
                                format!("Connection failed: {}", e),
                            );
                            tokio::time::sleep(interval).await;
                            continue;
                        }
                    }
                }

                if let Some(ref c) = client {
                    // Timeout prevents hanging on stale connections (e.g. after machine sleep)
                    match tokio::time::timeout(Duration::from_secs(30), poll_cluster(c)).await {
                        Ok(Ok((helm_releases, pods, kustomization))) => {
                            if let Some(ref cache) = cache {
                                if let Err(e) = cache.save_environment(
                                    &env_name,
                                    &helm_releases,
                                    &pods,
                                    kustomization.as_ref(),
                                ) {
                                    tracing::warn!(env = %env_name, err = %e, "Failed to cache K8s data");
                                }
                            }
                            state.update_environment(&env_name, helm_releases, pods, kustomization);
                        }
                        Ok(Err(e)) => {
                            warn!(env = %env_name, err = %e, "K8s poll failed, will retry");
                            state.set_environment_error(&env_name, format!("Poll failed: {}", e));
                            // Force reconnect on next iteration
                            client = None;
                        }
                        Err(_) => {
                            warn!(env = %env_name, "K8s poll timed out, reconnecting");
                            state.set_environment_error(&env_name, "Poll timed out".to_string());
                            client = None;
                        }
                    }
                }

                tokio::time::sleep(interval).await;
            }
        });
    }
}

/// Create a kube::Client from a kubeconfig path
pub(crate) async fn create_client(env_config: &EnvironmentConfig) -> Result<Client> {
    let kubeconfig_path = app_config::Config::expand_path(&env_config.kubeconfig);
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
        .with_context(|| {
            format!(
                "Failed to build K8s config from: {} (context: {:?})",
                kubeconfig_path_str, env_config.context
            )
        })?;

    Client::try_from(config).context("Failed to create K8s client")
}

/// Poll a single cluster for HelmReleases, Pods and the apps Kustomization.
/// The Kustomization is optional: not being able to read it never fails the
/// environment.
async fn poll_cluster(
    client: &Client,
) -> Result<(
    Vec<HelmReleaseInfo>,
    Vec<PodInfo>,
    Option<KustomizationInfo>,
)> {
    let (helm_releases, pods, kustomization) =
        tokio::try_join!(poll_helm_releases(client), poll_pods(client), async {
            Ok::<_, anyhow::Error>(match poll_apps_kustomization(client).await {
                Ok(k) => k,
                Err(e) => {
                    warn!(err = %e, "Failed to read apps Kustomization");
                    None
                }
            })
        })?;
    Ok((helm_releases, pods, kustomization))
}

/// The configured Flux Kustomization: what revision of the gitops repo is
/// applied to this cluster.
async fn poll_apps_kustomization(client: &Client) -> Result<Option<KustomizationInfo>> {
    use kube::api::{ApiResource, DynamicObject, GroupVersionKind};

    let flux = app_config::flux();
    let gvk = GroupVersionKind::gvk("kustomize.toolkit.fluxcd.io", "v1", "Kustomization");
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), &flux.namespace, &ar);
    let obj = match api
        .get_opt(&flux.kustomization)
        .await
        .with_context(|| format!("Failed to get Kustomization {}", flux.kustomization))?
    {
        Some(o) => o,
        None => return Ok(None),
    };
    let json = serde_json::to_value(&obj)?;
    Ok(Some(parse_kustomization(&json)))
}

fn parse_kustomization(obj: &serde_json::Value) -> KustomizationInfo {
    let status = obj.get("status");
    let str_field = |key: &str| {
        status
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    let last_applied_revision = str_field("lastAppliedRevision");
    let last_attempted_revision = str_field("lastAttemptedRevision");
    let ready_cond = status
        .and_then(|s| s.get("conditions"))
        .and_then(|c| c.as_array())
        .and_then(|conds| {
            conds
                .iter()
                .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("Ready"))
        });
    let ready = ready_cond
        .and_then(|c| c.get("status"))
        .and_then(|s| s.as_str())
        .and_then(|s| match s {
            "True" => Some(true),
            "False" => Some(false),
            _ => None,
        });
    let message = ready_cond
        .and_then(|c| c.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());
    KustomizationInfo {
        applied_sha: last_applied_revision
            .as_deref()
            .and_then(parse_revision_sha),
        last_applied_revision,
        last_attempted_revision,
        ready,
        message,
    }
}

/// The commit in a Flux revision string: `main@sha1:<sha>` (current) or
/// `main/<sha>` (older Flux).
pub(crate) fn parse_revision_sha(revision: &str) -> Option<String> {
    let sha = if let Some((_, rest)) = revision.rsplit_once("@sha1:") {
        rest
    } else if let Some((_, rest)) = revision.rsplit_once('/') {
        rest
    } else {
        revision
    };
    let sha = sha.trim();
    (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())).then(|| sha.to_string())
}

// ---- HelmRelease CRD types (partial, for deserialization) ----

/// Partial HelmRelease spec for deserialization via the dynamic API.
/// Matches helm.toolkit.fluxcd.io/v2.
#[derive(Debug, Deserialize)]
struct HelmReleaseCrd {
    metadata: HelmReleaseMetadata,
    #[serde(default)]
    spec: HelmReleaseSpec,
    #[serde(default)]
    status: Option<HelmReleaseStatus>,
}

#[derive(Debug, Deserialize)]
struct HelmReleaseMetadata {
    name: String,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    labels: Option<HashMap<String, String>>,
    #[serde(default)]
    generation: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct HelmReleaseSpec {
    #[serde(default)]
    chart: Option<HelmReleaseChart>,
    #[serde(default)]
    values: Option<serde_json::Value>,
    #[serde(default)]
    suspend: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct HelmReleaseChart {
    #[serde(default)]
    spec: Option<HelmChartSpec>,
}

#[derive(Debug, Deserialize)]
struct HelmChartSpec {
    #[serde(default)]
    chart: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HelmReleaseStatus {
    #[serde(default)]
    conditions: Option<Vec<HelmCondition>>,
    #[serde(default, rename = "lastAttemptedRevision")]
    last_attempted_revision: Option<String>,
    #[serde(default)]
    history: Option<Vec<HelmHistory>>,
    #[serde(default, rename = "observedGeneration")]
    observed_generation: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct HelmCondition {
    #[serde(default, rename = "type")]
    condition_type: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default, rename = "lastTransitionTime")]
    last_transition_time: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct HelmHistory {
    #[serde(default, rename = "chartVersion")]
    chart_version: Option<String>,
    #[serde(default, rename = "appVersion")]
    app_version: Option<String>,
}

/// Poll all HelmReleases across all namespaces using the dynamic API
async fn poll_helm_releases(client: &Client) -> Result<Vec<HelmReleaseInfo>> {
    use kube::api::{ApiResource, DynamicObject, GroupVersionKind};

    let gvk = GroupVersionKind::gvk("helm.toolkit.fluxcd.io", "v2", "HelmRelease");
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::all_with(client.clone(), &ar);

    let list = api
        .list(&ListParams::default())
        .await
        .context("Failed to list HelmReleases")?;

    let mut results = Vec::new();

    for obj in list.items {
        // Serialize the dynamic object to JSON, then parse our partial struct
        let json_val = serde_json::to_value(&obj)?;
        let hr: HelmReleaseCrd = match serde_json::from_value(json_val) {
            Ok(hr) => hr,
            Err(e) => {
                warn!(
                    name = obj.metadata.name.as_deref().unwrap_or("?"),
                    err = %e,
                    "Failed to parse HelmRelease"
                );
                continue;
            }
        };

        let namespace = hr
            .metadata
            .namespace
            .unwrap_or_else(|| "default".to_string());

        // Extract status from conditions
        let (status, status_message, last_transition) = extract_status(&hr.status);

        // Extract version info
        let (chart_name, chart_version) = hr
            .spec
            .chart
            .as_ref()
            .and_then(|c| c.spec.as_ref())
            .map(|s| (s.chart.clone(), s.version.clone()))
            .unwrap_or((None, None));

        // Every `<path>.tag` under values; charts with several images (a web
        // api/ingest pair, a worker) pin them under nested keys, and a leftover
        // top-level image.tag next to them is not what runs.
        let image_tags = hr
            .spec
            .values
            .as_ref()
            .map(nested_image_tags)
            .unwrap_or_default();
        let nested: Vec<&String> = image_tags
            .iter()
            .filter(|(k, _)| k.as_str() != "image")
            .map(|(_, v)| v)
            .collect();
        let image_tag = if nested.is_empty() {
            image_tags.get("image").cloned().or_else(|| {
                // Fleet instances may pin the app image under the operator's primary key
                hr.spec
                    .values
                    .as_ref()
                    .and_then(|v| v.get("target_image"))
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.rsplit(':').next())
                    .map(|s| s.to_string())
            })
        } else {
            common_tag(&nested)
        };

        let last_attempted_revision = hr
            .status
            .as_ref()
            .and_then(|s| s.last_attempted_revision.clone());

        let last_applied_revision = hr
            .status
            .as_ref()
            .and_then(|s| s.history.as_ref())
            .and_then(|h| h.first())
            .and_then(|h| h.chart_version.clone());

        let labels = hr.metadata.labels.unwrap_or_default();
        let app_name_label = labels.get("app.kubernetes.io/name").cloned();
        let kustomize_name = labels.get("kustomize.toolkit.fluxcd.io/name").cloned();

        let suspended = hr.spec.suspend.unwrap_or(false);

        let final_status = if suspended {
            DeploymentStatus::Suspended
        } else {
            status
        };

        let (ready, reconciling) = ready_and_reconciling(&hr.status);
        let observed_generation = hr.status.as_ref().and_then(|s| s.observed_generation);
        let replica_count = hr
            .spec
            .values
            .as_ref()
            .and_then(|v| v.get("replicaCount"))
            .and_then(|v| v.as_i64());
        let mut values_images = hr
            .spec
            .values
            .as_ref()
            .map(values_image_map)
            .unwrap_or_default();
        values_images.extend(image_tags);

        results.push(HelmReleaseInfo {
            name: hr.metadata.name,
            namespace,
            status: final_status,
            status_message,
            chart_version,
            image_tag,
            chart_name,
            last_attempted_revision,
            last_applied_revision,
            last_transition_time: last_transition,
            suspended,
            app_name_label,
            kustomize_name,
            ready,
            reconciling,
            generation: hr.metadata.generation,
            observed_generation,
            replica_count,
            values_images,
        });
    }

    Ok(results)
}

/// Top-level `*_image` values plus `target_image`, as the operator writes them
/// into every project HelmRelease.
fn values_image_map(values: &serde_json::Value) -> std::collections::BTreeMap<String, String> {
    values
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(k, _)| k.ends_with("_image") || k.as_str() == "target_image")
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Every mapping under values with a string `tag`, keyed by its dotted path
/// (`image`, `api.image`, `worker.image`), valued `tag` or `tag@digest` when a
/// sibling `digest` exists and the tag does not already carry one.
pub(crate) fn nested_image_tags(
    values: &serde_json::Value,
) -> std::collections::BTreeMap<String, String> {
    fn walk(
        node: &serde_json::Value,
        path: &mut Vec<String>,
        out: &mut std::collections::BTreeMap<String, String>,
    ) {
        let Some(map) = node.as_object() else {
            return;
        };
        if let Some(tag) = map.get("tag").and_then(|t| t.as_str()) {
            let digest = map.get("digest").and_then(|d| d.as_str());
            let value = match digest {
                Some(d) if !tag.contains('@') => format!("{}@{}", tag, d),
                _ => tag.to_string(),
            };
            out.insert(path.join("."), value);
        }
        for (k, v) in map {
            if v.is_object() {
                path.push(k.clone());
                walk(v, path, out);
                path.pop();
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(values, &mut Vec::new(), &mut out);
    out
}

/// The one tag several images share (digests differ per image, so only the
/// tag counts); None when they differ and the UI falls back to the chart.
fn common_tag(tags: &[&String]) -> Option<String> {
    let first = tags.first()?;
    let bare = |t: &str| t.split('@').next().unwrap_or(t).to_string();
    tags.iter()
        .all(|t| bare(t) == bare(first))
        .then(|| (*first).clone())
}

/// Ready condition as a tri-state, and whether the release is mid-rollout.
fn ready_and_reconciling(status: &Option<HelmReleaseStatus>) -> (Option<bool>, bool) {
    let Some(conditions) = status.as_ref().and_then(|s| s.conditions.as_ref()) else {
        return (None, false);
    };
    let ready = conditions
        .iter()
        .find(|c| c.condition_type == "Ready")
        .map(|c| c.status.as_str());
    let ready_value = match ready {
        Some("True") => Some(true),
        Some("False") => Some(false),
        _ => None,
    };
    let reconciling = conditions
        .iter()
        .any(|c| c.condition_type == "Reconciling" && c.status == "True")
        || ready == Some("Unknown");
    (ready_value, reconciling)
}

/// Extract status, message, and transition time from HelmRelease conditions
fn extract_status(
    status: &Option<HelmReleaseStatus>,
) -> (DeploymentStatus, Option<String>, Option<DateTime<Utc>>) {
    let Some(status) = status else {
        return (DeploymentStatus::Unknown, None, None);
    };
    let Some(ref conditions) = status.conditions else {
        return (DeploymentStatus::Unknown, None, None);
    };

    // Find the Ready condition
    let ready = conditions.iter().find(|c| c.condition_type == "Ready");

    match ready {
        Some(cond) => {
            let deployment_status = match cond.status.as_str() {
                "True" => DeploymentStatus::Healthy,
                "False" => {
                    // Check reason for more specific status
                    match cond.reason.as_deref() {
                        Some("ProgressingWithRetry" | "ArtifactFailed" | "DependencyNotReady") => {
                            DeploymentStatus::Progressing
                        }
                        _ => DeploymentStatus::Degraded,
                    }
                }
                _ => DeploymentStatus::Unknown,
            };

            let transition = cond
                .last_transition_time
                .as_ref()
                .and_then(|t| t.parse::<DateTime<Utc>>().ok());

            (deployment_status, cond.message.clone(), transition)
        }
        None => (DeploymentStatus::Unknown, None, None),
    }
}

/// Poll all Pods across all namespaces
async fn poll_pods(client: &Client) -> Result<Vec<PodInfo>> {
    let pods_api: Api<Pod> = Api::all(client.clone());
    let list = pods_api
        .list(&ListParams::default())
        .await
        .context("Failed to list Pods")?;

    let mut results = Vec::new();

    for pod in list.items {
        let metadata = &pod.metadata;
        let name = metadata.name.clone().unwrap_or_default();
        let namespace = metadata.namespace.clone().unwrap_or_default();

        // Skip kube-system and other system namespaces
        if namespace == "kube-system"
            || namespace == "kube-node-lease"
            || namespace == "kube-public"
        {
            continue;
        }

        let spec = pod.spec.as_ref();
        let status = pod.status.as_ref();

        let phase = status
            .and_then(|s| s.phase.clone())
            .unwrap_or_else(|| "Unknown".to_string());

        // Compute the display status (may differ from phase)
        let (display_status, reason) = compute_pod_status(status);

        // Count restarts
        let restarts = status
            .and_then(|s| s.container_statuses.as_ref())
            .map(|cs| cs.iter().map(|c| c.restart_count).sum())
            .unwrap_or(0);

        // Ready check
        let (ready, ready_count) = compute_readiness(status);

        // Container images
        let images = spec
            .map(|s| {
                s.containers
                    .iter()
                    .map(|c| c.image.clone().unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default();

        let node_name = spec.and_then(|s| s.node_name.clone());
        let created = metadata.creation_timestamp.as_ref().map(|t| t.0);

        // containerd reports status.image as a bare "sha256:..." once the
        // image was pulled by digest, so the spec image keeps the repo name.
        let spec_images: std::collections::HashMap<String, String> = spec
            .map(|s| {
                s.containers
                    .iter()
                    .chain(s.init_containers.iter().flatten())
                    .filter_map(|c| c.image.clone().map(|i| (c.name.clone(), i)))
                    .collect()
            })
            .unwrap_or_default();
        let volume_images: Vec<String> = spec
            .and_then(|s| s.volumes.as_ref())
            .map(|vols| {
                vols.iter()
                    .filter_map(|v| v.image.as_ref().and_then(|i| i.reference.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let mut containers = Vec::new();
        if let Some(status) = status {
            for (list, init) in [
                (status.init_container_statuses.as_deref(), true),
                (status.container_statuses.as_deref(), false),
            ] {
                for cs in list.unwrap_or(&[]) {
                    containers.push(ContainerStatusInfo {
                        name: cs.name.clone(),
                        image: spec_images
                            .get(&cs.name)
                            .cloned()
                            .unwrap_or_else(|| cs.image.clone()),
                        image_id: cs.image_id.clone(),
                        init,
                        ready: cs.ready,
                    });
                }
            }
        }

        // Parse resource requests/limits from container specs
        let (cpu_request, cpu_limit, mem_request, mem_limit) = spec
            .map(|s| {
                let mut cpu_req = 0.0_f64;
                let mut cpu_lim = 0.0_f64;
                let mut mem_req = 0_u64;
                let mut mem_lim = 0_u64;
                let mut has_cpu_req = false;
                let mut has_cpu_lim = false;
                let mut has_mem_req = false;
                let mut has_mem_lim = false;
                for c in &s.containers {
                    if let Some(ref res) = c.resources {
                        if let Some(ref requests) = res.requests {
                            if let Some(cpu) = requests.get("cpu") {
                                cpu_req += parse_cpu_quantity(&cpu.0);
                                has_cpu_req = true;
                            }
                            if let Some(mem) = requests.get("memory") {
                                mem_req += parse_memory_quantity(&mem.0);
                                has_mem_req = true;
                            }
                        }
                        if let Some(ref limits) = res.limits {
                            if let Some(cpu) = limits.get("cpu") {
                                cpu_lim += parse_cpu_quantity(&cpu.0);
                                has_cpu_lim = true;
                            }
                            if let Some(mem) = limits.get("memory") {
                                mem_lim += parse_memory_quantity(&mem.0);
                                has_mem_lim = true;
                            }
                        }
                    }
                }
                (
                    if has_cpu_req { Some(cpu_req) } else { None },
                    if has_cpu_lim { Some(cpu_lim) } else { None },
                    if has_mem_req { Some(mem_req) } else { None },
                    if has_mem_lim { Some(mem_lim) } else { None },
                )
            })
            .unwrap_or((None, None, None, None));

        results.push(PodInfo {
            name,
            namespace,
            phase,
            status: display_status,
            restarts,
            started_at: created,
            created,
            ready,
            ready_count,
            images,
            node_name,
            reason,
            cpu_request_millicores: cpu_request,
            cpu_limit_millicores: cpu_limit,
            memory_request_bytes: mem_request,
            memory_limit_bytes: mem_limit,
            containers,
            volume_images,
        });
    }

    Ok(results)
}

/// Compute display status and reason from pod status
fn compute_pod_status(
    status: Option<&k8s_openapi::api::core::v1::PodStatus>,
) -> (String, Option<String>) {
    let Some(status) = status else {
        return ("Unknown".to_string(), None);
    };

    // Check container statuses for waiting/terminated reasons
    if let Some(ref container_statuses) = status.container_statuses {
        for cs in container_statuses {
            if let Some(ref state) = cs.state {
                if let Some(ref waiting) = state.waiting {
                    if let Some(ref reason) = waiting.reason {
                        return (reason.clone(), Some(reason.clone()));
                    }
                }
                if let Some(ref terminated) = state.terminated {
                    if let Some(ref reason) = terminated.reason {
                        return (reason.clone(), Some(reason.clone()));
                    }
                }
            }
        }
    }

    // Check init container statuses
    if let Some(ref init_statuses) = status.init_container_statuses {
        for cs in init_statuses {
            if let Some(ref state) = cs.state {
                if let Some(ref waiting) = state.waiting {
                    if let Some(ref reason) = waiting.reason {
                        return (format!("Init:{}", reason), Some(reason.clone()));
                    }
                }
            }
        }
    }

    let phase = status
        .phase
        .clone()
        .unwrap_or_else(|| "Unknown".to_string());
    (phase, None)
}

/// Compute ready status and ready count string (e.g. "1/2")
fn compute_readiness(status: Option<&k8s_openapi::api::core::v1::PodStatus>) -> (bool, String) {
    let Some(status) = status else {
        return (false, "0/0".to_string());
    };

    let container_statuses = status.container_statuses.as_deref().unwrap_or(&[]);
    let total = container_statuses.len();
    let ready_count = container_statuses.iter().filter(|c| c.ready).count();
    let all_ready = total > 0 && ready_count == total;

    (all_ready, format!("{}/{}", ready_count, total))
}

/// Parse a K8s CPU quantity string to millicores.
/// Examples: "100m" → 100.0, "1" → 1000.0, "0.5" → 500.0, "250m" → 250.0
pub fn parse_cpu_quantity(s: &str) -> f64 {
    if let Some(m) = s.strip_suffix('m') {
        m.parse::<f64>().unwrap_or(0.0)
    } else if let Some(n) = s.strip_suffix('n') {
        n.parse::<f64>().unwrap_or(0.0) / 1_000_000.0
    } else if let Some(u) = s.strip_suffix('u') {
        u.parse::<f64>().unwrap_or(0.0) / 1_000.0
    } else {
        // Whole cores
        s.parse::<f64>().unwrap_or(0.0) * 1000.0
    }
}

/// Parse a K8s memory quantity string to bytes.
/// Examples: "128Mi" → 134217728, "1Gi" → 1073741824, "512000Ki" → 524288000
pub fn parse_memory_quantity(s: &str) -> u64 {
    if let Some(v) = s.strip_suffix("Gi") {
        (v.parse::<f64>().unwrap_or(0.0) * 1024.0 * 1024.0 * 1024.0) as u64
    } else if let Some(v) = s.strip_suffix("Mi") {
        (v.parse::<f64>().unwrap_or(0.0) * 1024.0 * 1024.0) as u64
    } else if let Some(v) = s.strip_suffix("Ki") {
        (v.parse::<f64>().unwrap_or(0.0) * 1024.0) as u64
    } else if let Some(v) = s.strip_suffix('G') {
        (v.parse::<f64>().unwrap_or(0.0) * 1_000_000_000.0) as u64
    } else if let Some(v) = s.strip_suffix('M') {
        (v.parse::<f64>().unwrap_or(0.0) * 1_000_000.0) as u64
    } else if let Some(v) = s.strip_suffix('K') {
        (v.parse::<f64>().unwrap_or(0.0) * 1_000.0) as u64
    } else if let Some(v) = s.strip_suffix("Ti") {
        (v.parse::<f64>().unwrap_or(0.0) * 1024.0 * 1024.0 * 1024.0 * 1024.0) as u64
    } else {
        // Raw bytes
        s.parse::<u64>().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_sha_formats() {
        assert_eq!(
            parse_revision_sha("main@sha1:4c75e35b1234567890abcdef1234567890abcdef").as_deref(),
            Some("4c75e35b1234567890abcdef1234567890abcdef")
        );
        assert_eq!(
            parse_revision_sha("main/4c75e35b1234567890abcdef1234567890abcdef").as_deref(),
            Some("4c75e35b1234567890abcdef1234567890abcdef")
        );
        assert_eq!(parse_revision_sha("main@sha1:notahash"), None);
        assert_eq!(parse_revision_sha(""), None);
    }

    #[test]
    fn kustomization_parses_status() {
        let obj = serde_json::json!({
            "metadata": {"name": "apps"},
            "status": {
                "lastAppliedRevision": "main@sha1:4c75e35b1234567890abcdef1234567890abcdef",
                "lastAttemptedRevision": "main@sha1:aaaaaaaa1234567890abcdef1234567890abcdef",
                "conditions": [
                    {"type": "Ready", "status": "False", "message": "kustomize build failed"}
                ]
            }
        });
        let k = parse_kustomization(&obj);
        assert_eq!(
            k.applied_sha.as_deref(),
            Some("4c75e35b1234567890abcdef1234567890abcdef")
        );
        assert_eq!(
            k.last_attempted_revision.as_deref(),
            Some("main@sha1:aaaaaaaa1234567890abcdef1234567890abcdef")
        );
        assert_eq!(k.ready, Some(false));
        assert_eq!(k.message.as_deref(), Some("kustomize build failed"));

        let empty = parse_kustomization(&serde_json::json!({"metadata": {"name": "apps"}}));
        assert_eq!(empty, KustomizationInfo::default());
    }

    #[test]
    fn values_images_keeps_only_image_keys() {
        let values = serde_json::json!({
            "engine_image": "ghcr.io/notes-co/engine:pr-612@sha256:abc",
            "target_image": "ghcr.io/notes-co/engine:pr-612@sha256:abc",
            "broker_image": "ghcr.io/notes-co/broker:pr-620@sha256:def",
            "replicaCount": 0,
            "log": {"filter": "info"}
        });
        let map = values_image_map(&values);
        assert_eq!(map.len(), 3);
        assert_eq!(
            map["broker_image"],
            "ghcr.io/notes-co/broker:pr-620@sha256:def"
        );
    }

    #[test]
    fn reconciling_from_conditions() {
        let status = |conds: Vec<(&str, &str)>| {
            Some(HelmReleaseStatus {
                conditions: Some(
                    conds
                        .into_iter()
                        .map(|(t, s)| HelmCondition {
                            condition_type: t.to_string(),
                            status: s.to_string(),
                            reason: None,
                            message: None,
                            last_transition_time: None,
                        })
                        .collect(),
                ),
                last_attempted_revision: None,
                history: None,
                observed_generation: None,
            })
        };
        assert_eq!(
            ready_and_reconciling(&status(vec![("Ready", "True")])),
            (Some(true), false)
        );
        assert_eq!(
            ready_and_reconciling(&status(vec![("Ready", "Unknown")])),
            (None, true)
        );
        assert_eq!(
            ready_and_reconciling(&status(vec![("Ready", "False"), ("Reconciling", "True")])),
            (Some(false), true)
        );
        assert_eq!(ready_and_reconciling(&None), (None, false));
    }
}

#[cfg(test)]
mod nested_image_tests {
    use super::*;

    #[test]
    fn nested_pins_win_over_a_leftover_top_level_tag() {
        let values = serde_json::json!({
            "image": {"tag": "pr-9@sha256:aaaa"},
            "api": {"image": {"tag": "0.1.0-468a3380aff71d3ccffc400da967967d6786711c@sha256:8027"}},
            "ingest": {"image": {"tag": "0.1.0-468a3380aff71d3ccffc400da967967d6786711c@sha256:0a5c"}}
        });
        let tags = nested_image_tags(&values);
        assert_eq!(tags.len(), 3);
        assert!(tags["api.image"].starts_with("0.1.0-468a3380"));
        let nested: Vec<&String> = tags
            .iter()
            .filter(|(k, _)| k.as_str() != "image")
            .map(|(_, v)| v)
            .collect();
        // Different digests per image, same tag: the shared tag is the version
        assert_eq!(
            common_tag(&nested)
                .as_deref()
                .map(|t| t.split('@').next().unwrap()),
            Some("0.1.0-468a3380aff71d3ccffc400da967967d6786711c")
        );
    }

    #[test]
    fn sibling_digest_is_attached_and_mixed_tags_have_no_common_version() {
        let values = serde_json::json!({
            "worker": {"image": {"tag": "tree-aaaa", "digest": "sha256:1111"}},
            "ui": {"image": {"tag": "tree-bbbb", "digest": "sha256:2222"}}
        });
        let tags = nested_image_tags(&values);
        assert_eq!(tags["worker.image"], "tree-aaaa@sha256:1111");
        let nested: Vec<&String> = tags.values().collect();
        assert_eq!(common_tag(&nested), None);
    }
}
