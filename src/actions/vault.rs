//! Vault unseal and pod management actions via kubectl exec.
//!
//! All operations use `tokio::process::Command` with the vault kubeconfig,
//! NOT the HTTP API (which load-balances and won't route to sealed nodes).
//!
//! SECURITY: Unseal keys are NEVER logged or put on the local argv. They go
//! to the pod over kubectl's stdin, live only inside the async task that
//! submits them, and are wiped when it ends.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::actions::unseal_keys::{self, KeySource, UnsealKeys};
use crate::config::{NotificationConfig, VaultConfig};
use crate::state::AppState;

/// Response from `vault status -format=json`
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct VaultStatusResponse {
    #[serde(rename = "type", default)]
    pub seal_type: String,
    pub sealed: bool,
    pub t: u32,        // threshold (keys needed)
    pub n: u32,        // total shares
    pub progress: u32, // keys submitted so far
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub cluster_name: String,
    #[serde(default)]
    pub ha_enabled: Option<bool>,
    #[serde(default)]
    pub active_time: Option<String>,
}

/// Result of an unseal attempt on a single pod
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UnsealResult {
    pub pod: String,
    pub sealed: bool,
    pub progress: u32,
    pub threshold: u32,
    pub error: Option<String>,
}

/// A sealed pod to unseal, with the threshold it reported (if any).
#[derive(Debug, Clone)]
pub struct UnsealTarget {
    pub name: String,
    pub threshold: Option<u32>,
}

/// Progress of one pod during an unseal run, reported after every key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodProgress {
    pub pod: String,
    pub submitted: u32,
    pub threshold: u32,
    /// None until the first key has been answered
    pub sealed: Option<bool>,
    pub error: Option<String>,
}

