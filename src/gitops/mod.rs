//! The gitops repository layout adapter: how declared versions are read for
//! an environment and how a promotion between two environments is written.
//! Two layouts ship: `overlay` (per-environment overlay patches, promotable)
//! and `cluster-base` (base-pinned HelmReleases with per-cluster overlays,
//! read only).

pub mod cluster_base;
pub mod overlay;

use anyhow::Result;
use std::collections::HashMap;

use crate::config::GitopsConfig;
use crate::gitops_mirror::Mirror;
use crate::state::{DeclaredVersion, OperatorPins};

/// Read access to one revision of the gitops repository.
pub trait GitopsTree {
    fn read(&self, path: &str) -> Result<Option<String>>;
    /// Directory names directly under `path`.
    fn list_dirs(&self, path: &str) -> Result<Vec<String>>;
    /// File names directly under `path`.
    fn list_files(&self, path: &str) -> Result<Vec<String>>;
}

/// A branch of the local mirror.
pub struct MirrorTree<'a> {
    pub mirror: &'a Mirror,
    pub rev: &'a str,
}

impl GitopsTree for MirrorTree<'_> {
    fn read(&self, path: &str) -> Result<Option<String>> {
        self.mirror.read_file(self.rev, path)
    }
    fn list_dirs(&self, path: &str) -> Result<Vec<String>> {
        self.mirror.list_dirs(self.rev, path)
    }
    fn list_files(&self, path: &str) -> Result<Vec<String>> {
        self.mirror.list_files(self.rev, path)
    }
}

/// An in-memory tree keyed by repo-relative path; listing is derived from
/// the keys.
#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
pub struct MapTree(pub HashMap<String, String>);

#[allow(dead_code)]
impl MapTree {
    fn children(&self, path: &str, dirs: bool) -> Vec<String> {
        let prefix = if path.is_empty() || path == "." {
            String::new()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        let mut out: Vec<String> = self
            .0
            .keys()
            .filter_map(|k| {
                let rest = k.strip_prefix(&prefix)?;
                let (first, is_dir) = match rest.split_once('/') {
                    Some((first, _)) => (first, true),
                    None => (rest, false),
                };
                (is_dir == dirs && !first.is_empty()).then(|| first.to_string())
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

impl GitopsTree for MapTree {
    fn read(&self, path: &str) -> Result<Option<String>> {
        Ok(self.0.get(path).cloned())
    }
    fn list_dirs(&self, path: &str) -> Result<Vec<String>> {
        Ok(self.children(path, true))
    }
    fn list_files(&self, path: &str) -> Result<Vec<String>> {
        Ok(self.children(path, false))
    }
}

/// Everything a layout reads from one revision.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeclaredSnapshot {
    pub services: Vec<String>,
    /// service -> env -> declared version
    pub declared_versions: HashMap<String, HashMap<String, DeclaredVersion>>,
    /// env -> an operator's component pins (empty when the layout has none)
    pub operator_pins: HashMap<String, OperatorPins>,
}

/// One value that differs between the source and the target environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueEdit {
    /// Dotted yaml path (`values.web.image.tag`) or operator data key
    pub path: String,
    pub old: String,
    pub new: String,
    /// Both values carry the same image digest: only the tag name changes.
    pub same_image: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceChange {
    pub name: String,
    /// Repo-relative path of the target file that changes
    pub file: String,
    pub edits: Vec<ValueEdit>,
    pub warning: Option<String>,
    /// Component names an operator change touches (empty for a service)
    pub components: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// Revision the change set was computed from
    pub base_sha: String,
    pub from: String,
    pub to: String,
    /// The operator entry in `services`, when the layout tracks one
    pub operator: Option<String>,
    pub services: Vec<ServiceChange>,
    /// Services whose target pins already match the source
    pub unchanged: Vec<String>,
    /// Operator data keys only the target has (left untouched)
    pub operator_target_only: Vec<String>,
}

/// How a gitops repository declares versions and takes promotions.
pub trait GitopsLayout: Send + Sync {
    fn name(&self) -> &'static str;

    /// Declared versions and operator pins for `envs`.
    fn read_declared(&self, tree: &dyn GitopsTree, envs: &[String]) -> Result<DeclaredSnapshot>;

    /// Whether `promotion` can produce a change set.
    fn supports_promotion(&self) -> bool;

    /// The names promotions can select from (service directories of `env`).
    fn promotable_services(&self, tree: &dyn GitopsTree, env: &str) -> Result<Vec<String>>;

    /// Diff `from` against `to` for `services`. `ahead` maps a service or
    /// component name to how many commits its `to` build is ahead of `from`;
    /// it only feeds warnings.
    fn promotion(
        &self,
        tree: &dyn GitopsTree,
        base_sha: &str,
        from: &str,
        to: &str,
        services: &[String],
        ahead: &HashMap<String, i64>,
    ) -> Result<ChangeSet>;
}

/// The layout the config selects.
pub fn layout_from_config(cfg: &GitopsConfig) -> Box<dyn GitopsLayout> {
    match cfg.layout.as_str() {
        "cluster-base" => Box::new(cluster_base::ClusterBaseLayout::new(
            cfg.cluster_base.clone(),
        )),
        _ => Box::new(overlay::OverlayLayout::new(cfg.overlay.clone())),
    }
}

pub(crate) fn scalar_string(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Parse a HelmRelease (or a patch of one): chart version, the classic
/// `image.tag`, and every image pin found by walking spec.values.
pub(crate) fn parse_helm_release(doc: &serde_yaml::Value) -> DeclaredVersion {
    let chart_version = doc
        .get("spec")
        .and_then(|s| s.get("chart"))
        .and_then(|c| c.get("spec"))
        .and_then(|s| s.get("version"))
        .and_then(scalar_string);

    let values = doc.get("spec").and_then(|s| s.get("values"));

    let image_tag = values
        .and_then(|v| v.get("image"))
        .and_then(|i| i.get("tag"))
        .and_then(scalar_string);

    let mut images = Vec::new();
    if let Some(values) = values {
        walk_images(values, "values", &mut images);
    }

    DeclaredVersion {
        chart_version,
        image_tag,
        images,
    }
}

fn walk_images(node: &serde_yaml::Value, path: &str, out: &mut Vec<crate::state::DeclaredImage>) {
    let Some(map) = node.as_mapping() else {
        return;
    };
    if let Some(tag) = map
        .get(serde_yaml::Value::from("tag"))
        .and_then(scalar_string)
    {
        out.push(crate::state::DeclaredImage {
            path: path.to_string(),
            tag,
            digest: map
                .get(serde_yaml::Value::from("digest"))
                .and_then(scalar_string),
            repository: map
                .get(serde_yaml::Value::from("repository"))
                .and_then(scalar_string),
        });
    }
    for (key, value) in map {
        let Some(key) = key.as_str() else { continue };
        if value.is_mapping() {
            walk_images(value, &format!("{path}.{key}"), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_tree_lists_children() {
        let tree = MapTree(
            [
                ("apps/base/notes-web/kustomization.yml", ""),
                ("apps/base/notes-api/kustomization.yml", ""),
                ("apps/dev/notes-web/notes-web-patch.yml", ""),
                ("apps/dev/kustomization.yml", ""),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        );
        assert_eq!(
            tree.list_dirs("apps/base").unwrap(),
            ["notes-api", "notes-web"]
        );
        assert_eq!(tree.list_dirs("apps/dev").unwrap(), ["notes-web"]);
        assert_eq!(tree.list_files("apps/dev").unwrap(), ["kustomization.yml"]);
        assert!(tree.list_dirs("apps/prod").unwrap().is_empty());
    }
}
