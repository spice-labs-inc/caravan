use chrono::{DateTime, Utc};
use std::collections::HashMap;

use super::deployment::{HelmReleaseInfo, PodInfo};

/// Represents the state of a single K8s environment (one per rung of the ladder)
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct EnvironmentState {
    /// All HelmReleases discovered in this environment
    pub helm_releases: Vec<HelmReleaseInfo>,
    /// All pods in tracked namespaces
    pub pods: Vec<PodInfo>,
    /// Whether we're currently connected to this cluster
    pub connected: bool,
    /// Last successful poll timestamp
    pub last_poll: Option<DateTime<Utc>>,
    /// Connection error if any
    pub error: Option<String>,
}

#[allow(dead_code)]
impl EnvironmentState {
    /// Get HelmReleases grouped by namespace
    pub fn helm_releases_by_namespace(&self) -> HashMap<String, Vec<&HelmReleaseInfo>> {
        let mut map: HashMap<String, Vec<&HelmReleaseInfo>> = HashMap::new();
        for hr in &self.helm_releases {
            map.entry(hr.namespace.clone()).or_default().push(hr);
        }
        map
    }

    /// Get pods for a specific namespace/service
    pub fn pods_for_namespace(&self, namespace: &str) -> Vec<&PodInfo> {
        self.pods
            .iter()
            .filter(|p| p.namespace == namespace)
            .collect()
    }

    /// Find a HelmRelease by name
    pub fn find_helm_release(&self, name: &str) -> Option<&HelmReleaseInfo> {
        self.helm_releases.iter().find(|hr| hr.name == name)
    }

    /// Get all fleet instances
    pub fn fleet_instances(&self) -> Vec<&HelmReleaseInfo> {
        self.helm_releases
            .iter()
            .filter(|hr| hr.is_fleet_instance())
            .collect()
    }

    /// Count of healthy vs total fleet instances
    pub fn fleet_health_summary(&self) -> (usize, usize) {
        let instances = self.fleet_instances();
        let total = instances.len();
        let healthy = instances
            .iter()
            .filter(|hr| hr.status == super::deployment::DeploymentStatus::Healthy)
            .count();
        (healthy, total)
    }

    /// Get non-instance HelmReleases (regular services + infra)
    pub fn service_helm_releases(&self) -> Vec<&HelmReleaseInfo> {
        self.helm_releases
            .iter()
            .filter(|hr| !hr.is_fleet_instance())
            .collect()
    }
}