impl PodProgress {
    pub fn is_unsealed(&self) -> bool {
        self.sealed == Some(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsealTrigger {
    Manual,
    Banner,
    Auto,
}

impl UnsealTrigger {
    pub fn label(&self) -> &'static str {
        match self {
            UnsealTrigger::Manual => "manual",
            UnsealTrigger::Banner => "banner",
            UnsealTrigger::Auto => "auto-unseal",
        }
    }
}

/// One unseal run for an env. Lives in `AppState` so the banner and the
/// Vault page render the same progress, and stays until dismissed.
#[derive(Debug, Clone)]
pub struct VaultUnsealRun {
    pub env: String,
    pub trigger: UnsealTrigger,
    pub source: KeySource,
    pub started: DateTime<Utc>,
    pub finished: Option<DateTime<Utc>>,
    pub pods: Vec<PodProgress>,
    /// Run-level failure (key source failed, no kubeconfig)
    pub error: Option<String>,
    /// Keys were stored in the keyring after this run
    pub keys_remembered: bool,
}

impl VaultUnsealRun {
    pub fn is_running(&self) -> bool {
        self.finished.is_none()
    }

    pub fn all_unsealed(&self) -> bool {
        !self.pods.is_empty() && self.pods.iter().all(|p| p.is_unsealed())
    }

    pub fn succeeded(&self) -> bool {
        self.finished.is_some() && self.error.is_none() && self.all_unsealed()
    }

    pub fn unsealed_count(&self) -> usize {
        self.pods.iter().filter(|p| p.is_unsealed()).count()
    }

    /// One-line outcome for the banner, toasts and notifications.
    pub fn summary(&self) -> String {
        if self.is_running() {
            return format!(
                "Unsealing {}: {} of {} pods done",
                self.env,
                self.unsealed_count(),
                self.pods.len()
            );
        }
        if let Some(ref e) = self.error {
            return format!("{} unseal failed: {}", self.env, e);
        }
        let failed: Vec<&str> = self
            .pods
            .iter()
            .filter(|p| p.error.is_some())
            .map(|p| p.pod.as_str())
            .collect();
        if self.all_unsealed() {
            let mut s = format!("{} vault unsealed ({} pods)", self.env, self.pods.len());
            if self.keys_remembered {
                s.push_str(", keys remembered in the keyring");
            }
            s
        } else if !failed.is_empty() {
            format!(
                "{} unseal: {} of {} pods unsealed, failed: {}",
                self.env,
                self.unsealed_count(),
                self.pods.len(),
                failed.join(", ")
            )
        } else {
            format!(
                "{} unseal: {} of {} pods unsealed, more keys needed",
                self.env,
                self.unsealed_count(),
                self.pods.len()
            )
        }
    }
}

/// Threshold in order of trust: what the pod reported, then the HTTP
/// seal-status `t`, then vault's default of 3.
pub fn select_threshold(pod_threshold: Option<u32>, http_threshold: Option<u32>) -> u32 {
    pod_threshold
        .filter(|t| *t > 0)
        .or(http_threshold.filter(|t| *t > 0))
        .unwrap_or(3)
}

/// Build a kubectl Command with the vault kubeconfig set
fn kubectl_cmd(kubeconfig: &Path) -> Command {
    let mut cmd = Command::new("kubectl");
    cmd.env("KUBECONFIG", kubeconfig);
    cmd
}

/// Get detailed vault status from a specific pod via `kubectl exec`.
pub async fn get_pod_status(
    kubeconfig: &Path,
    namespace: &str,
    pod_name: &str,
) -> Result<VaultStatusResponse> {
    let output = kubectl_cmd(kubeconfig)
        .args([
            "exec",
            pod_name,
            "-n",
            namespace,
            "--",
            "vault",
            "status",
            "-format=json",
        ])
        .output()
        .await
        .context("Failed to execute kubectl")?;

    // vault status returns exit code 2 when sealed, but still outputs valid JSON
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("No output from vault status: {}", stderr.trim());
    }

    serde_json::from_str(stdout.trim())
        .with_context(|| format!("Failed to parse vault status JSON from {}", pod_name))
}

/// The kubectl argv for one unseal. The key is not part of it: the shell
/// inside the pod reads it from stdin.
pub fn unseal_exec_args(namespace: &str, pod_name: &str) -> Vec<String> {
    [
        "exec",
        "-i",
        pod_name,
        "-n",
        namespace,
        "--",
        "sh",
        "-c",
        "IFS= read -r k; vault operator unseal -format=json \"$k\"",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn failed(pod_name: &str, error: String) -> UnsealResult {
    UnsealResult {
        pod: pod_name.to_string(),
        sealed: true,
        progress: 0,
        threshold: 0,
        error: Some(error),
    }
}

/// Submit one unseal key to a pod. The key travels over kubectl's stdin so
/// it never appears on the local process list; stdout/stderr are parsed for
/// status only.
pub async fn unseal_pod(
    kubeconfig: &Path,
    namespace: &str,
    pod_name: &str,
    unseal_key: &str,
) -> UnsealResult {
    let mut child = match kubectl_cmd(kubeconfig)
        .args(unseal_exec_args(namespace, pod_name))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return failed(pod_name, format!("kubectl exec failed: {}", e)),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let line = Zeroizing::new(format!("{}\n", unseal_key));
        if let Err(e) = stdin.write_all(line.as_bytes()).await {
            return failed(pod_name, format!("Could not send key to kubectl: {}", e));
        }
        // stdin drops here, closing the pipe
    }

    let output = match child.wait_with_output().await {
        Ok(o) => o,
        Err(e) => return failed(pod_name, format!("kubectl exec failed: {}", e)),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let safe_err = if stderr.contains(unseal_key) {
            "Unseal command failed (details redacted for security)".to_string()
        } else {
            format!("Unseal failed: {}", stderr.trim())
        };
        return failed(pod_name, safe_err);
    }

    // `vault operator unseal -format=json` prints the seal status after the key
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(status) = serde_json::from_str::<VaultStatusResponse>(stdout.trim()) {
        return UnsealResult {
            pod: pod_name.to_string(),
            sealed: status.sealed,
            progress: status.progress,
            threshold: status.t,
            error: None,
        };
    }

    match get_pod_status(kubeconfig, namespace, pod_name).await {
        Ok(status) => UnsealResult {
            pod: pod_name.to_string(),
            sealed: status.sealed,
            progress: status.progress,
            threshold: status.t,
            error: None,
        },
        Err(e) => failed(
            pod_name,
            format!("Unseal submitted but status check failed: {}", e),
        ),
    }
}

/// Unseal every target in parallel, one key at a time per pod, stopping when
/// the pod reports unsealed, `threshold` keys are used, or a key fails.
/// `threshold` is the fallback for targets that did not report their own.
pub async fn unseal_env(
    kubeconfig: PathBuf,
    namespace: String,
    targets: Vec<UnsealTarget>,
    keys: UnsealKeys,
    threshold: u32,
    progress: mpsc::Sender<PodProgress>,
) {
    let keys = std::sync::Arc::new(keys);
    let runs = targets.into_iter().map(|target| {
        let kubeconfig = kubeconfig.clone();
        let namespace = namespace.clone();
        let keys = keys.clone();
        let progress = progress.clone();
        let pod_threshold = select_threshold(target.threshold, Some(threshold));
        async move {
            for (i, key) in keys.iter().take(pod_threshold as usize).enumerate() {
                let result = unseal_pod(&kubeconfig, &namespace, &target.name, key).await;
                let report = PodProgress {
                    pod: target.name.clone(),
                    submitted: i as u32 + 1,
                    threshold: if result.threshold > 0 {
                        result.threshold
                    } else {
                        pod_threshold
                    },
                    sealed: if result.error.is_some() {
                        None
                    } else {
                        Some(result.sealed)
                    },
                    error: result.error.clone(),
                };
                let stop = result.error.is_some() || !result.sealed;
                let _ = progress.send(report).await;
                if stop {
                    break;
                }
            }
        }
    });
    futures::future::join_all(runs).await;
}

/// Keys handed to `spawn_unseal`: already collected (manual entry) or to be
/// resolved from the configured source when the run starts.
pub enum KeyInput {
    Keys(UnsealKeys, KeySource),
    Resolve {
        cfg: VaultConfig,
        remember_override: Option<bool>,
    },
}

/// Start an unseal run for `env` in the background and track it in
/// `AppState`. Returns false when a run for that env is already going.
#[allow(clippy::too_many_arguments)]
pub fn spawn_unseal(
    handle: &tokio::runtime::Handle,
    state: AppState,
    env: String,
    kubeconfig: PathBuf,
    targets: Vec<UnsealTarget>,
    fallback_threshold: u32,
    keys: KeyInput,
    trigger: UnsealTrigger,
    remember_on_success: bool,
    notifications: Option<NotificationConfig>,
) -> bool {
    let source = match &keys {
        KeyInput::Keys(_, s) => *s,
        KeyInput::Resolve { .. } => KeySource::Manual,
    };
    let run = VaultUnsealRun {
        env: env.clone(),
        trigger,
        source,
        started: Utc::now(),
        finished: None,
        pods: targets
            .iter()
            .map(|t| PodProgress {
                pod: t.name.clone(),
                submitted: 0,
                threshold: select_threshold(t.threshold, Some(fallback_threshold)),
                sealed: None,
                error: None,
            })
            .collect(),
        error: None,
        keys_remembered: false,
    };
    if !state.begin_vault_run(run) {
        return false;
    }
    let namespace = crate::config::vault_for(&env).namespace;

    handle.spawn(async move {
        let resolved = match keys {
            KeyInput::Keys(k, s) => Ok((k, s)),
            KeyInput::Resolve {
                cfg,
                remember_override,
            } => {
                let env_for_resolve = env.clone();
                match tokio::task::spawn_blocking(move || {
                    unseal_keys::resolve_keys(&env_for_resolve, &cfg, remember_override)
                })
                .await
                {
                    Ok(Ok(Some(pair))) => Ok(pair),
                    Ok(Ok(None)) => Err("no key source configured".to_string()),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(e) => Err(format!("key resolution task failed: {}", e)),
                }
            }
        };

        let (keys, source) = match resolved {
            Ok(pair) => pair,
            Err(e) => {
                state.finish_vault_run(&env, Some(e), false);
                let summary = state
                    .vault_run(&env)
                    .map(|r| r.summary())
                    .unwrap_or_default();
                if let Some(ref n) = notifications {
                    if trigger == UnsealTrigger::Auto && crate::notify::env_enabled(n, &env) {
                        crate::notify::notify(n, "Vault auto-unseal failed", &summary);
                    }
                }
                state.push_vault_event(summary);
                return;
            }
        };
        state.set_vault_run_source(&env, source);

        let (tx, mut rx) = mpsc::channel::<PodProgress>(32);
        let forward_state = state.clone();
        let forward_env = env.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                forward_state.update_vault_run_pod(&forward_env, p);
            }
        });

        let key_copy = if trigger == UnsealTrigger::Manual && remember_on_success {
            Some(Zeroizing::new(keys.to_vec()))
        } else {
            None
        };
        unseal_env(kubeconfig, namespace, targets, keys, fallback_threshold, tx).await;
        let _ = forwarder.await;

        let all_unsealed = state
            .vault_run(&env)
            .is_some_and(|r| r.all_unsealed());
        let mut remembered = false;
        if let (Some(keys), true) = (key_copy, all_unsealed) {
            let env_for_store = env.clone();
            let stored = tokio::task::spawn_blocking(move || {
                unseal_keys::remember_keys_in_keyring(&env_for_store, &keys)
            })
            .await;
            match stored {
                Ok(Ok(())) => remembered = true,
                Ok(Err(e)) => tracing::warn!(env = %env, err = %e, "Could not store unseal keys in keyring"),
                Err(e) => tracing::warn!(env = %env, err = %e, "Keyring task failed"),
            }
        }
        state.finish_vault_run(&env, None, remembered);

        let summary = state
            .vault_run(&env)
            .map(|r| r.summary())
            .unwrap_or_default();
        tracing::info!(env = %env, trigger = trigger.label(), source = source.label(), "{}", summary);
        if let Some(ref n) = notifications {
            if trigger == UnsealTrigger::Auto && crate::notify::env_enabled(n, &env) {
                let title = if all_unsealed {
                    "Vault auto-unsealed"
                } else {
                    "Vault auto-unseal incomplete"
                };
                crate::notify::notify(n, title, &summary);
            }
        }
        state.push_vault_event(summary);
    });
    true
}

