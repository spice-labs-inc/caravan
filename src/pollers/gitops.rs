//! Declared versions from the gitops repository, read from the local mirror
//! through the configured layout adapter. No GitHub API involved; passive
//! instances read without fetching.

use anyhow::Result;
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::gitops::{layout_from_config, DeclaredSnapshot, GitopsLayout, MirrorTree};
use crate::gitops_mirror::Mirror;
use crate::state::{AppState, GitopsHead};

/// Entry point: start the GitOps version-tracking poller.
pub async fn start_polling(
    config: Config,
    state: AppState,
    cache: Option<Arc<crate::cache::Cache>>,
) {
    let remote = config.github.app_gitops_remote.clone();
    let interval = Duration::from_secs(config.polling.gitops_interval_secs.max(15));
    let data_dir = crate::config::data_dir();
    let layout: Arc<dyn GitopsLayout> = Arc::from(layout_from_config(&config.gitops));
    let branch = config.gitops.branch.clone();
    let envs = config.environments.order();

    info!(
        remote = %remote,
        layout = layout.name(),
        interval_secs = interval.as_secs(),
        "Starting GitOps mirror poller"
    );

    loop {
        let active = state.run_mode().is_active();
        let cycle = {
            let data_dir = data_dir.clone();
            let remote = remote.clone();
            let layout = layout.clone();
            let branch = branch.clone();
            let envs = envs.clone();
            tokio::task::spawn_blocking(move || {
                poll_once(&data_dir, &remote, active, &*layout, &branch, &envs)
            })
        };
        let result = match tokio::time::timeout(Duration::from_secs(300), cycle).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => Err(anyhow::anyhow!("poll task panicked: {}", e)),
            Err(_) => Err(anyhow::anyhow!("Poll timed out")),
        };

        match result {
            Ok(Some((snapshot, head))) => {
                info!(
                    services = snapshot.services.len(),
                    envs_with_pins = snapshot.operator_pins.len(),
                    head = %head.sha,
                    "GitOps poll complete"
                );
                if let Some(ref cache) = cache {
                    if let Err(e) = cache.save_gitops_snapshot(
                        &snapshot.services,
                        &snapshot.declared_versions,
                        &snapshot.operator_pins,
                    ) {
                        warn!(err = %e, "Failed to cache gitops snapshot");
                    }
                }
                state.update_gitops(
                    snapshot.services,
                    snapshot.declared_versions,
                    snapshot.operator_pins,
                    Some(head),
                );
            }
            Ok(None) => {
                debug!("No gitops mirror yet and fetching is disabled");
            }
            Err(e) => {
                warn!(err = %e, "GitOps poll cycle failed");
                state.add_poller_error("gitops", &format!("Poll failed: {}", e));
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {},
            _ = state.gitops_refresh.notified() => {
                info!("Manual refresh requested, polling gitops now");
            },
        }
    }
}

/// One blocking poll: fetch (when allowed) and parse the branch. None when
/// there is no mirror and this instance may not create one.
fn poll_once(
    data_dir: &std::path::Path,
    remote: &str,
    fetch: bool,
    layout: &dyn GitopsLayout,
    branch: &str,
    envs: &[String],
) -> Result<Option<(DeclaredSnapshot, GitopsHead)>> {
    let mirror = if fetch {
        let m = Mirror::open_or_clone(data_dir, remote)?;
        m.fetch()?;
        m
    } else {
        match Mirror::open_existing(data_dir, remote) {
            Some(m) => m,
            None => return Ok(None),
        }
    };
    read_snapshot(&mirror, layout, branch, envs).map(Some)
}

pub fn read_snapshot(
    mirror: &Mirror,
    layout: &dyn GitopsLayout,
    branch: &str,
    envs: &[String],
) -> Result<(DeclaredSnapshot, GitopsHead)> {
    let sha = mirror.head_sha(branch)?;
    let tree = MirrorTree {
        mirror,
        rev: branch,
    };
    let snapshot = layout.read_declared(&tree, envs)?;
    Ok((
        snapshot,
        GitopsHead {
            sha,
            fetched_at: Utc::now(),
        },
    ))
}
