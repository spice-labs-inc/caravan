//! The environment ladder: an ordered list of deployment environments from
//! config, each optionally promoting from an upstream one. One environment is
//! the reference whose failures count most.

use serde::Deserialize;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Environment {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    /// Hex colour, `#RRGGBB`
    #[serde(default)]
    pub color: Option<String>,
    /// The environment this one promotes from
    #[serde(default)]
    pub upstream: Option<String>,
    /// Failures here are the most severe
    #[serde(default)]
    pub reference: bool,
    /// Empty when no cluster is attached
    #[serde(default)]
    pub kubeconfig: String,
    #[serde(default)]
    pub context: Option<String>,
}

impl Environment {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            label: None,
            color: None,
            upstream: None,
            reference: false,
            kubeconfig: String::new(),
            context: None,
        }
    }

    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }

    pub fn has_cluster(&self) -> bool {
        !self.kubeconfig.trim().is_empty()
    }
}

/// Ordered environments; the order is the display order (most important first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environments(Vec<Environment>);

/// `[[environments]]` is the shape; the older `[environments.<id>]` tables
/// still load, ordered like the public default where the ids match and
/// alphabetically otherwise.
impl<'de> Deserialize<'de> for Environments {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Legacy {
            #[serde(default)]
            label: Option<String>,
            #[serde(default)]
            color: Option<String>,
            #[serde(default)]
            upstream: Option<String>,
            #[serde(default)]
            reference: bool,
            #[serde(default)]
            kubeconfig: String,
            #[serde(default)]
            context: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            List(Vec<Environment>),
            Tables(std::collections::BTreeMap<String, Legacy>),
        }
        match Shape::deserialize(d)? {
            Shape::List(envs) => Ok(Self(envs)),
            Shape::Tables(map) => {
                let defaults = Environments::public_default();
                let mut envs: Vec<Environment> = map
                    .into_iter()
                    .map(|(id, l)| {
                        let d = defaults.get(&id);
                        Environment {
                            id,
                            label: l.label,
                            color: l.color,
                            upstream: l.upstream.or_else(|| d.and_then(|d| d.upstream.clone())),
                            reference: l.reference || d.is_some_and(|d| d.reference),
                            kubeconfig: l.kubeconfig,
                            context: l.context,
                        }
                    })
                    .collect();
                envs.sort_by_key(|e| (defaults.rank(&e.id), e.id.clone()));
                Ok(Self(envs))
            }
        }
    }
}

impl Default for Environments {
    fn default() -> Self {
        Self::public_default()
    }
}

#[allow(dead_code)]
impl Environments {
    pub fn new(envs: Vec<Environment>) -> Self {
        Self(envs)
    }

