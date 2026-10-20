//! Per-environment overlay layout: a base directory per service, an
//! environment directory per rung with a kustomization listing the applied
//! services and a HelmRelease patch per service, plus an optional operator
//! whose component pins live in a ConfigMap (base data with a per-environment
//! patch overlaid key by key). Promotion copies the source environment's pins
//! into the target's files.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use tracing::warn;

use super::{
    parse_helm_release, scalar_string, ChangeSet, DeclaredSnapshot, GitopsLayout, GitopsTree,
    ServiceChange, ValueEdit,
};
use crate::models::image::ImageRef;
use crate::state::{DeclaredVersion, OperatorPins};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct OverlayConfig {
    /// Directory whose subdirectories are the services
    #[serde(default = "default_base_dir")]
    pub base_dir: String,
    /// Environment directory, `{env}` substituted
    #[serde(default = "default_env_dir")]
    pub env_dir: String,
    /// Patch file name without extension, `{service}` substituted
    #[serde(default = "default_patch_file")]
    pub patch_file: String,
    /// Base subdirectories that are not services
    #[serde(default)]
    pub exclude: Vec<String>,
    /// An operator whose component pins live in a config map
    #[serde(default)]
    pub operator: Option<OperatorConfig>,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            base_dir: default_base_dir(),
            env_dir: default_env_dir(),
            patch_file: default_patch_file(),
            exclude: Vec::new(),
            operator: None,
        }
    }
}

/// An operator config map with per-environment patches. Its `data` keys
/// that look like `{image_key_prefix}<component>{image_key_suffix}` pin one
/// component image each; `primary_image_key` is an extra key pinning
/// `primary_component`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct OperatorConfig {
    /// Directory name under the base and environment directories
    pub name: String,
    #[serde(default = "default_install_file")]
    pub install_file: String,
    #[serde(default = "default_operator_patch_file")]
    pub patch_file: String,
    /// `metadata.name` of the ConfigMap in the install file
    #[serde(default = "default_configmap")]
    pub configmap: String,
    #[serde(default = "default_image_key_prefix")]
    pub image_key_prefix: String,
    #[serde(default = "default_image_key_suffix")]
    pub image_key_suffix: String,
    #[serde(default)]
    pub primary_component: Option<String>,
    #[serde(default)]
    pub primary_image_key: Option<String>,
    #[serde(default = "default_chart_name_key")]
    pub chart_name_key: String,
    #[serde(default = "default_chart_version_key")]
    pub chart_version_key: String,
}

fn default_base_dir() -> String {
    "apps/base".into()
}
fn default_env_dir() -> String {
    "apps/{env}".into()
}
fn default_patch_file() -> String {
    "{service}-patch".into()
}
fn default_install_file() -> String {
    "install.yml".into()
}
fn default_operator_patch_file() -> String {
    "config-patch.yml".into()
}
fn default_configmap() -> String {
    "config".into()
}
fn default_image_key_prefix() -> String {
    "values.".into()
}
fn default_image_key_suffix() -> String {
    "_image".into()
}
fn default_chart_name_key() -> String {
    "chart_name".into()
}
fn default_chart_version_key() -> String {
    "chart_version".into()
}

