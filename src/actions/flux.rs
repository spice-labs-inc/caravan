//! Flux "sync with source" — the same thing as Headlamp's button on the apps
//! Kustomization: annotate the GitRepository with reconcile.fluxcd.io/requestedAt
//! so it pulls the gitops repo, wait for it to be handled, then do the same for the
//! Kustomization and wait for the new revision to be applied.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use kube::api::{Api, DynamicObject, Patch, PatchParams};
use kube::core::GroupVersionKind;
use kube::Client;

use crate::state::{AppState, FluxSyncState};

/// How long to wait for each object to handle the reconcile request.
const WAIT_TIMEOUT_SECS: u64 = 180;

/// Kick off a sync-with-source for one environment on the runtime, reporting
/// progress into AppState (read by the UI).
pub fn spawn_sync(
    runtime: &tokio::runtime::Runtime,
    env_name: String,
    env_config: crate::config::EnvironmentConfig,
    state: AppState,
) {
    if state.flux_sync_running(&env_name) {
        return;
    }
    state.set_flux_sync(
        &env_name,
        FluxSyncState::Running {
            phase: "connecting".to_string(),
            started: Utc::now(),
        },
    );
    runtime.spawn(async move {
        let result = run_sync(&env_name, &env_config, &state).await;
        match result {
            Ok(revision) => {
                tracing::info!(env = %env_name, revision = %revision, "Flux sync complete");
                state.set_flux_sync(
                    &env_name,
                    FluxSyncState::Done {
                        revision,
                        finished: Utc::now(),
                    },
                );
            }
            Err(e) => {
                tracing::warn!(env = %env_name, err = %e, "Flux sync failed");
                state.set_flux_sync(
                    &env_name,
                    FluxSyncState::Failed {
                        error: format!("{}", e),
                        finished: Utc::now(),
                    },
                );
            }
        }
    });
}

async fn run_sync(
    env_name: &str,
    env_config: &crate::config::EnvironmentConfig,
    state: &AppState,
) -> Result<String> {
    let client = crate::pollers::k8s::create_client(env_config).await?;
    let requested_at = Utc::now().to_rfc3339();

    let phase = |p: &str| {
        state.set_flux_sync(
            env_name,
            FluxSyncState::Running {
                phase: p.to_string(),
                started: Utc::now(),
            },
        );
    };

    // 1. Pull the source
    let flux = crate::config::flux();
    phase("pulling gitops source");
    let git_gvk = GroupVersionKind::gvk("source.toolkit.fluxcd.io", "v1", "GitRepository");
    let git_api = dynamic_api(&client, &git_gvk, &flux.namespace).await?;
    annotate_reconcile(&git_api, &flux.git_repository, &requested_at).await?;
    wait_handled(&git_api, &flux.git_repository, &requested_at).await?;

    // 2. Apply the kustomization
    phase("applying kustomization");
    let ks_gvk = GroupVersionKind::gvk("kustomize.toolkit.fluxcd.io", "v1", "Kustomization");
    let ks_api = dynamic_api(&client, &ks_gvk, &flux.namespace).await?;
    annotate_reconcile(&ks_api, &flux.kustomization, &requested_at).await?;
    let obj = wait_handled(&ks_api, &flux.kustomization, &requested_at).await?;

    let revision = obj
        .data
        .get("status")
        .and_then(|s| s.get("lastAppliedRevision"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown revision")
        .to_string();
    Ok(revision)
}

async fn dynamic_api(
    client: &Client,
    gvk: &GroupVersionKind,
    namespace: &str,
) -> Result<Api<DynamicObject>> {
    let (ar, _caps) = kube::discovery::pinned_kind(client, gvk)
        .await
        .with_context(|| format!("Failed to discover {}", gvk.kind))?;
    Ok(Api::namespaced_with(client.clone(), namespace, &ar))
}

async fn annotate_reconcile(
    api: &Api<DynamicObject>,
    name: &str,
    requested_at: &str,
) -> Result<()> {
    let patch = serde_json::json!({
        "metadata": {
            "annotations": { "reconcile.fluxcd.io/requestedAt": requested_at }
        }
    });
    api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .with_context(|| format!("Failed to annotate {} for reconcile", name))?;
    Ok(())
}

/// Poll until the object reports our reconcile request handled and Ready=True.
async fn wait_handled(
    api: &Api<DynamicObject>,
    name: &str,
    requested_at: &str,
) -> Result<DynamicObject> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(WAIT_TIMEOUT_SECS);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let obj = api
            .get(name)
            .await
            .with_context(|| format!("Failed to get {}", name))?;

        let status = obj.data.get("status");
        let handled = status
            .and_then(|s| s.get("lastHandledReconcileAt"))
            .and_then(|v| v.as_str())
            == Some(requested_at);
        if handled {
            let ready = status
                .and_then(|s| s.get("conditions"))
                .and_then(|c| c.as_array())
                .and_then(|conds| {
                    conds
                        .iter()
                        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("Ready"))
                });
            match ready.and_then(|c| c.get("status")).and_then(|v| v.as_str()) {
                Some("True") => return Ok(obj),
                Some("False") => {
                    let msg = ready
                        .and_then(|c| c.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("not ready");
                    bail!("{} reconciled but not ready: {}", name, msg);
                }
                _ => {} // still progressing
            }
        }
        if tokio::time::Instant::now() > deadline {
            bail!("Timed out waiting for {} to reconcile", name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live test against a real cluster; set CARAVAN_TEST_KUBECONFIG and
    /// optionally CARAVAN_TEST_ENV, then run manually:
    /// cargo test sync_env_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn sync_env_live() {
        let Ok(kubeconfig) = std::env::var("CARAVAN_TEST_KUBECONFIG") else {
            eprintln!("CARAVAN_TEST_KUBECONFIG not set, skipping");
            return;
        };
        let env_name = std::env::var("CARAVAN_TEST_ENV").unwrap_or_else(|_| "dev".to_string());
        let env_config: crate::config::EnvironmentConfig =
            toml::from_str(&format!("id = {env_name:?}\nkubeconfig = {kubeconfig:?}\n"))
                .expect("environment config");
        let state = AppState::new();
        let revision = run_sync(&env_name, &env_config, &state)
            .await
            .expect("sync failed");
        println!("synced {env_name} to {revision}");
        assert!(revision.contains("@"));
    }
}
