mod actions;
mod app;
mod auth;
mod branding;
mod cache;
mod config;
mod fleet;
mod gitops;
mod gitops_mirror;
mod instance;
mod models;
mod notify;
mod pollers;
mod screenshots;
mod state;
mod ui;

use anyhow::Result;

/// App version: uses CARAVAN_VERSION from CI if set, otherwise "0.0.0" for dev builds.
pub fn app_version() -> &'static str {
    option_env!("CARAVAN_VERSION").unwrap_or("0.0.0")
}
use tracing_subscriber::EnvFilter;

/// Custom tracing timer that outputs local time with timezone.
struct LocalTimer;

impl tracing_subscriber::fmt::time::FormatTime for LocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        let now = chrono::Local::now();
        write!(w, "{}", now.format("%Y-%m-%d %H:%M:%S%.3f %Z"))
    }
}

fn load_icon() -> Option<eframe::egui::IconData> {
    let png_data = include_bytes!("../assets/icon.png");
    let image = image::load_from_memory(png_data).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(eframe::egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("caravan=info")),
        )
        .with_timer(LocalTimer)
        .init();

    // Create tokio runtime BEFORE eframe to avoid deadlock with winit event loop
    let runtime = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    let state = state::AppState::new();

    // Try to load config — may not exist yet if first run
    let config = match config::load_config() {
        Ok(c) => {
            tracing::info!("Loaded config with {} environments", c.environments.len());
            config::install(&c);
            models::environments::install(c.environments.clone());
            Some(c)
        }
        Err(e) => {
            tracing::info!("No valid config found ({}), will run setup wizard", e);
            None
        }
    };

    // Determine if setup is needed: no config file AND never completed setup before.
    // If config exists but fails to parse, don't re-show the wizard — the user already set up.
    let needs_setup = config.is_none() && {
        // Check if the config file exists on disk (even if it failed to parse)
        let config_file_exists = crate::config::config_file_exists();
        if config_file_exists {
            tracing::warn!(
                "Config file exists but failed to parse — skipping setup wizard. \
                 Check Settings to fix configuration."
            );
            false
        } else {
            true
        }
    };

    if needs_setup {
        tracing::info!("Setup needed — launching first-run wizard");
    }

    // Open cache and load persisted data into state for instant UI
    let cache = match cache::Cache::open() {
        Ok(c) => {
            // Load cached data
            match c.load_environments() {
                Ok(envs) => {
                    let count: usize = envs.values().map(|e| e.helm_releases.len()).sum();
                    tracing::info!(
                        "Loaded {} cached environments ({} helm releases)",
                        envs.len(),
                        count
                    );
                    state.write(|s| s.environments = envs);
                }
                Err(e) => tracing::warn!("Failed to load cached environments: {}", e),
            }
            match c.load_github() {
                Ok((runs, prs, releases, gitops_prs)) => {
                    tracing::info!(
                        "Loaded cache: {} workflow runs, {} PRs, {} releases",
                        runs.len(),
                        prs.len(),
                        releases.len()
                    );
                    state.update_github(runs, prs, releases, gitops_prs);
                }
                Err(e) => tracing::warn!("Failed to load cached GitHub data: {}", e),
            }
            match c.load_repo_tags() {
                Ok(tags) => {
                    tracing::info!("Loaded cached tags for {} repos", tags.len());
                    state.update_repo_tags(tags);
                }
                Err(e) => tracing::warn!("Failed to load cached repo tags: {}", e),
            }
            match c.load_gitops_snapshot() {
                Ok((services, versions, pins)) => {
                    let count: usize = versions.values().map(|e| e.len()).sum();
                    tracing::info!(
                        "Loaded cached gitops snapshot: {} services, {} versions, pins for {} envs",
                        services.len(),
                        count,
                        pins.len()
                    );
                    state.write(|s| {
                        s.gitops_services = services;
                        s.declared_versions = versions;
                        s.operator_pins = pins;
                    });
                }
                Err(e) => tracing::warn!("Failed to load cached gitops snapshot: {}", e),
            }
            match c.load_org_repos() {
                Ok(repos) => {
                    tracing::info!("Loaded {} cached org repos", repos.len());
                    state.update_org_repos(repos);
                }
                Err(e) => tracing::warn!("Failed to load cached org repos: {}", e),
            }
            match c.load_hidden_repos() {
                Ok(hidden) => {
                    tracing::info!("Loaded {} hidden repos", hidden.len());
                    state.write(|s| s.hidden_repos = hidden);
                }
                Err(e) => tracing::warn!("Failed to load hidden repos: {}", e),
            }
            match c.load_commit_comparisons() {
                Ok(comparisons) => {
                    tracing::info!("Loaded {} cached commit comparisons", comparisons.len());
                    state.set_commit_comparisons(comparisons);
                }
                Err(e) => tracing::warn!("Failed to load cached commit comparisons: {}", e),
            }
            match c.load_commit_relations() {
                Ok(relations) => state.set_commit_relations(relations),
                Err(e) => tracing::warn!("Failed to load cached commit relations: {}", e),
            }
            match c.load_pr_builds() {
                Ok(builds) => state.set_pr_builds(builds),
                Err(e) => tracing::warn!("Failed to load cached PR builds: {}", e),
            }
            match c.load_issues() {
                Ok(issues) => {
                    tracing::info!("Loaded {} cached issues", issues.len());
                    state.update_issues(issues);
                }
                Err(e) => tracing::warn!("Failed to load cached issues: {}", e),
            }
            state.auto_hide_stale_repos(&c);

            Some(std::sync::Arc::new(c))
        }
        Err(e) => {
            tracing::warn!("Failed to open cache, running without persistence: {}", e);
            None
        }
    };

    // Start pollers immediately only if setup is NOT needed
    let mut instance_guard = None;
    if !needs_setup {
        if let Some(ref cfg) = config {
            instance_guard = pollers::start_background(&runtime, cfg, &state, cache.clone());
        }
    }

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([1600.0, 1000.0])
        .with_min_inner_size([800.0, 600.0])
        .with_decorations(true)
        .with_transparent(false)
        .with_title(branding::window_title())
        .with_app_id(branding::APP_NAME.to_string());

    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        branding::APP_NAME,
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(app::CaravanApp::new_with_runtime(
                cc,
                state,
                cache.clone(),
                config,
                needs_setup,
                runtime,
                instance_guard,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}