#[allow(dead_code)]
/// Delete a vault pod (it will restart sealed).
/// This is an advanced operation: the pod will come back in a sealed state.
pub async fn delete_pod(kubeconfig: &Path, namespace: &str, pod_name: &str) -> Result<()> {
    let output = kubectl_cmd(kubeconfig)
        .args(["delete", "pod", pod_name, "-n", namespace])
        .output()
        .await
        .context("Failed to execute kubectl delete")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to delete pod {}: {}", pod_name, stderr.trim());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_prefers_pod_then_http_then_default() {
        assert_eq!(select_threshold(Some(2), Some(5)), 2);
        assert_eq!(select_threshold(None, Some(5)), 5);
        assert_eq!(select_threshold(Some(0), Some(5)), 5);
        assert_eq!(select_threshold(None, None), 3);
        assert_eq!(select_threshold(None, Some(0)), 3);
    }

    #[test]
    fn unseal_argv_never_carries_the_key() {
        let key = "super-secret-share-1";
        let args = unseal_exec_args("vault", "vault-0");
        assert!(args.iter().all(|a| !a.contains(key)));
        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"vault-0".to_string()));
        assert!(args.last().unwrap().contains("read -r k"));
        assert!(args.last().unwrap().contains("vault operator unseal"));
    }

    #[test]
    fn run_summary_reflects_outcome() {
        let mut run = VaultUnsealRun {
            env: "staging".to_string(),
            trigger: UnsealTrigger::Banner,
            source: KeySource::Command,
            started: Utc::now(),
            finished: None,
            pods: vec![
                PodProgress {
                    pod: "vault-0".to_string(),
                    submitted: 3,
                    threshold: 3,
                    sealed: Some(false),
                    error: None,
                },
                PodProgress {
                    pod: "vault-1".to_string(),
                    submitted: 1,
                    threshold: 3,
                    sealed: None,
                    error: Some("boom".to_string()),
                },
            ],
            error: None,
            keys_remembered: false,
        };
        assert!(run.is_running());
        assert!(run.summary().starts_with("Unsealing staging"));
        run.finished = Some(Utc::now());
        assert!(!run.succeeded());
        assert!(run.summary().contains("failed: vault-1"));
        run.pods[1].sealed = Some(false);
        run.pods[1].error = None;
        run.keys_remembered = true;
        assert!(run.succeeded());
        assert!(run.summary().contains("keys remembered"));
    }
}
