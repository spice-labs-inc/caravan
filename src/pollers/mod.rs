pub mod gh_http;
pub mod github;
pub mod gitops;
pub mod k8s;
pub mod logs;
pub mod metrics;
pub mod self_update;
pub mod update;
pub mod vault;

use crate::cache::Cache;
use crate::config::Config;
use crate::instance::{self, InstanceGuard, RunMode};
use crate::state::AppState;
use std::sync::Arc;

/// Decide the run mode, seed config-derived state and spawn every poller.
/// Passive instances skip the GitHub poller (Projects V2 included) and read
/// the gitops mirror without fetching; k8s, vault and update pollers always
/// run. Returns the lock guard an active instance must hold until exit.
pub fn start_background(
    runtime: &tokio::runtime::Runtime,
    config: &Config,
    state: &AppState,
    cache: Option<Arc<Cache>>,
) -> Option<InstanceGuard> {
    let (run_mode, mut guard) = instance::detect(&crate::config::data_dir());
    match &run_mode {
        RunMode::Active => tracing::info!("Running active: this instance polls GitHub"),
        RunMode::Passive { reason } => {
            tracing::info!(reason = %reason, "Running passive: GitHub polling off")
        }
    }
    state.set_run_mode(run_mode.clone());
    if let Some(ref mut guard) = guard {
        guard.spawn_heartbeat(runtime);
    }

    state.seed_hidden_repos(&config.repos.hide);

    let (poll_config, poll_state, poll_cache) = (config.clone(), state.clone(), cache.clone());
    runtime.spawn(async move {
        k8s::start_polling(poll_config, poll_state, poll_cache).await;
    });

    if run_mode.is_active() {
        let (poll_config, poll_state, poll_cache) = (config.clone(), state.clone(), cache.clone());
        runtime.spawn(async move {
            github::start_polling(poll_config, poll_state, poll_cache).await;
        });
    }

    let (poll_config, poll_state, poll_cache) = (config.clone(), state.clone(), cache.clone());
    runtime.spawn(async move {
        gitops::start_polling(poll_config, poll_state, poll_cache).await;
    });

    let (poll_config, poll_state, poll_cache) = (config.clone(), state.clone(), cache.clone());
    runtime.spawn(async move {
        vault::start_polling(poll_config, poll_state, poll_cache).await;
    });

    let (poll_config, poll_state, poll_cache) = (config.clone(), state.clone(), cache);
    runtime.spawn(async move {
        update::start_polling(poll_config, poll_state, poll_cache).await;
    });

    guard
}