    /// The ladder a fresh install gets: prod (reference), staging, dev.
    pub fn public_default() -> Self {
        Self(vec![
            Environment {
                label: Some("Production".into()),
                color: Some("#D26E28".into()),
                reference: true,
                ..Environment::new("prod")
            },
            Environment {
                label: Some("Staging".into()),
                color: Some("#8250DC".into()),
                upstream: Some("prod".into()),
                ..Environment::new("staging")
            },
            Environment {
                label: Some("Development".into()),
                color: Some("#3282DC".into()),
                upstream: Some("staging".into()),
                ..Environment::new("dev")
            },
        ])
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Environment> {
        self.0.iter()
    }

    /// Ids in pipeline order, borrowed.
    pub fn pipeline_order_ids(&self) -> Vec<&str> {
        self.0.iter().rev().map(|e| e.id.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&Environment> {
        self.0.iter().find(|e| e.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// Ids in display order.
    pub fn order(&self) -> Vec<String> {
        self.0.iter().map(|e| e.id.clone()).collect()
    }

    /// Ids in display order, borrowed.
    pub fn ids(&self) -> Vec<&str> {
        self.0.iter().map(|e| e.id.as_str()).collect()
    }

    /// Position in display order; unknown ids sort last.
    pub fn rank(&self, id: &str) -> usize {
        self.0
            .iter()
            .position(|e| e.id == id)
            .unwrap_or(self.0.len())
    }

    /// Ids in pipeline order: the lowest rung first, the reference last.
    pub fn pipeline_order(&self) -> Vec<String> {
        let mut ids = self.order();
        ids.reverse();
        ids
    }

    /// The environment `id` promotes from.
    pub fn upstream_of(&self, id: &str) -> Option<&str> {
        self.get(id)?.upstream.as_deref()
    }

    /// The environments that promote from `id`, in display order.
    pub fn downstream_of(&self, id: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|e| e.upstream.as_deref() == Some(id))
            .map(|e| e.id.as_str())
            .collect()
    }

    /// Every ancestor of `id` walking upstream, nearest first.
    pub fn ancestors_of(&self, id: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut cur = self.upstream_of(id);
        while let Some(up) = cur {
            if out.contains(&up) || up == id {
                break;
            }
            out.push(up);
            cur = self.upstream_of(up);
        }
        out
    }

    /// Whether walking upstream from `id` comes back to `id`.
    fn in_cycle(&self, id: &str) -> bool {
        let mut seen = vec![id];
        let mut cur = self.upstream_of(id);
        while let Some(up) = cur {
            if up == id {
                return true;
            }
            if seen.contains(&up) {
                return false;
            }
            seen.push(up);
            cur = self.upstream_of(up);
        }
        false
    }

    /// The reference environment: the one flagged, else the first.
    pub fn reference(&self) -> Option<&Environment> {
        self.0
            .iter()
            .find(|e| e.reference)
            .or_else(|| self.0.first())
    }

    pub fn reference_id(&self) -> Option<&str> {
        self.reference().map(|e| e.id.as_str())
    }

    pub fn is_reference(&self, id: &str) -> bool {
        self.reference_id() == Some(id)
    }

    /// The environment releases are cut from: the first that promotes from
    /// the reference.
    pub fn release_source(&self) -> Option<&str> {
        let reference = self.reference_id()?;
        self.downstream_of(reference).into_iter().next()
    }

    /// (ancestor, descendant) pairs whose deployed commits are related.
    pub fn relation_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        for env in &self.0 {
            for anc in self.ancestors_of(&env.id) {
                pairs.push((anc.to_string(), env.id.clone()));
            }
        }
        pairs
    }

    /// (upstream, downstream) edges in display order of the downstream.
    pub fn promotion_edges(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .filter_map(|e| e.upstream.as_ref().map(|up| (up.clone(), e.id.clone())))
            .collect()
    }

    /// The default promotion target: the last environment with an upstream.
    pub fn default_promote_target(&self) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|e| e.upstream.is_some())
            .map(|e| e.id.as_str())
    }

    pub fn label(&self, id: &str) -> String {
        self.get(id)
            .map(|e| e.label().to_string())
            .unwrap_or_else(|| id.to_string())
    }

    /// The configured colour, parsed; None when unset or malformed.
    pub fn color_rgb(&self, id: &str) -> Option<(u8, u8, u8)> {
        parse_hex(self.get(id)?.color.as_deref()?)
    }

    pub fn color_hex(&self, id: &str) -> Option<&str> {
        self.get(id)?.color.as_deref()
    }

    /// Ids of environments with a cluster attached, in display order.
    pub fn with_clusters(&self) -> impl Iterator<Item = &Environment> {
        self.0.iter().filter(|e| e.has_cluster())
    }

    /// Problems in the ladder that make it unusable.
    pub fn validate(&self) -> Result<(), String> {
        if self.0.is_empty() {
            return Err("no environments configured".into());
        }
        for env in &self.0 {
            if env.id.trim().is_empty() {
                return Err("an environment has an empty id".into());
            }
            if self.0.iter().filter(|e| e.id == env.id).count() > 1 {
                return Err(format!("environment {} is listed twice", env.id));
            }
            if let Some(up) = &env.upstream {
                if !self.contains(up) {
                    return Err(format!(
                        "environment {} promotes from unknown environment {up}",
                        env.id
                    ));
                }
                if up == &env.id {
                    return Err(format!("environment {} promotes from itself", env.id));
                }
            }
            if self.in_cycle(&env.id) {
                return Err(format!("environment {} is in a promotion cycle", env.id));
            }
        }
        if self.0.iter().filter(|e| e.reference).count() > 1 {
            return Err("more than one environment is marked reference".into());
        }
        Ok(())
    }
}

pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

static CURRENT: RwLock<Option<Arc<Environments>>> = RwLock::new(None);

/// The ladder in force: installed from config at startup and after a settings
/// save; the public default before that.
pub fn current() -> Arc<Environments> {
    if let Ok(guard) = CURRENT.read() {
        if let Some(envs) = guard.as_ref() {
            return envs.clone();
        }
    }
    Arc::new(Environments::public_default())
}

pub fn install(envs: Environments) {
    if let Ok(mut guard) = CURRENT.write() {
        *guard = Some(Arc::new(envs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_default_ladder() {
        let envs = Environments::public_default();
        assert!(envs.validate().is_ok());
        assert_eq!(envs.order(), ["prod", "staging", "dev"]);
        assert_eq!(envs.pipeline_order(), ["dev", "staging", "prod"]);
        assert_eq!(envs.reference_id(), Some("prod"));
        assert_eq!(envs.upstream_of("dev"), Some("staging"));
        assert_eq!(envs.upstream_of("prod"), None);
        assert_eq!(envs.downstream_of("prod"), ["staging"]);
        assert_eq!(envs.ancestors_of("dev"), ["staging", "prod"]);
        assert_eq!(envs.release_source(), Some("staging"));
        assert_eq!(envs.default_promote_target(), Some("dev"));
        assert_eq!(
            envs.relation_pairs(),
            [
                ("prod".to_string(), "staging".to_string()),
                ("staging".to_string(), "dev".to_string()),
                ("prod".to_string(), "dev".to_string()),
            ]
        );
        assert_eq!(envs.color_rgb("prod"), Some((0xD2, 0x6E, 0x28)));
        assert_eq!(envs.label("dev"), "Development");
        assert_eq!(envs.label("nope"), "nope");
        assert_eq!(envs.rank("nope"), 3);
    }

    #[test]
    fn ladder_parses_from_toml() {
        #[derive(Deserialize)]
        struct Wrap {
            environments: Environments,
        }
        let w: Wrap = toml::from_str(
            r##"
[[environments]]
id = "prod"
reference = true
kubeconfig = "~/.kube/prod.yaml"

[[environments]]
id = "staging"
upstream = "prod"
color = "#112233"
"##,
        )
        .unwrap();
        assert_eq!(w.environments.order(), ["prod", "staging"]);
        assert!(w.environments.get("prod").unwrap().has_cluster());
        assert!(!w.environments.get("staging").unwrap().has_cluster());
        assert_eq!(
            w.environments.color_rgb("staging"),
            Some((0x11, 0x22, 0x33))
        );
    }

    #[test]
    fn validation_catches_bad_ladders() {
        let mut bad = Environment::new("a");
        bad.upstream = Some("zzz".into());
        assert!(Environments::new(vec![bad]).validate().is_err());
        let mut a = Environment::new("a");
        a.upstream = Some("b".into());
        let mut b = Environment::new("b");
        b.upstream = Some("a".into());
        assert!(Environments::new(vec![a, b]).validate().is_err());
        assert!(Environments::new(vec![]).validate().is_err());
        assert!(
            Environments::new(vec![Environment::new("x"), Environment::new("x")])
                .validate()
                .is_err()
        );
    }
}