#[allow(dead_code)]
impl OperatorConfig {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            install_file: default_install_file(),
            patch_file: default_operator_patch_file(),
            configmap: default_configmap(),
            image_key_prefix: default_image_key_prefix(),
            image_key_suffix: default_image_key_suffix(),
            primary_component: None,
            primary_image_key: None,
            chart_name_key: default_chart_name_key(),
            chart_version_key: default_chart_version_key(),
        }
    }

    /// The component a data key pins, if it is an image key.
    pub fn component_of(&self, key: &str) -> Option<String> {
        if Some(key) == self.primary_image_key.as_deref() {
            return self.primary_component.clone();
        }
        let name = key
            .strip_prefix(self.image_key_prefix.as_str())?
            .strip_suffix(self.image_key_suffix.as_str())?;
        (!name.is_empty() && !name.contains('.')).then(|| name.to_string())
    }

    /// Whether a data key is a pin a promotion copies.
    pub fn is_pin_key(&self, key: &str) -> bool {
        key == self.chart_name_key
            || key == self.chart_version_key
            || self.component_of(key).is_some()
    }

    fn chart_label(&self) -> String {
        format!(
            "{} chart",
            self.primary_component.as_deref().unwrap_or(&self.name)
        )
    }

    /// Component names a set of edits touches, chart edits as "<x> chart".
    pub fn components_of(&self, edits: &[ValueEdit]) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for e in edits {
            let name = self
                .component_of(&e.path)
                .unwrap_or_else(|| self.chart_label());
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }

    /// Component pins: the ConfigMap `data` from the base install file with
    /// the environment patch `data` overlaid key by key.
    pub fn parse_pins(&self, base_install: &str, env_patch: Option<&str>) -> OperatorPins {
        let mut data: BTreeMap<String, String> = BTreeMap::new();
        for doc in serde_yaml::Deserializer::from_str(base_install) {
            let Ok(value) = serde_yaml::Value::deserialize(doc) else {
                continue;
            };
            if value.get("kind").and_then(|k| k.as_str()) != Some("ConfigMap") {
                continue;
            }
            if value
                .get("metadata")
                .and_then(|m| m.get("name"))
                .and_then(|n| n.as_str())
                != Some(self.configmap.as_str())
            {
                continue;
            }
            merge_data(&mut data, value.get("data"));
        }
        if let Some(patch) = env_patch {
            if let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(patch) {
                merge_data(&mut data, value.get("data"));
            }
        }
        self.pins_from_data(&data)
    }

    fn pins_from_data(&self, data: &BTreeMap<String, String>) -> OperatorPins {
        let mut components = BTreeMap::new();
        for (key, value) in data {
            if Some(key.as_str()) == self.primary_image_key.as_deref() {
                continue;
            }
            if let Some(name) = self.component_of(key) {
                components.insert(name, ImageRef::parse(value));
            }
        }
        if let (Some(primary), Some(key)) = (&self.primary_component, &self.primary_image_key) {
            if !components.contains_key(primary) {
                if let Some(target) = data.get(key) {
                    components.insert(primary.clone(), ImageRef::parse(target));
                }
            }
        }
        OperatorPins {
            chart_name: data.get(&self.chart_name_key).cloned(),
            chart_version: data.get(&self.chart_version_key).cloned(),
            components,
        }
    }
}

fn merge_data(into: &mut BTreeMap<String, String>, data: Option<&serde_yaml::Value>) {
    let Some(map) = data.and_then(|d| d.as_mapping()) else {
        return;
    };
    for (k, v) in map {
        if let (Some(k), Some(v)) = (k.as_str(), scalar_string(v)) {
            into.insert(k.to_string(), v);
        }
    }
}

pub struct OverlayLayout {
    cfg: OverlayConfig,
}

impl OverlayLayout {
    pub fn new(cfg: OverlayConfig) -> Self {
        Self { cfg }
    }

    pub fn env_dir(&self, env: &str) -> String {
        self.cfg.env_dir.replace("{env}", env)
    }

    /// The two candidate patch paths of a service in an env.
    pub fn patch_paths(&self, env: &str, service: &str) -> [String; 2] {
        let name = self.cfg.patch_file.replace("{service}", service);
        ["yml", "yaml"].map(|ext| format!("{}/{service}/{name}.{ext}", self.env_dir(env)))
    }

    pub fn operator_install_path(&self) -> Option<String> {
        let op = self.cfg.operator.as_ref()?;
        Some(format!(
            "{}/{}/{}",
            self.cfg.base_dir, op.name, op.install_file
        ))
    }

    pub fn operator_patch_path(&self, env: &str) -> Option<String> {
        let op = self.cfg.operator.as_ref()?;
        Some(format!(
            "{}/{}/{}",
            self.env_dir(env),
            op.name,
            op.patch_file
        ))
    }

    fn services(&self, tree: &dyn GitopsTree) -> Result<Vec<String>> {
        let mut services = tree.list_dirs(&self.cfg.base_dir)?;
        services.retain(|s| !self.cfg.exclude.contains(s));
        Ok(services)
    }

    /// The first of `paths` that exists, with its path.
    fn read_first(tree: &dyn GitopsTree, paths: &[String]) -> Result<Option<(String, String)>> {
        for path in paths {
            if let Some(content) = tree.read(path)? {
                return Ok(Some((path.clone(), content)));
            }
        }
        Ok(None)
    }

