//! Compile-time branding. These must be known before config.toml is found
//! (ProjectDirs decides where config.toml lives), so they come from
//! `CARAVAN_BRAND_*` environment variables at build time, with the public
//! caravan defaults when unset.

/// Short lowercase name: ProjectDirs application, keyring service, app id.
pub const APP_NAME: &str = match option_env!("CARAVAN_BRAND_APP_NAME") {
    Some(v) => v,
    None => "caravan",
};

/// Human name for window titles and the macOS bundle.
pub const DISPLAY_NAME: &str = match option_env!("CARAVAN_BRAND_DISPLAY_NAME") {
    Some(v) => v,
    None => "Caravan",
};

/// ProjectDirs qualifier (reverse-DNS first label).
pub const QUALIFIER: &str = match option_env!("CARAVAN_BRAND_QUALIFIER") {
    Some(v) => v,
    None => "dev",
};

/// ProjectDirs organization.
pub const ORG: &str = match option_env!("CARAVAN_BRAND_ORG") {
    Some(v) => v,
    None => "caravan",
};

/// macOS bundle identifier (used by the packaging workflow, mirrored here).
#[allow(dead_code)]
pub const BUNDLE_ID: &str = match option_env!("CARAVAN_BRAND_BUNDLE_ID") {
    Some(v) => v,
    None => "dev.caravan.app",
};

/// DMG volume name the self-updater mounts (macOS only).
#[allow(dead_code)]
pub const VOLUME_NAME: &str = match option_env!("CARAVAN_BRAND_VOLUME_NAME") {
    Some(v) => v,
    None => "Caravan",
};

/// Window title.
pub fn window_title() -> String {
    format!("{DISPLAY_NAME} GitOps Dashboard")
}

/// The per-user ProjectDirs for this brand.
pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from(QUALIFIER, ORG, APP_NAME)
}
