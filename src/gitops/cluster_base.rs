//! Cluster-base layout: use cases whose HelmReleases are pinned in base
//! multi-document files, applied per cluster through an overlay directory
//! named after the environment. Read only: pins live in base, so a promotion
//! between environments has no file to write.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;

use super::{parse_helm_release, ChangeSet, DeclaredSnapshot, GitopsLayout, GitopsTree};
use crate::state::DeclaredVersion;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ClusterBaseConfig {
    /// Directory whose subdirectories are the use cases
    #[serde(default = "default_use_cases_dir")]
    pub use_cases_dir: String,
    /// Base directory inside a use case
    #[serde(default = "default_base_dir")]
    pub base_dir: String,
    /// Overlay directory inside a use case, `{env}` substituted; an env
    /// without one does not run the use case
    #[serde(default = "default_overlay_dir")]
    pub overlay_dir: String,
}

impl Default for ClusterBaseConfig {
    fn default() -> Self {
        Self {
            use_cases_dir: default_use_cases_dir(),
            base_dir: default_base_dir(),
            overlay_dir: default_overlay_dir(),
        }
    }
}

fn default_use_cases_dir() -> String {
    "use-cases".into()
}
fn default_base_dir() -> String {
    "base".into()
}
fn default_overlay_dir() -> String {
    "overlays/{env}".into()
}

pub struct ClusterBaseLayout {
    cfg: ClusterBaseConfig,
}

impl ClusterBaseLayout {
    pub fn new(cfg: ClusterBaseConfig) -> Self {
        Self { cfg }
    }

    fn overlay_dir(&self, use_case: &str, env: &str) -> String {
        format!(
            "{}/{use_case}/{}",
            self.cfg.use_cases_dir,
            self.cfg.overlay_dir.replace("{env}", env)
        )
    }

    /// (HelmRelease name, declared version) for every HelmRelease document
    /// in the use case's base files.
    fn base_releases(
        &self,
        tree: &dyn GitopsTree,
        use_case: &str,
    ) -> Result<Vec<(String, DeclaredVersion)>> {
        let base = format!(
            "{}/{use_case}/{}",
            self.cfg.use_cases_dir, self.cfg.base_dir
        );
        let mut out = Vec::new();
        for file in tree.list_files(&base)? {
            if !(file.ends_with(".yml") || file.ends_with(".yaml")) {
                continue;
            }
            let Some(content) = tree.read(&format!("{base}/{file}"))? else {
                continue;
            };
            for doc in serde_yaml::Deserializer::from_str(&content) {
                let Ok(value) = serde_yaml::Value::deserialize(doc) else {
                    continue;
                };
                if value.get("kind").and_then(|k| k.as_str()) != Some("HelmRelease") {
                    continue;
                }
                let Some(name) = value
                    .get("metadata")
                    .and_then(|m| m.get("name"))
                    .and_then(|n| n.as_str())
                else {
                    continue;
                };
                out.push((name.to_string(), parse_helm_release(&value)));
            }
        }
        Ok(out)
    }
}

impl GitopsLayout for ClusterBaseLayout {
    fn name(&self) -> &'static str {
        "cluster-base"
    }

    fn read_declared(&self, tree: &dyn GitopsTree, envs: &[String]) -> Result<DeclaredSnapshot> {
        let mut snapshot = DeclaredSnapshot::default();
        for use_case in tree.list_dirs(&self.cfg.use_cases_dir)? {
            let releases = self.base_releases(tree, &use_case)?;
            if releases.is_empty() {
                continue;
            }
            let overlays_root = self.overlay_dir(&use_case, "");
            let overlays_root = overlays_root.trim_end_matches('/');
            let overlays = tree.list_dirs(overlays_root).unwrap_or_default();
            for env in envs {
                // Without any overlay directory every env runs the base.
                if !overlays.is_empty() && !overlays.contains(env) {
                    continue;
                }
                for (name, version) in &releases {
                    if !snapshot.services.contains(name) {
                        snapshot.services.push(name.clone());
                    }
                    snapshot
                        .declared_versions
                        .entry(name.clone())
                        .or_default()
                        .insert(env.clone(), version.clone());
                }
            }
        }
        snapshot.services.sort();
        Ok(snapshot)
    }

    fn supports_promotion(&self) -> bool {
        false
    }

    fn promotable_services(&self, _tree: &dyn GitopsTree, _env: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn promotion(
        &self,
        _tree: &dyn GitopsTree,
        _base_sha: &str,
        from: &str,
        to: &str,
        _services: &[String],
        _ahead: &HashMap<String, i64>,
    ) -> Result<ChangeSet> {
        anyhow::bail!(
            "the cluster-base layout pins versions in base; there is no {from} to {to} promotion to write"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitops::MapTree;

    const NOTES_BASE: &str = r#"
apiVersion: v1
kind: Namespace
metadata:
  name: notes
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: notes-api
  namespace: notes
spec:
  chart:
    spec:
      chart: notes-api
      version: "1.4.0"
  values:
    image:
      repository: ghcr.io/notes-co/notes-api
      tag: v1.4.0
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: notes-worker
spec:
  chart:
    spec:
      version: "0.9.2"
  values:
    worker:
      image:
        tag: v0.9.2
        digest: sha256:0000000000000000000000000000000000000000000000000000000000000001
"#;

    fn tree() -> MapTree {
        MapTree(
            [
                ("use-cases/notes/base/release.yaml", NOTES_BASE),
                ("use-cases/notes/overlays/dev/kustomization.yaml", "resources: [../../base]\n"),
                ("use-cases/notes/overlays/prod/kustomization.yaml", "resources: [../../base]\n"),
                ("use-cases/blank/base/README.md", "nothing here"),
                ("use-cases/shared/base/release.yml", "kind: HelmRelease\nmetadata:\n  name: shared-cache\nspec:\n  chart:\n    spec:\n      version: 2.0.0\n"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        )
    }

    #[test]
    fn base_pins_apply_to_envs_with_an_overlay() {
        let layout = ClusterBaseLayout::new(ClusterBaseConfig::default());
        let envs: Vec<String> = ["prod", "staging", "dev"].map(String::from).to_vec();
        let snap = layout.read_declared(&tree(), &envs).unwrap();
        assert_eq!(snap.services, ["notes-api", "notes-worker", "shared-cache"]);
        let api = &snap.declared_versions["notes-api"];
        assert_eq!(api["dev"].chart_version.as_deref(), Some("1.4.0"));
        assert_eq!(api["dev"].image_tag.as_deref(), Some("v1.4.0"));
        assert!(api.contains_key("prod"));
        assert!(!api.contains_key("staging"), "no staging overlay");
        let worker = &snap.declared_versions["notes-worker"]["prod"];
        assert_eq!(worker.images[0].path, "values.worker.image");
        assert!(worker.images[0].digest.is_some());
        // no overlays directory at all: every env runs it
        let shared = &snap.declared_versions["shared-cache"];
        assert_eq!(shared.len(), 3);
        assert!(snap.operator_pins.is_empty());
    }

    #[test]
    fn promotion_is_refused() {
        let layout = ClusterBaseLayout::new(ClusterBaseConfig::default());
        assert!(!layout.supports_promotion());
        let err = layout
            .promotion(&tree(), "abc", "staging", "dev", &[], &HashMap::new())
            .unwrap_err();
        assert!(err.to_string().contains("no staging to dev promotion"));
    }
}
