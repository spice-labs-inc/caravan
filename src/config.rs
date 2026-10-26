use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

pub use crate::models::environments::{Environment, Environments};

/// Top-level configuration loaded from ~/.config/caravan/config.toml
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub github: GithubConfig,
    /// The environment ladder, most important first
    #[serde(default)]
    pub environments: Environments,
    #[serde(default)]
    pub vault: HashMap<String, VaultConfig>,
    #[serde(default)]
    pub honeycomb: HashMap<String, HoneycombConfig>,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub polling: PollingConfig,
    #[serde(default)]
    pub urls: UrlConfig,
    #[serde(default)]
    pub repos: ReposConfig,
    #[serde(default)]
    pub promote: PromoteConfig,
    #[serde(default)]
    pub gitops: GitopsConfig,
    #[serde(default)]
    pub openobserve: OpenObserveConfig,
    #[serde(default)]
    pub flux: FluxConfig,
    #[serde(default)]
    pub kubeconfigs: KubeconfigsConfig,
    #[serde(default)]
    pub update: UpdateConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubConfig {
    pub org: String,
    /// Three-tier secret resolution: token_cmd > token_env > token
    #[serde(default)]
    pub token_cmd: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    /// Client id of the GitHub OAuth App (device flow enabled) this
    /// deployment registered. No default: each deployment registers its own.
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    /// Name of the gitops repo in `org` (release PRs, the mirror remote).
    #[serde(default = "default_gitops_repo")]
    pub gitops_repo: String,
    /// Git remote of the gitops repo; the local mirror clones and fetches
    /// from here over ssh with the user's own keys. Empty means
    /// `git@github.com:{org}/{gitops_repo}.git` (filled by `load_config`).
    #[serde(default, alias = "app_gitops_remote", rename = "gitops_remote")]
    pub app_gitops_remote: String,
    /// Repos whose name starts with this are GitHub Actions, not services.
    #[serde(default = "default_action_repo_prefix")]
    pub action_repo_prefix: String,
    /// Title prefix of a release PR on the gitops repo: "{prefix}{service} v1.2.3".
    #[serde(default = "default_release_pr_prefix")]
    pub release_pr_prefix: String,
}

/// Flux objects the console syncs and reads on every environment cluster.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct FluxConfig {
    #[serde(default = "default_flux_namespace")]
    pub namespace: String,
    #[serde(default = "default_flux_git_repository")]
    pub git_repository: String,
    #[serde(default = "default_flux_kustomization")]
    pub kustomization: String,
}

impl Default for FluxConfig {
    fn default() -> Self {
        Self {
            namespace: default_flux_namespace(),
            git_repository: default_flux_git_repository(),
            kustomization: default_flux_kustomization(),
        }
    }
}

/// Where the setup wizard and settings look for kubeconfig files, besides
/// ~/.kube/config and $KUBECONFIG.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct KubeconfigsConfig {
    /// Glob patterns, `~` expanded.
    #[serde(default = "default_kubeconfig_globs")]
    pub globs: Vec<String>,
}

impl Default for KubeconfigsConfig {
    fn default() -> Self {
        Self {
            globs: default_kubeconfig_globs(),
        }
    }
}

/// Self-update source. None means the update check is off.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UpdateConfig {
    /// "owner/repo" whose GitHub releases carry this app's binaries.
    #[serde(default)]
    pub repo: Option<String>,
}

/// One rung of the ladder; see `models::environments`.
pub type EnvironmentConfig = Environment;

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct VaultConfig {
    pub url: String,
    #[serde(default)]
    pub kubeconfig: Option<String>,
    /// Namespace the vault pods run in
    #[serde(default = "default_vault_namespace")]
    pub namespace: String,
    /// Label selector that finds the vault server pods
    #[serde(default = "default_vault_label_selector")]
    pub label_selector: String,
    /// Command printing one unseal key per line
    #[serde(default)]
    pub unseal_keys_cmd: Option<String>,
    /// Keep entered keys in the keyring for later unseals
    #[serde(default)]
    pub remember_keys: bool,
    /// Unseal without asking when a key source resolves
    #[serde(default)]
    pub auto_unseal: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct HoneycombConfig {
    #[serde(default)]
    pub api_key_cmd: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Failure notifications per environment id. An environment not listed
    /// notifies unless it is the bottom rung of the ladder.
    #[serde(default)]
    pub failures: BTreeMap<String, bool>,
    #[serde(default = "default_true")]
    pub release_pr_reminders: bool,
    #[serde(default = "default_true")]
    pub deploy_completions: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failures: BTreeMap::new(),
            release_pr_reminders: true,
            deploy_completions: true,
        }
    }
}

