use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Status of a HelmRelease or deployment
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DeploymentStatus {
    /// Ready=True, all pods healthy
    Healthy,
    /// Reconciling / in progress
    Progressing,
    /// Ready=False or pods failing
    Degraded,
    /// Hard failure
    Failed,
    /// Suspended by user
    Suspended,
    /// Not deployed in this environment
    NotDeployed,
    /// Unknown / not yet polled
    #[default]
    Unknown,
}

impl std::fmt::Display for DeploymentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "Healthy"),
            Self::Progressing => write!(f, "Progressing"),
            Self::Degraded => write!(f, "Degraded"),
            Self::Failed => write!(f, "Failed"),
            Self::Suspended => write!(f, "Suspended"),
            Self::NotDeployed => write!(f, "Not Deployed"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Information extracted from a Flux HelmRelease custom resource
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HelmReleaseInfo {
    /// HelmRelease metadata.name (e.g. "notes-api", "project-23070ea5-...")
    pub name: String,
    /// HelmRelease metadata.namespace
    pub namespace: String,
    /// Overall status derived from conditions
    pub status: DeploymentStatus,
    /// Human-readable status message from the Ready condition
    pub status_message: Option<String>,

    // Version info from spec
    /// Chart version from spec.chart.spec.version (e.g. "0.1.0-2b5ba6be...")
    pub chart_version: Option<String>,
    /// Image tag from spec.values.image.tag (e.g. "sha-2b5ba6be...")
    pub image_tag: Option<String>,
    /// Chart name from spec.chart.spec.chart (e.g. "notes-api")
    pub chart_name: Option<String>,

    // Status info
    /// Last attempted revision from status
    pub last_attempted_revision: Option<String>,
    /// Last applied revision from status history
    pub last_applied_revision: Option<String>,
    /// When the Ready condition last transitioned
    pub last_transition_time: Option<DateTime<Utc>>,
    /// Whether the HelmRelease is suspended
    pub suspended: bool,

    // Labels for classification
    /// The app.kubernetes.io/name label if present (helps identify fleet instances)
    pub app_name_label: Option<String>,
    /// The kustomize source label
    pub kustomize_name: Option<String>,

    /// Ready condition status (None when the condition is missing)
    #[serde(default)]
    pub ready: Option<bool>,
    /// A Reconciling condition is True, or Ready is Unknown
    #[serde(default)]
    pub reconciling: bool,
    /// metadata.generation
    #[serde(default)]
    pub generation: Option<i64>,
    /// status.observedGeneration
    #[serde(default)]
    pub observed_generation: Option<i64>,
    /// spec.values.replicaCount
    #[serde(default)]
    pub replica_count: Option<i64>,
    /// Top-level spec.values keys ending in `_image`, plus `target_image`,
    /// mapped to the full image reference
    #[serde(default)]
    pub values_images: BTreeMap<String, String>,
}

impl HelmReleaseInfo {
    /// Extract the short commit SHA from the image tag or chart version.
    /// Image tags are like "sha-2b5ba6be2808cadf5e0414f6fa817c76fd79b952"
    /// Chart versions are like "0.1.0-2b5ba6be2808cadf5e0414f6fa817c76fd79b952"
    pub fn commit_sha(&self) -> Option<String> {
        // Try image tag first: "sha-<full_sha>"
        if let Some(ref tag) = self.image_tag {
            if let Some(sha) = tag.strip_prefix("sha-") {
                return Some(sha[..sha.len().min(8)].to_string());
            }
        }
        // Try chart version: "0.1.0-<full_sha>"
        if let Some(ref ver) = self.chart_version {
            if let Some(pos) = ver.rfind('-') {
                let sha_part = &ver[pos + 1..];
                // Only if it looks like a hex SHA (> 8 chars)
                if sha_part.len() >= 8 && sha_part.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(sha_part[..8].to_string());
                }
            }
        }
        None
    }

    /// Full commit SHA if available
    pub fn full_commit_sha(&self) -> Option<String> {
        if let Some(ref tag) = self.image_tag {
            let tag = tag.split('@').next().unwrap_or(tag);
            if let Some(sha) = tag.strip_prefix("sha-") {
                return Some(sha.to_string());
            }
            // `1.0.10-<sha>` style tags carry the commit after the last dash;
            // tree- and pr- tags do not.
            if !tag.starts_with("tree-") && !tag.starts_with("pr-") {
                if let Some((_, suffix)) = tag.rsplit_once('-') {
                    if suffix.len() >= 8 && suffix.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Some(suffix.to_string());
                    }
                }
            }
        }
        if let Some(ref ver) = self.chart_version {
            if let Some(pos) = ver.rfind('-') {
                let sha_part = &ver[pos + 1..];
                if sha_part.len() >= 8 && sha_part.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(sha_part.to_string());
                }
            }
        }
        None
    }

    /// Whether this HelmRelease is a fleet instance (see `crate::fleet`)
    #[allow(dead_code)]
    pub fn is_fleet_instance(&self) -> bool {
        crate::fleet::provider().is_instance(self)
    }
}