    /// The resources an env's kustomization lists. None when the file is
    /// missing or unreadable, in which case every patch counts as applied.
    fn env_resources(&self, tree: &dyn GitopsTree, env: &str) -> Option<HashSet<String>> {
        let paths = ["yml", "yaml"].map(|ext| format!("{}/kustomization.{ext}", self.env_dir(env)));
        match Self::read_first(tree, &paths) {
            Ok(Some((_, content))) => parse_kustomization_resources(&content),
            Ok(None) => None,
            Err(e) => {
                warn!(env = %env, err = %e, "Failed to read env kustomization");
                None
            }
        }
    }

    fn operator_data(content: &str) -> Result<BTreeMap<String, String>> {
        let doc: serde_yaml::Value =
            serde_yaml::from_str(content).context("Failed to parse operator config patch")?;
        let mut data = BTreeMap::new();
        merge_data(&mut data, doc.get("data"));
        Ok(data)
    }
}

impl GitopsLayout for OverlayLayout {
    fn name(&self) -> &'static str {
        "overlay"
    }

    fn read_declared(&self, tree: &dyn GitopsTree, envs: &[String]) -> Result<DeclaredSnapshot> {
        let services = self.services(tree)?;
        let mut declared_versions: HashMap<String, HashMap<String, DeclaredVersion>> =
            HashMap::new();
        for env in envs {
            // A patch the env kustomization does not list (a commented-out
            // resource) is never applied by Flux, so it declares nothing.
            let resources = self.env_resources(tree, env);
            for service in &services {
                if resources.as_ref().is_some_and(|r| !r.contains(service)) {
                    continue;
                }
                match Self::read_first(tree, &self.patch_paths(env, service)) {
                    Ok(Some((_, content))) => {
                        declared_versions
                            .entry(service.clone())
                            .or_default()
                            .insert(env.clone(), parse_patch_yaml(&content));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(env = %env, service = %service, err = %e, "Failed to read patch")
                    }
                }
            }
        }

        let mut operator_pins = HashMap::new();
        if let (Some(op), Some(install)) = (&self.cfg.operator, self.operator_install_path()) {
            let base = tree.read(&install)?.unwrap_or_default();
            for env in envs {
                let overlay = match self.operator_patch_path(env) {
                    Some(p) => tree.read(&p)?,
                    None => None,
                };
                if overlay.is_none() && base.is_empty() {
                    continue;
                }
                let pins = op.parse_pins(&base, overlay.as_deref());
                if !pins.components.is_empty() || pins.chart_version.is_some() {
                    operator_pins.insert(env.clone(), pins);
                }
            }
        }

        Ok(DeclaredSnapshot {
            services,
            declared_versions,
            operator_pins,
        })
    }

    fn supports_promotion(&self) -> bool {
        true
    }

    fn promotable_services(&self, tree: &dyn GitopsTree, env: &str) -> Result<Vec<String>> {
        tree.list_dirs(&self.env_dir(env))
    }

    fn promotion(
        &self,
        tree: &dyn GitopsTree,
        base_sha: &str,
        from: &str,
        to: &str,
        services: &[String],
        ahead: &HashMap<String, i64>,
    ) -> Result<ChangeSet> {
        let mut cs = ChangeSet {
            base_sha: base_sha.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            operator: self.cfg.operator.as_ref().map(|o| o.name.clone()),
            ..Default::default()
        };
        let operator = self.cfg.operator.as_ref();

        for service in services {
            if operator.is_some_and(|op| &op.name == service) {
                continue;
            }
            let Some((_, source)) = Self::read_first(tree, &self.patch_paths(from, service))?
            else {
                continue;
            };
            let Some((target_path, target)) =
                Self::read_first(tree, &self.patch_paths(to, service))?
            else {
                continue;
            };
            let edits = diff_pins(&patch_pins(&source), &patch_pins(&target));
            if edits.is_empty() {
                cs.unchanged.push(service.clone());
                continue;
            }
            let warning = ahead
                .get(service)
                .filter(|n| **n > 0)
                .map(|n| ahead_warning(from, to, *n, None));
            cs.services.push(ServiceChange {
                name: service.clone(),
                file: target_path,
                edits,
                warning,
                components: Vec::new(),
            });
        }

        if let Some(op) = operator {
            let (Some(source_path), Some(target_path)) =
                (self.operator_patch_path(from), self.operator_patch_path(to))
            else {
                return Ok(cs);
            };
            if let (Some(source), Some(target)) =
                (tree.read(&source_path)?, tree.read(&target_path)?)
            {
                let source_data = Self::operator_data(&source)?;
                let target_data = Self::operator_data(&target)?;
                cs.operator_target_only = target_data
                    .keys()
                    .filter(|k| !source_data.contains_key(*k))
                    .cloned()
                    .collect();
                let source_pins: BTreeMap<String, String> = source_data
                    .into_iter()
                    .filter(|(k, _)| op.is_pin_key(k))
                    .collect();
                let edits = diff_pins(&source_pins, &target_data);
                if edits.is_empty() {
                    cs.unchanged.push(op.name.clone());
                } else {
                    let mut behind: Vec<(String, i64)> = op
                        .components_of(&edits)
                        .into_iter()
                        .filter_map(|c| ahead.get(&c).filter(|n| **n > 0).map(|n| (c, *n)))
                        .collect();
                    behind.sort();
                    let warning = behind.iter().max_by_key(|(_, n)| *n).map(|(_, n)| {
                        let names: Vec<&str> = behind.iter().map(|(c, _)| c.as_str()).collect();
                        ahead_warning(from, to, *n, Some(&names.join(", ")))
                    });
                    let components = op.components_of(&edits);
                    cs.services.push(ServiceChange {
                        name: op.name.clone(),
                        file: target_path,
                        edits,
                        warning,
                        components,
                    });
                }
            }
        }

        Ok(cs)
    }
}

