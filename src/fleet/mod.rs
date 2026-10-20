//! The fleet boundary: a deployment may run many instances of one product
//! (a multi-tenant operator, a per-customer chart) that the console should
//! aggregate rather than list as services. What counts as an instance, which
//! repo builds which image, and whether there is a fleet view at all are
//! product knowledge a `FleetProvider` supplies. The provider is chosen by the
//! build; the default is `NoFleet`.

use crate::models::deployment::HelmReleaseInfo;

#[allow(dead_code)]
pub trait FleetProvider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Whether a HelmRelease is a fleet instance (aggregated, not a service).
    fn is_instance(&self, hr: &HelmReleaseInfo) -> bool;

    /// The GitHub repo a service is built from.
    fn repo_for_service(&self, service: &str) -> String;

    /// The GitHub repo a container image is built from.
    fn repo_for_image(&self, image_name: &str) -> String;

    /// The repo whose one release pins several images, when the fleet has one.
    fn multi_image_repo(&self) -> Option<&'static str>;

    /// Name of the aggregate matrix row for instances, when the fleet shows one.
    fn instances_row(&self) -> Option<&'static str>;
}

/// No fleet: every HelmRelease is a service built from a repo of its name.
pub struct NoFleet;

impl FleetProvider for NoFleet {
    fn name(&self) -> &'static str {
        "none"
    }
    fn is_instance(&self, _hr: &HelmReleaseInfo) -> bool {
        false
    }
    fn repo_for_service(&self, service: &str) -> String {
        service.to_string()
    }
    fn repo_for_image(&self, image_name: &str) -> String {
        image_name.to_string()
    }
    fn multi_image_repo(&self) -> Option<&'static str> {
        None
    }
    fn instances_row(&self) -> Option<&'static str> {
        None
    }
}

static PROVIDER: NoFleet = NoFleet;

pub fn provider() -> &'static dyn FleetProvider {
    &PROVIDER
}

pub fn repo_for_service(service: &str) -> String {
    provider().repo_for_service(service)
}

pub fn repo_for_image(image_name: &str) -> String {
    provider().repo_for_image(image_name)
}

pub fn multi_image_repo() -> Option<&'static str> {
    provider().multi_image_repo()
}

pub fn instances_row() -> Option<&'static str> {
    provider().instances_row()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_maps_repos() {
        let p = provider();
        assert_eq!(p.repo_for_service("notes-web"), "notes-web");
        assert_eq!(p.repo_for_image("notes-api"), "notes-api");
        assert_eq!(p.instances_row(), None);
        assert_eq!(p.multi_image_repo(), None);
    }
}
