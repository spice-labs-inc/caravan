use anyhow::Result;
use std::path::PathBuf;

/// A discovered kubeconfig file with its metadata.
#[derive(Debug, Clone)]
pub struct DiscoveredKubeconfig {
    pub path: PathBuf,
    pub contexts: Vec<String>,
    pub clusters: Vec<String>,
    /// Auto-suggested environment label (e.g., "dev", "staging", "prod")
    pub suggested_env: Option<String>,
    /// Auto-suggested type (e.g., "apps", "vault", "infra")
    pub suggested_type: Option<String>,
    /// Whether the file is readable and parseable
    pub valid: bool,
    pub error: Option<String>,
}

/// Discover kubeconfig files from common locations.
pub fn discover_kubeconfigs() -> Vec<DiscoveredKubeconfig> {
    let mut results = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let home_path = PathBuf::from(&home);

    // 1. Default ~/.kube/config
    let default_kube = home_path.join(".kube").join("config");
    if default_kube.exists() && seen_paths.insert(canonical(&default_kube)) {
        results.push(parse_kubeconfig(&default_kube));
    }

    // 2. The configured globs (`[kubeconfigs] globs`, `~` expanded)
    for pattern in crate::config::kubeconfig_globs() {
        let pattern = shellexpand::tilde(&pattern).to_string();
        if let Ok(paths) = glob::glob(&pattern) {
            for entry in paths.flatten() {
                if seen_paths.insert(canonical(&entry)) {
                    results.push(parse_kubeconfig(&entry));
                }
            }
        }
    }

    // 3. $KUBECONFIG env var (colon-separated paths)
    if let Ok(kubeconfig_env) = std::env::var("KUBECONFIG") {
        for path_str in kubeconfig_env.split(':') {
            let path = PathBuf::from(shellexpand::tilde(path_str).as_ref());
            if path.exists() && seen_paths.insert(canonical(&path)) {
                results.push(parse_kubeconfig(&path));
            }
        }
    }

    // Sort: valid files first, then by path
    results.sort_by(|a, b| b.valid.cmp(&a.valid).then_with(|| a.path.cmp(&b.path)));

    results
}

fn canonical(path: &PathBuf) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())
}

/// Parse a single kubeconfig file and return its metadata.
pub fn parse_kubeconfig_file(path: &PathBuf) -> DiscoveredKubeconfig {
    parse_kubeconfig(path)
}

fn parse_kubeconfig(path: &PathBuf) -> DiscoveredKubeconfig {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return DiscoveredKubeconfig {
                path: path.clone(),
                contexts: vec![],
                clusters: vec![],
                suggested_env: None,
                suggested_type: None,
                valid: false,
                error: Some(format!("Cannot read: {}", e)),
            };
        }
    };

    // Parse as YAML to extract contexts and clusters
    let parsed: Result<KubeconfigYaml, _> = serde_yaml::from_str(&contents);
    match parsed {
        Ok(kc) => {
            let contexts: Vec<String> = kc
                .contexts
                .unwrap_or_default()
                .into_iter()
                .filter_map(|c| c.name)
                .collect();
            let clusters: Vec<String> = kc
                .clusters
                .unwrap_or_default()
                .into_iter()
                .filter_map(|c| c.name)
                .collect();

            // Try to suggest from cluster/context names first (more reliable),
            // then fall back to filename
            let all_names: Vec<String> = contexts
                .iter()
                .chain(clusters.iter())
                .map(|s| s.to_lowercase())
                .collect();
            let (suggested_env, suggested_type) =
                suggest_from_names(&all_names).unwrap_or_else(|| {
                    let filename = path
                        .file_name()
                        .map(|f| f.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    suggest_from_name(&filename)
                });

            DiscoveredKubeconfig {
                path: path.clone(),
                contexts,
                clusters,
                suggested_env,
                suggested_type,
                valid: true,
                error: None,
            }
        }
        Err(e) => DiscoveredKubeconfig {
            path: path.clone(),
            contexts: vec![],
            clusters: vec![],
            suggested_env: None,
            suggested_type: None,
            valid: false,
            error: Some(format!("Parse error: {}", e)),
        },
    }
}

/// Suggest env and type from a single context name. Public for use by UI.
pub fn suggest_from_context(name: &str) -> (Option<String>, Option<String>) {
    suggest_from_name(&name.to_lowercase())
}

/// Try to suggest env and type from a list of cluster/context names.
/// Returns None if nothing matched.
fn suggest_from_names(names: &[String]) -> Option<(Option<String>, Option<String>)> {
    let mut env = None;
    let mut ktype = None;

    for name in names {
        let (e, t) = suggest_from_name(name);
        if e.is_some() && env.is_none() {
            env = e;
        }
        if t.is_some() && ktype.is_none() {
            ktype = t;
        }
        if env.is_some() && ktype.is_some() {
            break;
        }
    }

    if env.is_some() || ktype.is_some() {
        Some((env, ktype))
    } else {
        None
    }
}