fn ahead_warning(from: &str, to: &str, n: i64, components: Option<&str>) -> String {
    let where_ = match components {
        Some(c) => format!(" ({c})"),
        None => String::new(),
    };
    format!("{n} {to} is ahead here{where_} · promoting moves it back to the {from} build")
}

/// Edits for every source pin whose target value exists and differs.
fn diff_pins(
    source: &BTreeMap<String, String>,
    target: &BTreeMap<String, String>,
) -> Vec<ValueEdit> {
    source
        .iter()
        .filter_map(|(path, new)| {
            let old = target.get(path)?;
            (old != new).then(|| ValueEdit {
                path: path.clone(),
                old: old.clone(),
                new: new.clone(),
                same_image: same_digest(old, new),
            })
        })
        .collect()
}

fn same_digest(a: &str, b: &str) -> bool {
    use crate::models::image::digests_equal;
    match (ImageRef::parse(a).digest, ImageRef::parse(b).digest) {
        (Some(x), Some(y)) => digests_equal(&x, &y),
        _ => false,
    }
}

/// `chart.spec.version` plus every `tag`, `digest` and `version` scalar
/// under spec.values, keyed by dotted path.
fn patch_pins(content: &str) -> BTreeMap<String, String> {
    let mut pins = BTreeMap::new();
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(content) else {
        return pins;
    };
    let Some(spec) = doc.get("spec") else {
        return pins;
    };
    if let Some(v) = spec
        .get("chart")
        .and_then(|c| c.get("spec"))
        .and_then(|s| s.get("version"))
        .and_then(scalar_string)
    {
        pins.insert("chart.spec.version".to_string(), v);
    }
    if let Some(values) = spec.get("values") {
        walk_pins(values, "values", &mut pins);
    }
    pins
}

fn walk_pins(node: &serde_yaml::Value, path: &str, out: &mut BTreeMap<String, String>) {
    let Some(map) = node.as_mapping() else {
        return;
    };
    for (key, value) in map {
        let Some(key) = key.as_str() else { continue };
        let child = format!("{path}.{key}");
        if value.is_mapping() {
            walk_pins(value, &child, out);
        } else if matches!(key, "tag" | "digest" | "version") {
            if let Some(s) = scalar_string(value) {
                out.insert(child, s);
            }
        }
    }
}

/// Parse a HelmRelease patch into its declared version.
pub(crate) fn parse_patch_yaml(content: &str) -> DeclaredVersion {
    match serde_yaml::from_str::<serde_yaml::Value>(content) {
        Ok(doc) => parse_helm_release(&doc),
        Err(_) => DeclaredVersion::default(),
    }
}