/// Information about a single Kubernetes pod
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodInfo {
    /// Pod name
    pub name: String,
    /// Pod namespace
    pub namespace: String,
    /// Pod phase (Running, Pending, Succeeded, Failed, Unknown)
    pub phase: String,
    /// Human-readable status (may include container statuses like CrashLoopBackOff)
    pub status: String,
    /// Total container restart count
    pub restarts: i32,
    /// Pod creation timestamp
    pub created: Option<DateTime<Utc>>,
    /// Alias for created, used by UI
    pub started_at: Option<DateTime<Utc>>,
    /// Whether the pod is ready (all containers ready)
    pub ready: bool,
    /// Number of ready containers / total containers (e.g. "1/1")
    pub ready_count: String,
    /// Container image(s) in this pod
    pub images: Vec<String>,
    /// The node this pod is running on
    pub node_name: Option<String>,
    /// Reason for current state (e.g. "OOMKilled", "CrashLoopBackOff")
    pub reason: Option<String>,
    /// Total CPU request across all containers (millicores)
    #[serde(default)]
    pub cpu_request_millicores: Option<f64>,
    /// Total CPU limit across all containers (millicores)
    #[serde(default)]
    pub cpu_limit_millicores: Option<f64>,
    /// Total memory request across all containers (bytes)
    #[serde(default)]
    pub memory_request_bytes: Option<u64>,
    /// Total memory limit across all containers (bytes)
    #[serde(default)]
    pub memory_limit_bytes: Option<u64>,
    /// Per-container status, init containers included
    #[serde(default)]
    pub containers: Vec<ContainerStatusInfo>,
    /// Image volumes mounted by the pod (OCI images used as data, e.g. rogues)
    #[serde(default)]
    pub volume_images: Vec<String>,
}

/// One entry of status.containerStatuses or status.initContainerStatuses
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerStatusInfo {
    pub name: String,
    /// Image as the kubelet reports it
    pub image: String,
    /// The `imageID` field, usually `<repo>@sha256:<digest>`; may be empty
    pub image_id: String,
    pub init: bool,
    pub ready: bool,
}

/// Resource usage metrics for a single pod (from metrics.k8s.io API)
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PodMetrics {
    /// Pod name
    pub name: String,
    /// Pod namespace
    pub namespace: String,
    /// Total CPU usage across all containers (millicores)
    pub cpu_millicores: f64,
    /// Total memory usage across all containers (bytes)
    pub memory_bytes: u64,
    /// When this metric was collected
    pub timestamp: DateTime<Utc>,
}

impl PodInfo {
    /// Whether this pod is in a problematic state
    pub fn is_problematic(&self) -> bool {
        let bad_statuses = [
            "CrashLoopBackOff",
            "OOMKilled",
            "ImagePullBackOff",
            "ErrImagePull",
            "Error",
            "Failed",
            "CreateContainerConfigError",
        ];
        bad_statuses
            .iter()
            .any(|s| self.status.contains(s) || self.reason.as_deref() == Some(s))
            || self.phase == "Failed"
            || self.restarts > 5
    }
}

#[cfg(test)]
mod commit_sha_tests {
    use super::*;

    fn hr(tag: Option<&str>, chart: Option<&str>) -> HelmReleaseInfo {
        HelmReleaseInfo {
            image_tag: tag.map(str::to_string),
            chart_version: chart.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn version_suffixed_tags_carry_their_commit() {
        let h = hr(
            Some("1.0.10-0158394a64a322f9140ec21e0cd8822799a22d01@sha256:ee50"),
            Some("0.1.0-61ace0a101a475d9bf2b7c63da376a5ff88895d1"),
        );
        assert_eq!(
            h.full_commit_sha().as_deref(),
            Some("0158394a64a322f9140ec21e0cd8822799a22d01")
        );
        // pr- and tree- tags fall back to the chart commit
        let h = hr(
            Some("pr-11@sha256:aaaa"),
            Some("0.1.0-61ace0a101a475d9bf2b7c63da376a5ff88895d1"),
        );
        assert_eq!(
            h.full_commit_sha().as_deref(),
            Some("61ace0a101a475d9bf2b7c63da376a5ff88895d1")
        );
        let h = hr(Some("tree-d29f9e94aaaaaaaa"), None);
        assert_eq!(h.full_commit_sha(), None);
    }
}