impl NotificationConfig {
    /// Whether failures in `env` notify, given the ladder in force.
    pub fn failures_enabled(&self, env: &str, envs: &Environments) -> bool {
        match self.failures.get(env) {
            Some(v) => *v,
            None => envs.default_promote_target() != Some(env),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct PollingConfig {
    #[serde(default = "default_k8s_interval")]
    pub k8s_interval_secs: u64,
    #[serde(default = "default_github_interval")]
    pub github_interval_secs: u64,
    #[serde(default = "default_github_issues_interval")]
    pub github_issues_interval_secs: u64,
    #[serde(default = "default_github_releases_interval")]
    pub github_releases_interval_secs: u64,
    #[serde(default = "default_gitops_interval")]
    pub gitops_interval_secs: u64,
    #[serde(default = "default_honeycomb_interval")]
    pub honeycomb_interval_secs: u64,
    #[serde(default = "default_registry_interval")]
    pub registry_interval_secs: u64,
    #[serde(default = "default_vault_interval")]
    pub vault_interval_secs: u64,
    #[serde(default = "default_vault_sealed_interval")]
    pub vault_sealed_interval_secs: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            k8s_interval_secs: default_k8s_interval(),
            github_interval_secs: default_github_interval(),
            github_issues_interval_secs: default_github_issues_interval(),
            github_releases_interval_secs: default_github_releases_interval(),
            gitops_interval_secs: default_gitops_interval(),
            honeycomb_interval_secs: default_honeycomb_interval(),
            registry_interval_secs: default_registry_interval(),
            vault_interval_secs: default_vault_interval(),
            vault_sealed_interval_secs: default_vault_sealed_interval(),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct UrlConfig {
    #[serde(default = "default_service_pattern")]
    pub service_pattern: String,
    #[serde(default = "default_vault_pattern")]
    pub vault_pattern: String,
}

/// Overrides for repo discovery. Services, components and libraries are
/// discovered from the gitops repo, the operator and the clusters; this is for the rare
/// exception.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ReposConfig {
    /// Repos and deployed apps never shown (seeds the hidden set at startup)
    #[serde(default)]
    pub hide: Vec<String>,
    /// repo name -> service name, when a repo deploys under another name
    #[serde(default)]
    pub service_names: HashMap<String, String>,
}

/// How the gitops repository is laid out; see `crate::gitops`.
#[derive(Debug, Clone, Deserialize)]
pub struct GitopsConfig {
    /// "overlay" (default) or "cluster-base"
    #[serde(default = "default_layout")]
    pub layout: String,
    /// Branch the declared versions are read from and promotions push to
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default)]
    pub overlay: crate::gitops::overlay::OverlayConfig,
    #[serde(default)]
    pub cluster_base: crate::gitops::cluster_base::ClusterBaseConfig,
}

impl Default for GitopsConfig {
    fn default() -> Self {
        Self {
            layout: default_layout(),
            branch: default_branch(),
            overlay: Default::default(),
            cluster_base: Default::default(),
        }
    }
}

fn default_layout() -> String {
    "overlay".to_string()
}
fn default_branch() -> String {
    "main".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct PromoteConfig {
    /// Whether this install promotes at all; false hides the promote button
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Trigger a Flux sync of the target after a promotion push
    #[serde(default = "default_true")]
    pub sync_after_push: bool,
    /// The environment promotions write to; default: the bottom rung
    #[serde(default)]
    pub target: Option<String>,
    /// The environment promotions copy from; default: the target's upstream
    #[serde(default)]
    pub source: Option<String>,
}

impl Default for PromoteConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sync_after_push: true,
            target: None,
            source: None,
        }
    }
}

