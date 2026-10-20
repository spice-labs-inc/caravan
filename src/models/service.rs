use std::collections::HashMap;

use super::deployment::{DeploymentStatus, HelmReleaseInfo, PodInfo};

/// A logical service that may be deployed across multiple environments.
/// Built by aggregating data from all environment polls.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Service {
    /// Service name (e.g. "notes-api", "notes-web")
    pub name: String,
    /// Display name override from manifest
    pub display_name: Option<String>,
    /// Service group for UI organization (e.g. "core", "compute")
    pub group: Option<String>,
    /// Service kind
    pub kind: ServiceKind,
    /// Per-environment deployment info
    pub deployments: HashMap<String, ServiceDeployment>,
}

/// What kind of service this is
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceKind {
    /// Regular app deployed via Flux (web, api, etc.)
    Application,
    /// Infrastructure service (cert-manager, envoy-gateway, etc.)
    Infrastructure,
    /// Fleet: has dynamic project-* instances
    Fleet,
}

/// Deployment info for a service in a single environment
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ServiceDeployment {
    /// The HelmRelease info for this service in this env
    pub helm_release: HelmReleaseInfo,
    /// Pods belonging to this service in this env
    pub pods: Vec<PodInfo>,
}

#[allow(dead_code)]
impl ServiceDeployment {
    /// Overall status combining HelmRelease + pods
    pub fn overall_status(&self) -> DeploymentStatus {
        // If HelmRelease is unhealthy, that takes priority
        if self.helm_release.status == DeploymentStatus::Degraded {
            return DeploymentStatus::Degraded;
        }
        if self.helm_release.suspended {
            return DeploymentStatus::Suspended;
        }
        // Check pods
        if self.pods.iter().any(|p| p.is_problematic()) {
            return DeploymentStatus::Degraded;
        }
        if self.helm_release.status == DeploymentStatus::Progressing {
            return DeploymentStatus::Progressing;
        }
        self.helm_release.status.clone()
    }

    /// Short version string for display (commit SHA or chart version)
    pub fn version_display(&self) -> String {
        if let Some(sha) = self.helm_release.commit_sha() {
            sha
        } else if let Some(ref ver) = self.helm_release.chart_version {
            ver.clone()
        } else {
            "unknown".to_string()
        }
    }
}

#[allow(dead_code)]
impl Service {
    /// Construct a Service from its name and collected per-env data
    pub fn new(name: String, kind: ServiceKind) -> Self {
        Self {
            name,
            display_name: None,
            group: None,
            kind,
            deployments: HashMap::new(),
        }
    }

    /// Display name (falls back to name)
    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// Whether this service is deployed in a given environment
    pub fn is_deployed_in(&self, env: &str) -> bool {
        self.deployments.contains_key(env)
    }

    /// Get the worst status across all environments
    pub fn worst_status(&self) -> DeploymentStatus {
        let mut worst = DeploymentStatus::Healthy;
        for dep in self.deployments.values() {
            let s = dep.overall_status();
            match s {
                DeploymentStatus::Degraded => return DeploymentStatus::Degraded,
                DeploymentStatus::Progressing => worst = DeploymentStatus::Progressing,
                DeploymentStatus::Suspended if worst == DeploymentStatus::Healthy => {
                    worst = DeploymentStatus::Suspended;
                }
                _ => {}
            }
        }
        worst
    }

    /// Whether versions differ between environments (indicates drift)
    pub fn has_version_drift(&self) -> bool {
        let shas: Vec<_> = self
            .deployments
            .values()
            .filter_map(|d| d.helm_release.full_commit_sha())
            .collect();
        if shas.len() < 2 {
            return false;
        }
        shas.windows(2).any(|w| w[0] != w[1])
    }
}