/// Auto-suggest environment and type from a single name string.
/// Works for filenames, cluster names, or context names.
fn suggest_from_name(name: &str) -> (Option<String>, Option<String>) {
    // Configured ids first, then the long spellings of the public ladder.
    let ladder = crate::models::environments::current();
    let mut envs: Vec<(String, String)> = ladder
        .iter()
        .map(|e| (e.id.clone(), e.id.clone()))
        .collect();
    for (long, short) in [("production", "prod"), ("development", "dev")] {
        if ladder.contains(short) {
            envs.push((long.to_string(), short.to_string()));
        }
    }
    let types = ["apps", "vault", "infra", "infrastructure"];

    let suggested_env = envs
        .iter()
        .find(|(needle, _)| name.contains(needle.as_str()))
        .map(|(_, id)| id.clone());

    let suggested_type = types.iter().find(|t| name.contains(**t)).map(|t| {
        match *t {
            "infrastructure" => "infra",
            other => other,
        }
        .to_string()
    });

    (suggested_env, suggested_type)
}

// Minimal kubeconfig YAML structure for parsing
#[derive(Debug, serde::Deserialize)]
struct KubeconfigYaml {
    #[serde(default)]
    contexts: Option<Vec<KubeconfigNamedItem>>,
    #[serde(default)]
    clusters: Option<Vec<KubeconfigNamedItem>>,
}

#[derive(Debug, serde::Deserialize)]
struct KubeconfigNamedItem {
    #[serde(default)]
    name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggest_dev_apps() {
        let (env, ktype) = suggest_from_context("admin@dev-apps");
        assert_eq!(env, Some("dev".to_string()));
        assert_eq!(ktype, Some("apps".to_string()));
    }

    #[test]
    fn suggest_prod_vault() {
        let (env, ktype) = suggest_from_context("prod-vault");
        assert_eq!(env, Some("prod".to_string()));
        assert_eq!(ktype, Some("vault".to_string()));
    }

    #[test]
    fn suggest_staging_infra() {
        let (env, ktype) = suggest_from_context("staging-cluster-infra-admin");
        assert_eq!(env, Some("staging".to_string()));
        assert_eq!(ktype, Some("infra".to_string()));
    }

    #[test]
    fn suggest_unknown() {
        let (env, ktype) = suggest_from_context("some-random-context");
        assert_eq!(env, None);
        assert_eq!(ktype, None);
    }

    #[test]
    fn suggest_normalizes_production() {
        let (env, _) = suggest_from_context("production-apps");
        assert_eq!(env, Some("prod".to_string()));
    }

    #[test]
    fn suggest_normalizes_infrastructure() {
        let (_, ktype) = suggest_from_context("dev-infrastructure");
        assert_eq!(ktype, Some("infra".to_string()));
    }

    #[test]
    fn suggest_from_names_picks_first_match() {
        let names = vec![
            "default".to_string(),
            "dev-apps".to_string(),
            "staging-vault".to_string(),
        ];
        let result = suggest_from_names(&names);
        assert_eq!(
            result,
            Some((Some("dev".to_string()), Some("apps".to_string())))
        );
    }

    #[test]
    fn suggest_from_names_none_when_no_match() {
        let names = vec!["default".to_string(), "minikube".to_string()];
        let result = suggest_from_names(&names);
        assert_eq!(result, None);
    }

    #[test]
    fn parse_valid_kubeconfig() {
        let dir = std::env::temp_dir();
        let path = dir.join("test-kubeconfig.yaml");
        std::fs::write(
            &path,
            r#"
apiVersion: v1
kind: Config
clusters:
- name: test-cluster
  cluster:
    server: https://localhost:6443
contexts:
- name: test-context
  context:
    cluster: test-cluster
    user: test-user
users:
- name: test-user
"#,
        )
        .unwrap();

        let kc = parse_kubeconfig_file(&path);
        assert!(kc.valid);
        assert_eq!(kc.contexts, vec!["test-context".to_string()]);
        assert_eq!(kc.clusters, vec!["test-cluster".to_string()]);
        assert!(kc.error.is_none());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parse_invalid_kubeconfig() {
        let dir = std::env::temp_dir();
        let path = dir.join("test-bad-kubeconfig.yaml");
        std::fs::write(&path, "not: valid: yaml: {{{}}}").unwrap();

        let kc = parse_kubeconfig_file(&path);
        assert!(!kc.valid);
        assert!(kc.error.is_some());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parse_missing_file() {
        let path = std::path::PathBuf::from("/tmp/nonexistent-kubeconfig-12345.yaml");
        let kc = parse_kubeconfig_file(&path);
        assert!(!kc.valid);
        assert!(kc.error.is_some());
    }
}