pub(crate) fn parse_kustomization_resources(content: &str) -> Option<HashSet<String>> {
    let doc: serde_yaml::Value = serde_yaml::from_str(content).ok()?;
    let resources = doc.get("resources")?.as_sequence()?;
    Some(
        resources
            .iter()
            .filter_map(|r| r.as_str())
            .map(|r| {
                let r = r.trim().trim_end_matches('/');
                r.strip_prefix("./").unwrap_or(r).to_string()
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator shape the fixtures use.
    fn op() -> OperatorConfig {
        OperatorConfig {
            primary_component: Some("engine".into()),
            primary_image_key: Some("target_image".into()),
            ..OperatorConfig::new("operator")
        }
    }

    const WORKER: &str = r#"
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: notes-worker
spec:
  chart:
    spec:
      version: 0.1.0-27ef94dc1fe4148af50c1ecd2b95f0a2e27bcc54
  values:
    image:
      registry: ghcr.io/notes-co
    worker:
      image:
        tag: tree-3438098b2d850f296438bfe9d15b485e47c4077d
        digest: sha256:5af0a017e7dff8f41782113a42698a26956b1871a357b216e543e298995fb468
    ui:
      image:
        tag: tree-c63b2527040be51952f47711eeb1bd5a58ef6539
        digest: sha256:bec940e59cc9d0c56bd23b34eef95d217e66aa501c367eedf75a527f003891fb
    sampleData:
      image:
        tag: tree-f59eb665d87374d9614be653d11028e4186d9336
        digest: sha256:064b6d0e4ef109895cf1e676f67894d13b1829fc85df4cddab9ff08e51f8cb2c
    pqcOrchestrator:
      image:
        tag: tree-a1030c56842436bc88b9db47fe6972e32d1e8ac5
        digest: sha256:b9acf54ae2e643ca71beff2966eb80ee81cc898e2c5bf81bec4e712b7e4d015f
"#;

    const NOTES_API: &str = r#"
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: notes-api
spec:
  chart:
    spec:
      version: 0.1.0-38f2c7ba3b853148b11662c429dce4a53b928482
  values:
    image:
      tag: sha-38f2c7ba3b853148b11662c429dce4a53b928482
    publicApiUrl: "https://api.staging.notes-co.example"
    dashboardHosts:
      - "staging.notes-co.example"
    pause:
      enabled: true
      timeoutSeconds: 7200
"#;

    const NOTES_WEB: &str = r#"
spec:
  chart:
    spec:
      version: 0.1.0-61ace0a101a475d9bf2b7c63da376a5ff88895d1
  values:
    api:
      image:
        tag: pr-11@sha256:0d2585f6f2fc03aff453c993fc4fad13d81363ca41f350d180c3dbd8b0796540
    ingest:
      image:
        tag: pr-11@sha256:0f51edb4be6a7bd88a5becabe72e86d03f4a633a05745cd52d4326f9c5d3651a
"#;

    const OPERATOR_BASE: &str = r#"
apiVersion: v1
kind: Namespace
metadata:
  name: operator
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: operator
  namespace: operator
spec:
  chart:
    spec:
      chart: operator
      version: "1.0.0"
  values:
    vault:
      url: https://vault.${domain}:8200
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: config
data:
  vault_addr: https://vault.${domain}:8200
  target_image: ghcr.io/notes-co/engine:REPLACEME
  values.crypto_image: ghcr.io/notes-co/crypto:REPLACEME
  values.rogues_image: gchr.io/notes-co/pqc-rogues-gallery:REPLACEME
  values.sqld_image: "ghcr.io/notes-co/libsql-server:a64d504@sha256:f07e8119b33a266da781329f427b020649104de660c2a181dab12bed28d04412"
  values: |
    log:
      filter: "info"
---
apiVersion: external-secrets.io/v1
kind: ClusterExternalSecret
metadata:
  name: engine-blob-store
spec:
  externalSecretName: "blob-store-config"
"#;

    const OPERATOR_STAGING: &str = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: config
data:
  chart_name: engine
  chart_version: 1.0.0-bedcc26732af16ab6a55811a6b7f4c7b8fb842a5
  target_image: ghcr.io/notes-co/engine:sha-e3f01e86a467c48fed87c4de092ec00343b28c1f@sha256:44009d72cc98b13eec906dde0f0498a24901d951a6cc6548b2fc65d96e65d77b
  values.crypto_image: ghcr.io/notes-co/crypto:sha-e3f01e86a467c48fed87c4de092ec00343b28c1f@sha256:42060262e5048e5ea09499dde0b20d99595c2254d3423c9247ac6a63d7367b15
  values.rogues_image: ghcr.io/notes-co/pqc-rogues-gallery:sha-894e9fed629816e72632593df569412cf2990775
  fuse_sidecar_image: ghcr.io/notes-co/engine-fuse-sidecar:sha-f058488f480162fc3a27768cef67905c43eea673
  values.mailer_image: ghcr.io/notes-co/mailer:sha-e3f01e86a467c48fed87c4de092ec00343b28c1f@sha256:91cc3b2f72646a835ef4508cc84ca1173bb4f9affdfcde8fd1d345166541dc56
  values.broker_image: ghcr.io/notes-co/broker:sha-1e758420189b97dd2fed99f97c35e7e1c6510f45@sha256:764cc7326a9fbe2812f5ccd689877949c4335821bfe4fc577798b7fe68fb40ab
  values.engine_image: ghcr.io/notes-co/engine:sha-e3f01e86a467c48fed87c4de092ec00343b28c1f@sha256:44009d72cc98b13eec906dde0f0498a24901d951a6cc6548b2fc65d96e65d77b
  values.broker.mintJobs: "true"
  values.broker.renderDispatchEnabled: "true"
  values.report_image: ghcr.io/notes-co/report:sha-e3f01e86a467c48fed87c4de092ec00343b28c1f@sha256:6b6aa893a68034a6725fd4e18f71c619971e3ab55d4275da151006b381596a47
  values.search_image: ghcr.io/notes-co/search:sha-7539d3262693cf990ac284e8c36500f5250484e1@sha256:af41b8e7d27afb46322b1da04f9073fd5df9b2e56434c56daea828a83da5087b
"#;

    const OPERATOR_DEV: &str = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: config
data:
  chart_name: engine
  chart_version: 1.0.0-bedcc26732af16ab6a55811a6b7f4c7b8fb842a5
  target_image: ghcr.io/notes-co/engine:pr-612@sha256:44009d72cc98b13eec906dde0f0498a24901d951a6cc6548b2fc65d96e65d77b
  values.crypto_image: ghcr.io/notes-co/crypto:pr-612@sha256:42060262e5048e5ea09499dde0b20d99595c2254d3423c9247ac6a63d7367b15
  values.rogues_image: ghcr.io/notes-co/pqc-rogues-gallery:sha-d091740b4c4d3641ef43d0a50875656ce31aa31e
  values.mailer_image: ghcr.io/notes-co/mailer:pr-612@sha256:91cc3b2f72646a835ef4508cc84ca1173bb4f9affdfcde8fd1d345166541dc56
  values.broker_image: ghcr.io/notes-co/broker:pr-620@sha256:7814242c2522d9beb70c2cbe12b0b15e8308e260a5fe380fba35a10bfbeab555
  values.engine_image: ghcr.io/notes-co/engine:pr-612@sha256:44009d72cc98b13eec906dde0f0498a24901d951a6cc6548b2fc65d96e65d77b
  values.broker.mintJobs: "true"
  values.report_image: ghcr.io/notes-co/report:pr-612@sha256:6b6aa893a68034a6725fd4e18f71c619971e3ab55d4275da151006b381596a47
  values.search_image: ghcr.io/notes-co/search:sha-7539d3262693cf990ac284e8c36500f5250484e1@sha256:af41b8e7d27afb46322b1da04f9073fd5df9b2e56434c56daea828a83da5087b
  values.sqld_image: "ghcr.io/notes-co/libsql-server:a64d504@sha256:f07e8119b33a266da781329f427b020649104de660c2a181dab12bed28d04412"
  values.truffle_url: "https://truffle.notes-co.example"
"#;

    #[test]
    fn worker_bouche_patch_yields_four_images() {
        let dv = parse_patch_yaml(WORKER);
        assert_eq!(
            dv.chart_version.as_deref(),
            Some("0.1.0-27ef94dc1fe4148af50c1ecd2b95f0a2e27bcc54")
        );
        assert_eq!(dv.image_tag, None, "values.image has no tag");
        let paths: Vec<&str> = dv.images.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "values.worker.image",
                "values.ui.image",
                "values.sampleData.image",
                "values.pqcOrchestrator.image"
            ]
        );
        let worker = &dv.images[0];
        assert_eq!(worker.tag, "tree-3438098b2d850f296438bfe9d15b485e47c4077d");
        assert_eq!(
            worker.digest.as_deref(),
            Some("sha256:5af0a017e7dff8f41782113a42698a26956b1871a357b216e543e298995fb468")
        );
        assert_eq!(worker.repository, None);
        assert_eq!(worker.effective_digest(), worker.digest.as_deref());
    }

    #[test]
    fn api_patch_yields_the_classic_image_tag() {
        let dv = parse_patch_yaml(NOTES_API);
        assert_eq!(
            dv.chart_version.as_deref(),
            Some("0.1.0-38f2c7ba3b853148b11662c429dce4a53b928482")
        );
        assert_eq!(
            dv.image_tag.as_deref(),
            Some("sha-38f2c7ba3b853148b11662c429dce4a53b928482")
        );
        assert_eq!(dv.images.len(), 1);
        assert_eq!(dv.images[0].path, "values.image");
        assert_eq!(dv.images[0].digest, None);
    }

    #[test]
    fn web_tags_carry_their_digest() {
        let dv = parse_patch_yaml(NOTES_WEB);
        assert_eq!(dv.images.len(), 2);
        let api = &dv.images[0];
        assert_eq!(api.path, "values.api.image");
        assert_eq!(api.bare_tag(), "pr-11");
        assert_eq!(
            api.effective_digest(),
            Some("sha256:0d2585f6f2fc03aff453c993fc4fad13d81363ca41f350d180c3dbd8b0796540")
        );
    }

    #[test]
    fn unparseable_patch_is_empty() {
        let dv = parse_patch_yaml("spec: [unclosed");
        assert_eq!(dv, DeclaredVersion::default());
    }

    #[test]
    fn commented_out_resources_are_not_listed() {
        let resources = parse_kustomization_resources(
            r#"
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - operator
  #- docs
  - ./notes-api/
  - ../base/public
patches:
  - target:
      kind: Gateway
"#,
        )
        .unwrap();
        assert!(resources.contains("operator"));
        assert!(resources.contains("notes-api"));
        assert!(!resources.contains("notes-docs"));
    }

    #[test]
    fn kustomization_without_resources_lists_nothing() {
        assert_eq!(parse_kustomization_resources("kind: Kustomization"), None);
        assert_eq!(parse_kustomization_resources("resources: [unclosed"), None);
    }

    #[test]
    fn operator_staging_overlays_base() {
        let pins = op().parse_pins(OPERATOR_BASE, Some(OPERATOR_STAGING));
        assert_eq!(pins.chart_name.as_deref(), Some("engine"));
        assert_eq!(
            pins.chart_version.as_deref(),
            Some("1.0.0-bedcc26732af16ab6a55811a6b7f4c7b8fb842a5")
        );
        let names: Vec<&str> = pins.components.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec!["broker", "crypto", "engine", "mailer", "report", "rogues", "search", "sqld"]
        );
        // sqld comes from base, staging does not override it
        let sqld = &pins.components["sqld"];
        assert_eq!(sqld.name, "libsql-server");
        assert_eq!(sqld.tag.as_deref(), Some("a64d504"));
        // engine comes from values.engine_image, not target_image
        let engine = &pins.components["engine"];
        assert_eq!(
            engine.tag.as_deref(),
            Some("sha-e3f01e86a467c48fed87c4de092ec00343b28c1f")
        );
        // rogues overlay replaces the base placeholder
        assert_eq!(
            pins.components["rogues"].tag.as_deref(),
            Some("sha-894e9fed629816e72632593df569412cf2990775")
        );
        assert!(!pins.components.contains_key("fuse_sidecar"));
        assert!(!pins.components.contains_key("broker.mintJobs"));
    }

    #[test]
    fn operator_dev_pr_tags() {
        let pins = op().parse_pins(OPERATOR_BASE, Some(OPERATOR_DEV));
        assert_eq!(pins.components["broker"].tag.as_deref(), Some("pr-620"));
        assert_eq!(
            pins.components["broker"].digest.as_deref(),
            Some("sha256:7814242c2522d9beb70c2cbe12b0b15e8308e260a5fe380fba35a10bfbeab555")
        );
        assert_eq!(pins.components["engine"].tag.as_deref(), Some("pr-612"));
        assert_eq!(pins.components.len(), 8);
    }

    #[test]
    fn operator_target_image_is_engine_only_without_engine_image() {
        let pins = op().parse_pins(OPERATOR_BASE, None);
        assert_eq!(pins.components["engine"].tag.as_deref(), Some("REPLACEME"));
        assert_eq!(pins.chart_version, None);
        let only_patch = "data:\n  target_image: ghcr.io/x/engine:v1.0.0\n";
        let pins = op().parse_pins("", Some(only_patch));
        assert_eq!(pins.components["engine"].tag.as_deref(), Some("v1.0.0"));
        assert_eq!(pins.components.len(), 1);
    }
}