impl PromoteConfig {
    /// (source, target) of the promotion this install performs, if promotion is
    /// enabled and the ladder has one.
    pub fn pair(&self, envs: &Environments) -> Option<(String, String)> {
        if !self.enabled {
            return None;
        }
        let target = self
            .target
            .clone()
            .or_else(|| envs.default_promote_target().map(str::to_string))?;
        let source = self
            .source
            .clone()
            .or_else(|| envs.upstream_of(&target).map(str::to_string))?;
        Some((source, target))
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct OpenObserveConfig {
    #[serde(default = "default_openobserve_url")]
    pub url: String,
    #[serde(default = "default_openobserve_org")]
    pub org: String,
}

impl Default for OpenObserveConfig {
    fn default() -> Self {
        Self {
            url: default_openobserve_url(),
            org: default_openobserve_org(),
        }
    }
}

// Defaults
fn default_true() -> bool {
    true
}
fn default_gitops_repo() -> String {
    "gitops".to_string()
}
fn default_action_repo_prefix() -> String {
    "action-".to_string()
}
fn default_release_pr_prefix() -> String {
    "Release ".to_string()
}
fn default_flux_namespace() -> String {
    "flux-system".to_string()
}
fn default_flux_git_repository() -> String {
    "flux-system".to_string()
}
fn default_flux_kustomization() -> String {
    "apps".to_string()
}
fn default_kubeconfig_globs() -> Vec<String> {
    vec!["~/.kube/*.yaml".to_string(), "~/.kube/*.yml".to_string()]
}
fn default_vault_namespace() -> String {
    "vault".to_string()
}
fn default_vault_label_selector() -> String {
    "app.kubernetes.io/name=vault,component=server".to_string()
}
fn default_k8s_interval() -> u64 {
    10
}
fn default_github_interval() -> u64 {
    120
}
fn default_github_issues_interval() -> u64 {
    300
}
fn default_github_releases_interval() -> u64 {
    900
}
fn default_gitops_interval() -> u64 {
    120
}
fn default_honeycomb_interval() -> u64 {
    60
}
fn default_registry_interval() -> u64 {
    60
}
fn default_vault_interval() -> u64 {
    30
}
fn default_vault_sealed_interval() -> u64 {
    5
}
impl Default for UrlConfig {
    fn default() -> Self {
        Self {
            service_pattern: default_service_pattern(),
            vault_pattern: default_vault_pattern(),
        }
    }
}

fn default_service_pattern() -> String {
    "https://{service}.{env}.example.com".to_string()
}
pub fn default_vault_pattern() -> String {
    "https://vault.{env}.example.com:8200".to_string()
}
/// Empty: no log backend configured, the log viewer's external link is hidden.
fn default_openobserve_url() -> String {
    String::new()
}
fn default_openobserve_org() -> String {
    "default".to_string()
}

/// Resolve a secret using three-tier resolution: cmd > env > literal
pub fn resolve_secret(
    cmd: &Option<String>,
    env_var: &Option<String>,
    literal: &Option<String>,
) -> Result<Option<String>> {
    if let Some(cmd) = cmd {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .with_context(|| format!("Failed to run secret command: {}", cmd))?;
        if output.status.success() {
            let val = String::from_utf8(output.stdout)?.trim().to_string();
            if !val.is_empty() {
                return Ok(Some(val));
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("Secret command failed: {} — {}", cmd, stderr);
        }
    }
    if let Some(env_name) = env_var {
        if let Ok(val) = std::env::var(env_name) {
            if !val.is_empty() {
                return Ok(Some(val));
            }
        }
    }
    if let Some(val) = literal {
        if !val.is_empty() {
            return Ok(Some(val.clone()));
        }
    }
    Ok(None)
}

impl Config {
    /// Resolve GitHub token with priority: keyring → token_cmd → token_env → token literal
    pub fn resolve_github_token(&self) -> Result<Option<String>> {
        // 1. Try keyring first
        match crate::auth::keyring::retrieve_token() {
            Ok(Some(token)) => {
                tracing::info!("Resolved GitHub token from keyring");
                return Ok(Some(token));
            }
            Ok(None) => {
                tracing::info!("No token found in keyring");
            }
            Err(e) => tracing::warn!("Keyring lookup failed: {}", e),
        }

        // 2. Fall back to config-based resolution: cmd → env → literal
        resolve_secret(
            &self.github.token_cmd,
            &self.github.token_env,
            &self.github.token,
        )
    }

    /// Expand ~ in a kubeconfig path
    pub fn expand_path(path: &str) -> PathBuf {
        let expanded = shellexpand::tilde(path);
        PathBuf::from(expanded.as_ref())
    }

    /// Fill values that derive from other keys when left empty.
    pub fn finalize(mut self) -> Self {
        if self.github.app_gitops_remote.trim().is_empty() {
            self.github.app_gitops_remote = format!(
                "git@github.com:{}/{}.git",
                self.github.org, self.github.gitops_repo
            );
        }
        self
    }
}

static INSTALLED: std::sync::OnceLock<std::sync::RwLock<Option<std::sync::Arc<Config>>>> =
    std::sync::OnceLock::new();

/// Make `config` the process-wide config for code paths that have no config
/// in hand (Flux object names, vault namespaces, kubeconfig discovery).
pub fn install(config: &Config) {
    let slot = INSTALLED.get_or_init(|| std::sync::RwLock::new(None));
    if let Ok(mut w) = slot.write() {
        *w = Some(std::sync::Arc::new(config.clone()));
    }
}

/// The installed config, if `install` ran.
pub fn installed() -> Option<std::sync::Arc<Config>> {
    INSTALLED
        .get()
        .and_then(|slot| slot.read().ok().and_then(|r| r.clone()))
}

/// The installed Flux object names, or the public defaults.
pub fn flux() -> FluxConfig {
    installed().map(|c| c.flux.clone()).unwrap_or_default()
}

/// The installed gitops repo name, or the default.
pub fn gitops_repo() -> String {
    installed()
        .map(|c| c.github.gitops_repo.clone())
        .unwrap_or_else(default_gitops_repo)
}

/// The installed release PR title prefix, or the default.
pub fn release_pr_prefix() -> String {
    installed()
        .map(|c| c.github.release_pr_prefix.clone())
        .unwrap_or_else(default_release_pr_prefix)
}

/// The installed action repo prefix, or the default.
pub fn action_repo_prefix() -> String {
    installed()
        .map(|c| c.github.action_repo_prefix.clone())
        .unwrap_or_else(default_action_repo_prefix)
}

/// The installed vault config for `env`, or the defaults.
pub fn vault_for(env: &str) -> VaultConfig {
    installed()
        .and_then(|c| c.vault.get(env).cloned())
        .unwrap_or_else(|| VaultConfig {
            url: String::new(),
            kubeconfig: None,
            namespace: default_vault_namespace(),
            label_selector: default_vault_label_selector(),
            unseal_keys_cmd: None,
            remember_keys: false,
            auto_unseal: false,
        })
}

/// The installed kubeconfig globs, or the defaults.
pub fn kubeconfig_globs() -> Vec<String> {
    installed()
        .map(|c| c.kubeconfigs.globs.clone())
        .unwrap_or_else(default_kubeconfig_globs)
}

pub fn config_dir() -> PathBuf {
    crate::branding::project_dirs()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home)
                .join(".config")
                .join(crate::branding::APP_NAME)
        })
}

/// Per-user data directory: the gitops mirror, the instance lock and the
/// keyring fallback token live here.
pub fn data_dir() -> PathBuf {
    crate::branding::project_dirs()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join(crate::branding::APP_NAME)
        })
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Check if the config file exists on disk (regardless of whether it parses).
pub fn config_file_exists() -> bool {
    config_path().exists()
}

/// Load and parse the config file.
pub fn load_config() -> Result<Config> {
    let path = config_path();

    if !path.exists() {
        anyhow::bail!("No config file found at {:?}", path);
    }

    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("Failed to read {:?}", path))?;

    let config: Config =
        toml::from_str(&contents).with_context(|| format!("Failed to parse {:?}", path))?;
    config
        .environments
        .validate()
        .map_err(|e| anyhow::anyhow!("{:?}: {e}", path))?;

    Ok(config.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_secret_literal() {
        let result = resolve_secret(&None, &None, &Some("my-token".to_string())).unwrap();
        assert_eq!(result, Some("my-token".to_string()));
    }

    #[test]
    fn resolve_secret_empty_literal() {
        let result = resolve_secret(&None, &None, &Some("".to_string())).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn resolve_secret_none() {
        let result = resolve_secret(&None, &None, &None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn resolve_secret_env_var() {
        std::env::set_var("CARAVAN_TEST_TOKEN_123", "env-token");
        let result =
            resolve_secret(&None, &Some("CARAVAN_TEST_TOKEN_123".to_string()), &None).unwrap();
        assert_eq!(result, Some("env-token".to_string()));
        std::env::remove_var("CARAVAN_TEST_TOKEN_123");
    }

    #[test]
    fn resolve_secret_cmd() {
        let result = resolve_secret(&Some("echo cmd-token".to_string()), &None, &None).unwrap();
        assert_eq!(result, Some("cmd-token".to_string()));
    }

    #[test]
    fn resolve_secret_priority_cmd_over_env() {
        std::env::set_var("CARAVAN_TEST_PRIORITY", "env-val");
        let result = resolve_secret(
            &Some("echo cmd-val".to_string()),
            &Some("CARAVAN_TEST_PRIORITY".to_string()),
            &Some("literal-val".to_string()),
        )
        .unwrap();
        assert_eq!(result, Some("cmd-val".to_string()));
        std::env::remove_var("CARAVAN_TEST_PRIORITY");
    }

    #[test]
    fn expand_path_tilde() {
        let result = Config::expand_path("~/test.yaml");
        assert!(!result.to_string_lossy().contains('~'));
        assert!(result.to_string_lossy().ends_with("/test.yaml"));
    }

    #[test]
    fn parse_minimal_config() {
        let toml_str = r#"
[github]
org = "test-org"
token = "test-token"

[[environments]]
id = "dev"
kubeconfig = "~/kubeconfig.yaml"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.github.org, "test-org");
        assert_eq!(config.github.token, Some("test-token".to_string()));
        assert!(config.environments.contains("dev"));
        assert_eq!(config.polling.gitops_interval_secs, 120);
        assert!(config.notifications.enabled);
        assert!(config.notifications.failures.is_empty());
        assert!(config.promote.sync_after_push);
        assert_eq!(config.promote.pair(&config.environments), None);
        assert!(config.repos.hide.is_empty());
    }

    #[test]
    fn missing_environments_fall_back_to_the_public_ladder() {
        let config: Config = toml::from_str("[github]\norg = \"notes-co\"\n").unwrap();
        assert_eq!(config.environments.order(), ["prod", "staging", "dev"]);
        assert_eq!(
            config.promote.pair(&config.environments),
            Some(("staging".to_string(), "dev".to_string()))
        );
        assert!(config
            .notifications
            .failures_enabled("prod", &config.environments));
        assert!(!config
            .notifications
            .failures_enabled("dev", &config.environments));
    }

    #[test]
    fn ladder_with_explicit_promote_pair() {
        let config: Config = toml::from_str(
            r#"
[github]
org = "notes-co"

[[environments]]
id = "prod"
reference = true

[[environments]]
id = "staging"
upstream = "prod"

[[environments]]
id = "dev"
upstream = "staging"

[promote]
target = "staging"

[notifications.failures]
dev = true
staging = false
"#,
        )
        .unwrap();
        assert_eq!(
            config.promote.pair(&config.environments),
            Some(("prod".to_string(), "staging".to_string()))
        );
        assert!(config
            .notifications
            .failures_enabled("dev", &config.environments));
        assert!(!config
            .notifications
            .failures_enabled("staging", &config.environments));
    }

    #[test]
    fn promote_disabled_has_no_pair_and_keeps_the_ladder() {
        let config: Config = toml::from_str(
            r#"
[github]
org = "notes-co"

[[environments]]
id = "prod"
reference = true

[[environments]]
id = "staging"
upstream = "prod"

[promote]
enabled = false
"#,
        )
        .unwrap();
        assert!(!config.promote.enabled);
        assert_eq!(config.promote.pair(&config.environments), None);
        assert_eq!(config.environments.upstream_of("staging"), Some("prod"));
        assert_eq!(config.environments.release_source(), Some("staging"));
    }

    #[test]
    fn promote_disabled_ignores_an_explicit_pair() {
        let config: Config = toml::from_str(
            "[github]\norg = \"notes-co\"\n\n[promote]\nenabled = false\ntarget = \"dev\"\nsource = \"staging\"\n",
        )
        .unwrap();
        assert_eq!(config.promote.pair(&config.environments), None);
    }

    #[test]
    fn promote_enabled_by_default() {
        let config: Config = toml::from_str("[github]\norg = \"notes-co\"\n").unwrap();
        assert!(config.promote.enabled);
        assert!(PromoteConfig::default().enabled);
    }

    #[test]
    fn parse_config_with_context() {
        let toml_str = r#"
[github]
org = "test-org"

[environments.prod]
kubeconfig = "~/.kube/config"
context = "prod-apps"
label = "Production"

[environments.staging]
kubeconfig = "~/.kube/staging"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let env = config.environments.get("prod").unwrap();
        assert_eq!(env.context, Some("prod-apps".to_string()));
        // legacy tables take the public ladder's order and links
        assert_eq!(config.environments.order(), ["prod", "staging"]);
        assert!(env.reference);
        assert_eq!(config.environments.upstream_of("staging"), Some("prod"));
    }

    #[test]
    fn old_config_keys_still_load() {
        let toml_str = r#"
[github]
org = "test-org"
manifest_repo = "app-gitops"
manifest_path = "caravan.toml"
app_gitops_dir = "~/dev/app-gitops"

[environments.prod]
kubeconfig = "~/.kube/config"
app_gitops_dir = "~/dev/app-gitops"

[vault.prod]
url = "https://vault.example:8200"
unseal_keys_cmd = "pass show vault/prod"
auto_unseal = true

[repos]
hide = ["hello-k8s"]
[repos.service_names]
"pqc-rogues-gallery" = "rogues"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.repos.hide, vec!["hello-k8s".to_string()]);
        assert_eq!(
            config
                .repos
                .service_names
                .get("pqc-rogues-gallery")
                .map(String::as_str),
            Some("rogues")
        );
        let vault = config.vault.get("prod").unwrap();
        assert_eq!(
            vault.unseal_keys_cmd.as_deref(),
            Some("pass show vault/prod")
        );
        assert!(vault.auto_unseal);
        assert!(!vault.remember_keys);
    }

    #[test]
    fn public_defaults_and_derived_remote() {
        let toml_str = r#"
[github]
org = "notes-co"
"#;
        let config = toml::from_str::<Config>(toml_str).unwrap().finalize();
        assert_eq!(config.github.gitops_repo, "gitops");
        assert_eq!(
            config.github.app_gitops_remote,
            "git@github.com:notes-co/gitops.git"
        );
        assert_eq!(config.github.oauth_client_id, None);
        assert_eq!(config.github.action_repo_prefix, "action-");
        assert_eq!(config.github.release_pr_prefix, "Release ");
        assert_eq!(config.flux.namespace, "flux-system");
        assert_eq!(config.flux.git_repository, "flux-system");
        assert_eq!(config.flux.kustomization, "apps");
        assert_eq!(
            config.urls.service_pattern,
            "https://{service}.{env}.example.com"
        );
        assert_eq!(config.openobserve.url, "");
        assert_eq!(config.update.repo, None);
        assert_eq!(
            config.kubeconfigs.globs,
            vec!["~/.kube/*.yaml", "~/.kube/*.yml"]
        );
    }

    #[test]
    fn old_remote_key_and_explicit_remote_win() {
        let toml_str = r#"
[github]
org = "notes-co"
app_gitops_remote = "git@example.com:notes-co/deploy.git"
oauth_client_id = "Iv1.example"

[flux]
namespace = "platform-system"

[vault.prod]
url = "https://vault.example:8200"
namespace = "secrets"
"#;
        let config = toml::from_str::<Config>(toml_str).unwrap().finalize();
        assert_eq!(
            config.github.app_gitops_remote,
            "git@example.com:notes-co/deploy.git"
        );
        assert_eq!(
            config.github.oauth_client_id.as_deref(),
            Some("Iv1.example")
        );
        assert_eq!(config.flux.namespace, "platform-system");
        assert_eq!(config.flux.kustomization, "apps");
        let vault = config.vault.get("prod").unwrap();
        assert_eq!(vault.namespace, "secrets");
        assert_eq!(
            vault.label_selector,
            "app.kubernetes.io/name=vault,component=server"
        );
    }
}
