use crate::actions::unseal_keys::KeySource;
use crate::actions::vault::{PodProgress, VaultUnsealRun};
use crate::cache::{OrgRepo, RepoKind};
use crate::instance::RunMode;
use crate::models::deployment::{DeploymentStatus, HelmReleaseInfo, PodInfo};
use crate::models::image::{digests_equal, tag_kind, ImageRef, TagKind};
use crate::models::issue::GitHubIssue;
use crate::models::release::{GitHubRelease, PullRequest, WorkflowRun};
use crate::pollers::vault::VaultState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// Declared version of a service in a given environment, as read from app-gitops patch files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclaredVersion {
    /// The Helm chart version (from spec.chart.spec.version)
    pub chart_version: Option<String>,
    /// The container image tag (from spec.values.image.tag)
    pub image_tag: Option<String>,
    /// Every image pinned anywhere under spec.values
    #[serde(default)]
    pub images: Vec<DeclaredImage>,
}

/// One image pin inside a HelmRelease patch: any mapping under spec.values
/// with a `tag` key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclaredImage {
    /// Dotted YAML path of the mapping, e.g. `values.worker.image`
    pub path: String,
    /// The tag exactly as written (may carry an `@sha256:` suffix)
    pub tag: String,
    /// Sibling `digest` key when present
    pub digest: Option<String>,
    /// Sibling `repository` key when present
    pub repository: Option<String>,
}

#[allow(dead_code)]
impl DeclaredImage {
    /// The tag without an embedded `@digest` suffix.
    pub fn bare_tag(&self) -> &str {
        self.tag.split('@').next().unwrap_or(&self.tag)
    }

    /// The digest, from the sibling key or embedded in the tag.
    pub fn effective_digest(&self) -> Option<&str> {
        self.digest
            .as_deref()
            .or_else(|| self.tag.split_once('@').map(|(_, d)| d))
    }
}

/// Image pins for the project components, from the operator ConfigMap in
/// app-gitops (base install.yml overlaid with the env config-patch).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorPins {
    pub chart_name: Option<String>,
    pub chart_version: Option<String>,
    /// component name -> pinned image
    pub components: BTreeMap<String, ImageRef>,
}

/// HEAD of app-gitops main as last seen in the local mirror.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitopsHead {
    pub sha: String,
    pub fetched_at: DateTime<Utc>,
}

/// The Flux Kustomization `apps` of one environment.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct KustomizationInfo {
    pub last_applied_revision: Option<String>,
    pub last_attempted_revision: Option<String>,
    /// Commit sha parsed from `main@sha1:<sha>`
    pub applied_sha: Option<String>,
    pub ready: Option<bool>,
    pub message: Option<String>,
}

/// The central application state shared between the UI thread and background pollers.
/// UI reads, pollers write.
#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<RwLock<AppStateInner>>,
    /// Notify to wake the GitHub poller immediately on refresh
    pub github_refresh: Arc<tokio::sync::Notify>,
    /// Notify to wake the GitOps poller immediately on refresh
    pub gitops_refresh: Arc<tokio::sync::Notify>,
}

#[derive(Debug, Default)]
pub struct AppStateInner {
    /// Per-environment K8s data
    pub environments: HashMap<String, EnvironmentState>,
    /// Last poll times
    pub last_k8s_poll: Option<DateTime<Utc>>,
    pub last_github_poll: Option<DateTime<Utc>>,
    /// GitHub Actions workflow runs across all tracked repos
    pub workflow_runs: Vec<WorkflowRun>,
    /// Open PRs across all tracked repos (services + libraries)
    pub pull_requests: Vec<PullRequest>,
    /// Recent GitHub Releases across all tracked repos
    pub releases: Vec<GitHubRelease>,
    /// Recent git tags per repo: repo -> [(tag, commit sha)], newest first.
    /// Lets version labels resolve tags that have no GitHub Release.
    pub repo_tags: HashMap<String, Vec<(String, String)>>,
    /// Open PRs on app-gitops (release PRs for prod deployments)
    pub open_gitops_prs: Vec<PullRequest>,
    /// GitOps declared versions: service -> env -> DeclaredVersion
    pub declared_versions: HashMap<String, HashMap<String, DeclaredVersion>>,
    /// Last gitops poll time
    pub last_gitops_poll: Option<DateTime<Utc>>,
    /// Vault state per environment
    pub vault_states: HashMap<String, VaultState>,
    /// Unseal runs, toast-able vault events and in-memory remember-keys flags
    pub vault_ops: VaultOps,
    /// Errors from pollers
    pub poller_errors: Vec<PollerError>,
    /// Commit comparison: (repo, base_env, head_env) -> ahead_by count
    /// e.g. ('notes-api', 'prod', 'staging') -> 5 means staging is 5 commits ahead of prod
    pub commit_comparisons: HashMap<(String, String, String), i64>,
    /// Commit relation between two environments' deployed commits:
    /// (service, base_env, head_env) -> how far head is ahead of and behind base.
    pub commit_relations: HashMap<(String, String, String), CommitRelation>,
    /// (repo, PR number) -> what its pr-N image tag was built from
    pub pr_builds: HashMap<(String, u64), PrBuild>,
    /// All repos discovered from the GitHub org
    pub org_repos: Vec<OrgRepo>,
    /// Repos the user has hidden (persisted in SQLite)
    pub hidden_repos: HashSet<String>,
    /// GitHub Issues across tracked repos
    pub issues: Vec<GitHubIssue>,
    /// Whether the GitHub PAT has the `read:project` scope (None = not checked yet)
    pub has_project_scope: Option<bool>,
    /// Latest available version of Caravan (from GitHub releases)
    pub latest_version: Option<LatestVersion>,
    /// Manual refresh requested from UI
    pub refresh_requested: bool,
    /// Flux sync-with-source operations in flight or recently finished, per env
    pub flux_syncs: HashMap<String, FluxSyncState>,
    /// GitHub core API rate limit, from response headers / the rate_limit endpoint
    pub gh_rate_core: Option<RateLimitInfo>,
    /// GitHub search API rate limit
    pub gh_rate_search: Option<RateLimitInfo>,
    /// Operator component pins per environment
    pub operator_pins: HashMap<String, OperatorPins>,
    /// Service directories under apps/base in app-gitops
    pub gitops_services: Vec<String>,
    /// HEAD of app-gitops main in the local mirror
    pub gitops_head: Option<GitopsHead>,
    /// Commits on main that each env's applied kustomization revision lacks
    pub applied_distance: HashMap<String, u64>,
    /// Active (polls GitHub) or passive (another instance owns the budget)
    pub run_mode: RunMode,
    /// The view currently on screen, for lazy pollers
    pub active_view: String,
    /// When each lazily polled view was last drawn, so reopening it can wake
    /// the poller
    pub view_shown_at: HashMap<String, std::time::Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CommitRelation {
    pub ahead: i64,
    pub behind: i64,
}

/// What a `pr-N` image tag points at, from the GitHub pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrBuild {
    pub head_sha: String,
    pub open: bool,
    pub merged: bool,
    /// The commit on main once the PR merged; comparisons use it instead of
    /// the branch head so a merged build reads like the main commit it became.
    #[serde(default)]
    pub merge_sha: Option<String>,
}

impl PrBuild {
    pub fn compare_sha(&self) -> &str {
        match (&self.merged, &self.merge_sha) {
            (true, Some(m)) => m,
            _ => &self.head_sha,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedEnv {
    pub env: String,
    pub sealed_pods: usize,
    pub total_pods: usize,
    pub reachable: bool,
}

const VAULT_EVENT_CAP: usize = 20;

/// Vault unseal bookkeeping shared by the banner, the Vault page and the
/// auto-unseal poller.
#[derive(Debug, Default)]
pub struct VaultOps {
    /// Latest run per env; stays after completion until dismissed
    pub runs: HashMap<String, VaultUnsealRun>,
    /// Toast-able messages, drained by `take_vault_events`
    pub events: Vec<String>,
    /// In-memory `remember_keys` overrides per env
    pub remember_keys: HashMap<String, bool>,
}

/// A GitHub API rate-limit bucket snapshot.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitInfo {
    pub remaining: u64,
    pub limit: u64,
    pub reset: Option<DateTime<Utc>>,
}

/// Progress of a Flux sync-with-source operation for one environment.
#[derive(Debug, Clone)]
pub enum FluxSyncState {
    Running {
        phase: String,
        started: DateTime<Utc>,
    },
    Done {
        revision: String,
        finished: DateTime<Utc>,
    },
    Failed {
        error: String,
        finished: DateTime<Utc>,
    },
}

#[derive(Debug, Default, Clone)]
pub struct EnvironmentState {
    /// All HelmReleases in this environment
    pub helm_releases: Vec<HelmReleaseInfo>,
    /// All pods in tracked namespaces
    pub pods: Vec<PodInfo>,
    /// Whether we're currently connected to this cluster
    pub connected: bool,
    /// Last successful poll
    pub last_poll: Option<DateTime<Utc>>,
    /// Connection error if any
    pub error: Option<String>,
    /// The configured Flux Kustomization, when readable
    pub kustomization: Option<KustomizationInfo>,
}

impl EnvironmentState {
    /// Get all fleet instances
    pub fn fleet_instances(&self) -> Vec<&HelmReleaseInfo> {
        self.helm_releases
            .iter()
            .filter(|hr| hr.is_fleet_instance())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct LatestVersion {
    /// Version string (e.g. "0.2.0")
    pub version: String,
    /// URL to the release page
    pub html_url: String,
    /// Direct download URL for the current platform's binary (GitHub API asset URL)
    pub download_url: Option<String>,
    /// Original asset filename (e.g. "caravan-linux-x86_64.tar.gz") for format detection
    pub asset_name: Option<String>,
    /// Direct download URL for the SHA256 checksum file
    pub checksum_url: Option<String>,
    /// Release notes (markdown body from GitHub)
    pub release_notes: Option<String>,
    /// When this was published
    #[allow(dead_code)]
    pub published_at: Option<DateTime<Utc>>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PollerError {
    pub source: String,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(AppStateInner::default())),
            github_refresh: Arc::new(tokio::sync::Notify::new()),
            gitops_refresh: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Read access for the UI thread.
    ///
    /// WARNING: never call another AppState method (read or write) from inside
    /// the closure. std RwLock is writer-preferring on Linux, so a nested read
    /// deadlocks permanently once a poller's queued write lands between the
    /// outer and inner acquisition. This froze the UI overnight before.
    pub fn read<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&AppStateInner) -> R,
    {
        let guard = self.inner.read().unwrap();
        f(&guard)
    }

    /// Write access for pollers
    pub fn write<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut AppStateInner) -> R,
    {
        let mut guard = self.inner.write().unwrap();
        f(&mut guard)
    }

    /// Update environment state from a K8s poll
    pub fn update_environment(
        &self,
        env_name: &str,
        helm_releases: Vec<HelmReleaseInfo>,
        pods: Vec<PodInfo>,
        kustomization: Option<KustomizationInfo>,
    ) {
        self.write(|state| {
            let env = state.environments.entry(env_name.to_string()).or_default();
            env.helm_releases = helm_releases;
            env.pods = pods;
            if kustomization.is_some() {
                env.kustomization = kustomization;
            }
            env.connected = true;
            env.last_poll = Some(Utc::now());
            env.error = None;
            state.last_k8s_poll = Some(Utc::now());
        });
    }

    /// Mark an environment as having an error
    pub fn set_environment_error(&self, env_name: &str, error: String) {
        self.write(|state| {
            let env = state.environments.entry(env_name.to_string()).or_default();
            env.connected = false;
            env.error = Some(error.clone());
            state.poller_errors.push(PollerError {
                source: format!("k8s/{}", env_name),
                message: error,
                timestamp: Utc::now(),
            });
            // Keep only last 50 errors
            if state.poller_errors.len() > 50 {
                state.poller_errors.drain(0..state.poller_errors.len() - 50);
            }
        });
    }

    /// Get a snapshot of the environment state for rendering
    pub fn get_environment(&self, env_name: &str) -> Option<EnvironmentState> {
        self.read(|state| state.environments.get(env_name).cloned())
    }

    /// Update GitHub state from a poll cycle
    pub fn update_github(
        &self,
        workflow_runs: Vec<WorkflowRun>,
        pull_requests: Vec<PullRequest>,
        releases: Vec<GitHubRelease>,
        open_gitops_prs: Vec<PullRequest>,
    ) {
        self.write(|state| {
            state.workflow_runs = workflow_runs;
            state.pull_requests = pull_requests;
            state.releases = releases;
            state.open_gitops_prs = open_gitops_prs;
            state.last_github_poll = Some(Utc::now());
        });
    }

    /// Record a rate-limit snapshot for the given API resource bucket.
    pub fn set_rate_limit(&self, resource: &str, info: RateLimitInfo) {
        self.write(|state| match resource {
            "core" => state.gh_rate_core = Some(info),
            "search" => state.gh_rate_search = Some(info),
            _ => {}
        });
    }

    /// Update issues from a poll cycle
    pub fn update_issues(&self, issues: Vec<GitHubIssue>) {
        self.write(|state| {
            state.issues = issues;
        });
    }

    /// Get all issues
    pub fn get_issues(&self) -> Vec<GitHubIssue> {
        self.read(|state| state.issues.clone())
    }

    /// Get issues for a specific repo, plus any internal-docs issues that reference it
    #[allow(dead_code)]
    pub fn get_issues_for_repo(&self, repo: &str) -> Vec<GitHubIssue> {
        self.read(|state| {
            state
                .issues
                .iter()
                .filter(|i| {
                    i.repo == repo
                        || (i.repo == "internal-docs" && i.matched_repos.iter().any(|r| r == repo))
                })
                .cloned()
                .collect()
        })
    }

    /// Get issues grouped by assignee. Unassigned issues are under the "" key.
    #[allow(dead_code)]
    pub fn get_issues_by_assignee(&self) -> HashMap<String, Vec<GitHubIssue>> {
        self.read(|state| {
            let mut map: HashMap<String, Vec<GitHubIssue>> = HashMap::new();
            for issue in &state.issues {
                if issue.assignees.is_empty() {
                    map.entry(String::new()).or_default().push(issue.clone());
                } else {
                    for assignee in &issue.assignees {
                        map.entry(assignee.clone()).or_default().push(issue.clone());
                    }
                }
            }
            map
        })
    }

    /// Get all epics
    #[allow(dead_code)]
    pub fn get_epics(&self) -> Vec<GitHubIssue> {
        self.read(|state| state.issues.iter().filter(|i| i.is_epic).cloned().collect())
    }

    /// Set whether the PAT has read:project scope
    pub fn set_has_project_scope(&self, has_scope: bool) {
        self.write(|state| {
            state.has_project_scope = Some(has_scope);
        });
    }

    /// Check if PAT has read:project scope (None = not checked yet)
    pub fn has_project_scope(&self) -> Option<bool> {
        self.read(|state| state.has_project_scope)
    }

    /// Request a manual refresh of GitHub + GitOps data.
    /// Wakes the pollers immediately instead of waiting for next cycle.
    pub fn request_refresh(&self) {
        self.write(|state| state.refresh_requested = true);
        self.github_refresh.notify_one();
        self.gitops_refresh.notify_one();
    }

    /// Wake the GitHub poller so a view's lazy tier runs now. Unlike
    /// `request_refresh` this forces nothing: the poller reads `active_view`
    /// and runs only what that view needs.
    pub fn request_view_refresh(&self) {
        self.github_refresh.notify_one();
    }

    /// Check and clear the refresh flag (called by poller)
    pub fn take_refresh_requested(&self) -> bool {
        self.write(|state| {
            let was = state.refresh_requested;
            state.refresh_requested = false;
            was
        })
    }

    /// Merge project fields into existing issues
    pub fn merge_project_fields(
        &self,
        fields: HashMap<(String, u64), (Option<String>, Option<String>)>,
    ) {
        self.write(|state| {
            for issue in &mut state.issues {
                if let Some((status, priority)) = fields.get(&(issue.repo.clone(), issue.number)) {
                    issue.project_status = status.clone();
                    issue.project_priority = priority.clone();
                    issue.on_project = true;
                }
            }
        });
    }

    /// Add a poller error
    pub fn add_poller_error(&self, source: &str, message: &str) {
        self.write(|state| {
            state.poller_errors.push(PollerError {
                source: source.to_string(),
                message: message.to_string(),
                timestamp: Utc::now(),
            });
            if state.poller_errors.len() > 50 {
                state.poller_errors.drain(0..state.poller_errors.len() - 50);
            }
        });
    }

    /// Get all workflow runs
    pub fn get_workflow_runs(&self) -> Vec<WorkflowRun> {
        self.read(|state| state.workflow_runs.clone())
    }

    /// Get workflow runs for a specific repo
    pub fn get_workflow_runs_for_repo(&self, repo: &str) -> Vec<WorkflowRun> {
        self.read(|state| {
            state
                .workflow_runs
                .iter()
                .filter(|r| r.repo == repo)
                .cloned()
                .collect()
        })
    }

    /// Get in-progress workflow runs across all repos
    pub fn get_in_progress_runs(&self) -> Vec<WorkflowRun> {
        self.read(|state| {
            state
                .workflow_runs
                .iter()
                .filter(|r| r.is_in_progress())
                .cloned()
                .collect()
        })
    }

    /// Get recently failed workflow runs (last 24h)
    #[allow(dead_code)]
    pub fn get_failed_runs(&self) -> Vec<WorkflowRun> {
        let cutoff = Utc::now() - chrono::Duration::hours(24);
        self.read(|state| {
            state
                .workflow_runs
                .iter()
                .filter(|r| r.is_failed() && r.updated_at > cutoff)
                .cloned()
                .collect()
        })
    }

    /// Get all open PRs
    pub fn get_pull_requests(&self) -> Vec<PullRequest> {
        self.read(|state| state.pull_requests.clone())
    }

    /// Get open PRs for a specific repo
    #[allow(dead_code)]
    pub fn get_pull_requests_for_repo(&self, repo: &str) -> Vec<PullRequest> {
        self.read(|state| {
            state
                .pull_requests
                .iter()
                .filter(|pr| pr.repo == repo)
                .cloned()
                .collect()
        })
    }

    /// Get open gitops PRs (release PRs for prod)
    pub fn get_open_gitops_prs(&self) -> Vec<PullRequest> {
        self.read(|state| state.open_gitops_prs.clone())
    }

    /// Get all releases
    pub fn get_releases(&self) -> Vec<GitHubRelease> {
        self.read(|state| state.releases.clone())
    }

    /// Update the per-repo tag→SHA maps from a poll cycle
    pub fn update_repo_tags(&self, tags: HashMap<String, Vec<(String, String)>>) {
        self.write(|state| {
            state.repo_tags = tags;
        });
    }

    /// Find the release tag for a given commit SHA in a repo.
    /// Used to display "v1.0.2" next to the running SHA.
    #[allow(dead_code)]
    pub fn find_release_for_sha(&self, repo: &str, sha: &str) -> Option<String> {
        let clean_sha = sha.strip_prefix("sha-").unwrap_or(sha);
        self.read(|state| {
            state
                .releases
                .iter()
                .find(|r| {
                    r.repo == repo
                        && (r.target_commitish.starts_with(clean_sha)
                            || clean_sha.starts_with(&r.target_commitish))
                })
                .map(|r| r.tag.clone())
        })
    }

    /// Find release tag for a service, trying both SHA match and chart version match.
    /// For prod: chart_version '0.8.2' matches release tag 'v0.8.2'
    /// For non-reference environments: full image SHA matches release target_commitish
    pub fn find_release_for_service(
        &self,
        repo: &str,
        full_sha: &str,
        chart_version: Option<&str>,
    ) -> Option<String> {
        let clean_sha = full_sha.strip_prefix("sha-").unwrap_or(full_sha);
        self.read(|state| {
            // Try SHA match first (works when tags have resolved SHAs)
            if !clean_sha.is_empty() {
                if let Some(r) = state.releases.iter().find(|r| {
                    r.repo == repo
                        && !r.target_commitish.is_empty()
                        && (r.target_commitish.starts_with(clean_sha)
                            || clean_sha.starts_with(&r.target_commitish))
                }) {
                    return Some(r.tag.clone());
                }
                // Fall back to bare git tags — a tag may exist without a Release
                if let Some(tags) = state.repo_tags.get(repo) {
                    if let Some((tag, _)) = tags.iter().find(|(_, sha)| {
                        sha.starts_with(clean_sha) || clean_sha.starts_with(sha.as_str())
                    }) {
                        return Some(tag.clone());
                    }
                }
            }
            // Try chart version match (works for prod where chart_version is semver)
            // Chart versions like "0.1.0-<sha>" won't match, but "0.8.2" will match tag "v0.8.2"
            if let Some(cv) = chart_version {
                // Skip SHA-style chart versions like "0.1.0-abc123def..."
                let is_sha_version = cv.contains('-')
                    && cv
                        .split('-')
                        .next_back()
                        .is_some_and(|s| s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()));
                if !is_sha_version {
                    if let Some(r) = state.releases.iter().find(|r| {
                        r.repo == repo
                            && (r.tag == format!("v{}", cv)
                                || r.tag == *cv
                                || r.tag.strip_prefix('v') == Some(cv))
                    }) {
                        return Some(r.tag.clone());
                    }
                    if let Some(tags) = state.repo_tags.get(repo) {
                        if let Some((tag, _)) = tags.iter().find(|(t, _)| {
                            *t == format!("v{}", cv) || t == cv || t.strip_prefix('v') == Some(cv)
                        }) {
                            return Some(tag.clone());
                        }
                    }
                }
            }
            None
        })
    }

    /// Get releases for a specific repo
    #[allow(dead_code)]
    pub fn get_releases_for_repo(&self, repo: &str) -> Vec<GitHubRelease> {
        self.read(|state| {
            state
                .releases
                .iter()
                .filter(|r| r.repo == repo)
                .cloned()
                .collect()
        })
    }

    /// Update declared versions from a GitOps poll cycle
    #[allow(dead_code)]
    pub fn update_declared_versions(
        &self,
        versions: HashMap<String, HashMap<String, DeclaredVersion>>,
    ) {
        self.write(|state| {
            state.declared_versions = versions;
        });
    }

    /// Get the declared version for a specific service in a specific environment
    #[allow(dead_code)]
    pub fn get_declared_version(&self, service: &str, env: &str) -> Option<DeclaredVersion> {
        self.read(|state| {
            state
                .declared_versions
                .get(service)
                .and_then(|envs| envs.get(env))
                .cloned()
        })
    }

    /// Get all declared versions (service -> env -> version)
    #[allow(dead_code)]
    pub fn get_declared_versions(&self) -> HashMap<String, HashMap<String, DeclaredVersion>> {
        self.read(|state| state.declared_versions.clone())
    }

    /// Store one gitops poll cycle's results.
    pub fn update_gitops(
        &self,
        services: Vec<String>,
        declared_versions: HashMap<String, HashMap<String, DeclaredVersion>>,
        operator_pins: HashMap<String, OperatorPins>,
        head: Option<GitopsHead>,
    ) {
        self.write(|state| {
            state.gitops_services = services;
            state.declared_versions = declared_versions;
            state.operator_pins = operator_pins;
            if head.is_some() {
                state.gitops_head = head;
            }
            state.last_gitops_poll = Some(Utc::now());
        });
    }

    #[allow(dead_code)]
    pub fn get_operator_pins(&self) -> HashMap<String, OperatorPins> {
        self.read(|state| state.operator_pins.clone())
    }

    #[allow(dead_code)]
    pub fn get_gitops_services(&self) -> Vec<String> {
        self.read(|state| state.gitops_services.clone())
    }

    #[allow(dead_code)]
    pub fn get_gitops_head(&self) -> Option<GitopsHead> {
        self.read(|state| state.gitops_head.clone())
    }

    pub fn run_mode(&self) -> RunMode {
        self.read(|state| state.run_mode.clone())
    }

    pub fn set_run_mode(&self, mode: RunMode) {
        self.write(|state| state.run_mode = mode);
    }

    /// The view on screen, set every frame by the UI.
    #[allow(dead_code)]
    pub fn active_view(&self) -> String {
        self.read(|state| state.active_view.clone())
    }

    pub fn set_active_view(&self, view: &str) {
        let changed = self.read(|state| state.active_view != view);
        if changed {
            self.write(|state| state.active_view = view.to_string());
        }
    }

    /// Called by a lazily polled view every frame it is drawn. Wakes the
    /// GitHub poller when the view was not on screen a moment ago, so its
    /// tier runs right away instead of at the next cycle.
    pub fn note_view_shown(&self, view: &str) {
        let now = std::time::Instant::now();
        let reopened = self.write(|state| {
            let last = state.view_shown_at.insert(view.to_string(), now);
            last.is_none_or(|t| now.duration_since(t) > std::time::Duration::from_secs(2))
        });
        if reopened {
            self.request_view_refresh();
        }
    }

    /// Operator pins transposed: component -> env -> pinned image.
    pub fn component_pins(&self) -> BTreeMap<String, BTreeMap<String, ImageRef>> {
        self.read(|state| {
            let mut out: BTreeMap<String, BTreeMap<String, ImageRef>> = BTreeMap::new();
            for (env, pins) in &state.operator_pins {
                for (component, image) in &pins.components {
                    out.entry(component.clone())
                        .or_default()
                        .insert(env.clone(), image.clone());
                }
            }
            out
        })
    }

    /// The best-known tag for an image digest across every env's operator pins
    /// and declared images: a version tag beats a sha tag beats a pr tag
    /// beats anything else.
    #[allow(dead_code)]
    pub fn best_name_for_digest(&self, digest: &str) -> Option<String> {
        if digest.is_empty() {
            return None;
        }
        self.read(|state| {
            let mut best: Option<(TagKind, String)> = None;
            let mut offer = |tag: &str| {
                if tag.is_empty() {
                    return;
                }
                let kind = tag_kind(tag);
                let better = match &best {
                    Some((k, _)) => kind.rank() > k.rank(),
                    None => true,
                };
                if better {
                    best = Some((kind, tag.to_string()));
                }
            };
            for pins in state.operator_pins.values() {
                for image in pins.components.values() {
                    if image.has_digest(digest) {
                        if let Some(tag) = &image.tag {
                            offer(tag);
                        }
                    }
                }
            }
            for envs in state.declared_versions.values() {
                for dv in envs.values() {
                    for image in &dv.images {
                        if image
                            .effective_digest()
                            .is_some_and(|d| digests_equal(d, digest))
                        {
                            offer(image.bare_tag());
                        }
                    }
                }
            }
            best.map(|(_, tag)| tag)
        })
    }

    pub fn set_commit_comparisons(&self, comparisons: HashMap<(String, String, String), i64>) {
        self.write(|state| {
            state.commit_comparisons = comparisons;
        });
    }

    pub fn set_commit_relations(
        &self,
        relations: HashMap<(String, String, String), CommitRelation>,
    ) {
        self.write(|state| {
            state.commit_relations = relations;
        });
    }

    /// How far head_env's deployed commit is ahead of and behind base_env's
    /// for a service (or component). None when the pair was never compared.
    pub fn get_relation(
        &self,
        service: &str,
        base_env: &str,
        head_env: &str,
    ) -> Option<CommitRelation> {
        self.read(|state| {
            state
                .commit_relations
                .get(&(
                    service.to_string(),
                    base_env.to_string(),
                    head_env.to_string(),
                ))
                .copied()
        })
    }

    pub fn set_pr_builds(&self, builds: HashMap<(String, u64), PrBuild>) {
        self.write(|state| {
            state.pr_builds = builds;
        });
    }

    pub fn pr_build(&self, repo: &str, number: u64) -> Option<PrBuild> {
        self.read(|state| state.pr_builds.get(&(repo.to_string(), number)).cloned())
    }

    /// Per-environment sealed vault pods, from the kubectl pod status (the
    /// HTTP probe fails while a vault is sealed, so pods are the truth).
    pub fn sealed_summary(&self) -> Vec<SealedEnv> {
        self.read(|state| {
            let mut out: Vec<SealedEnv> = state
                .vault_states
                .iter()
                .map(|(env, v)| SealedEnv {
                    env: env.clone(),
                    sealed_pods: v.pods.iter().filter(|p| p.sealed == Some(true)).count(),
                    total_pods: v.pods.len(),
                    reachable: v.reachable,
                })
                .collect();
            out.sort_by(|a, b| a.env.cmp(&b.env));
            out
        })
    }

    // --- Vault unseal runs and events ---

    /// Register a new unseal run. Refuses while one is still running for the
    /// same env; a finished run is replaced.
    pub fn begin_vault_run(&self, run: VaultUnsealRun) -> bool {
        self.write(|s| {
            if s.vault_ops
                .runs
                .get(&run.env)
                .is_some_and(|r| r.is_running())
            {
                return false;
            }
            s.vault_ops.runs.insert(run.env.clone(), run);
            true
        })
    }

    pub fn vault_run(&self, env: &str) -> Option<VaultUnsealRun> {
        self.read(|s| s.vault_ops.runs.get(env).cloned())
    }

    pub fn set_vault_run_source(&self, env: &str, source: KeySource) {
        self.write(|s| {
            if let Some(r) = s.vault_ops.runs.get_mut(env) {
                r.source = source;
            }
        });
    }

    pub fn update_vault_run_pod(&self, env: &str, progress: PodProgress) {
        self.write(|s| {
            if let Some(r) = s.vault_ops.runs.get_mut(env) {
                match r.pods.iter_mut().find(|p| p.pod == progress.pod) {
                    Some(p) => *p = progress,
                    None => r.pods.push(progress),
                }
            }
        });
    }

    pub fn finish_vault_run(&self, env: &str, error: Option<String>, keys_remembered: bool) {
        self.write(|s| {
            if let Some(r) = s.vault_ops.runs.get_mut(env) {
                r.finished = Some(Utc::now());
                r.error = error;
                r.keys_remembered = keys_remembered;
            }
        });
    }

    /// Drop a finished run (the Dismiss button). A running one stays.
    pub fn dismiss_vault_run(&self, env: &str) {
        self.write(|s| {
            if s.vault_ops.runs.get(env).is_some_and(|r| !r.is_running()) {
                s.vault_ops.runs.remove(env);
            }
        });
    }

    /// Queue a one-line vault event for toasts. Capped so an undrained
    /// queue cannot grow without bound.
    pub fn push_vault_event(&self, message: String) {
        self.write(|s| {
            s.vault_ops.events.push(message);
            let overflow = s.vault_ops.events.len().saturating_sub(VAULT_EVENT_CAP);
            if overflow > 0 {
                s.vault_ops.events.drain(0..overflow);
            }
        });
    }

    /// Drain queued vault events (auto-unseal outcomes and the like) so the
    /// app can toast them.
    #[allow(dead_code)]
    pub fn take_vault_events(&self) -> Vec<String> {
        self.write(|s| std::mem::take(&mut s.vault_ops.events))
    }

    /// In-memory override of `[vault.<env>] remember_keys`, set by the
    /// "Remember keys on this machine" button until settings persist it.
    pub fn vault_remember_override(&self, env: &str) -> Option<bool> {
        self.read(|s| s.vault_ops.remember_keys.get(env).copied())
    }

    pub fn set_vault_remember_override(&self, env: &str, on: bool) {
        self.write(|s| {
            s.vault_ops.remember_keys.insert(env.to_string(), on);
        });
    }

    /// Whether a service's deployed SHA in env_name matches prod's. HelmRelease
    /// SHAs refresh with the k8s poller, much faster than the GitHub comparison
    /// map — so this is the freshest signal for hiding a stale ahead badge.
    pub fn envs_in_sync(&self, service: &str, env_name: &str) -> bool {
        self.read(|state| {
            let sha_of = |env: &str| {
                state
                    .environments
                    .get(env)
                    .and_then(|e| e.helm_releases.iter().find(|h| h.name == service))
                    .and_then(|h| h.full_commit_sha())
            };
            let reference = crate::models::environments::current();
            let Some(reference) = reference.reference_id() else {
                return false;
            };
            match (sha_of(reference), sha_of(env_name)) {
                (Some(p), Some(o)) => p == o,
                _ => false,
            }
        })
    }

    /// Get how many commits ahead env_name is compared to prod for a service.
    /// Returns None if unknown, Some(N) where N>0 means ahead, N<0 means behind.
    pub fn get_ahead_of_prod(&self, service: &str, env_name: &str) -> Option<i64> {
        let reference = crate::models::environments::current()
            .reference_id()?
            .to_string();
        self.read(|state| {
            state
                .commit_comparisons
                .get(&(service.to_string(), reference.clone(), env_name.to_string()))
                .copied()
        })
    }

    /// Get all environment names
    #[allow(dead_code)]
    pub fn environment_names(&self) -> Vec<String> {
        self.read(|state| state.environments.keys().cloned().collect())
    }

    /// Update discovered org repos
    pub fn update_org_repos(&self, repos: Vec<OrgRepo>) {
        self.write(|state| {
            state.org_repos = repos;
        });
    }

    /// Get visible repos of a given kind (excludes hidden)
    pub fn get_repos_by_kind(&self, kind: RepoKind) -> Vec<OrgRepo> {
        self.read(|state| {
            state
                .org_repos
                .iter()
                .filter(|r| r.kind == kind && !state.hidden_repos.contains(&r.name))
                .cloned()
                .collect()
        })
    }

    /// Get all tracked repo names (services + libraries + actions, excluding hidden)
    pub fn get_tracked_repo_names(&self) -> Vec<String> {
        self.read(|state| {
            state
                .org_repos
                .iter()
                .filter(|r| {
                    !state.hidden_repos.contains(&r.name)
                        && matches!(
                            r.kind,
                            RepoKind::Service | RepoKind::Library | RepoKind::Action
                        )
                })
                .map(|r| r.name.clone())
                .collect()
        })
    }

    /// Get service repo names (for commit comparisons, etc.)
    /// Get repo names by kind (excluding hidden repos).
    pub fn get_repo_names_by_kind(&self, kind: RepoKind) -> Vec<String> {
        self.read(|state| {
            state
                .org_repos
                .iter()
                .filter(|r| r.kind == kind && !state.hidden_repos.contains(&r.name))
                .map(|r| r.name.clone())
                .collect()
        })
    }

    /// Auto-hide library repos with no releases and no recent activity (30 days).
    /// Only runs once — sets a meta flag so user's manual choices are respected afterwards.
    /// Call after org repos and releases are loaded.
    pub fn auto_hide_stale_repos(&self, cache: &crate::cache::Cache) {
        // Check if we've already done this
        if let Ok(Some(_)) = cache.get_meta("auto_hide_initialized") {
            return;
        }

        let cutoff = chrono::Utc::now() - chrono::Duration::days(30);

        let to_hide: Vec<String> = self.read(|state| {
            state
                .org_repos
                .iter()
                .filter(|r| {
                    if state.hidden_repos.contains(&r.name) {
                        return false;
                    }
                    // Always hide infra-* repos (terraform/IaC, not library artifacts)
                    if r.name.starts_with("infra-") || r.name == "infrastructure-modules" {
                        return true;
                    }
                    // For libraries: hide if no recent activity AND no recent releases
                    r.kind == RepoKind::Library
                        && r.updated_at.is_none_or(|t| t <= cutoff)
                        && !state
                            .releases
                            .iter()
                            .any(|rel| rel.repo == r.name && !rel.draft && rel.created_at > cutoff)
                })
                .map(|r| r.name.clone())
                .collect()
        });

        if !to_hide.is_empty() {
            tracing::info!("Auto-hiding {} stale library repos", to_hide.len());
            self.write(|state| {
                for name in &to_hide {
                    state.hidden_repos.insert(name.clone());
                }
            });
            for name in &to_hide {
                let _ = cache.save_hidden_repo(name);
            }
        }

        let _ = cache.set_meta("auto_hide_initialized", "1");
    }

    /// Hide everything in the config's `[repos] hide` list, on top of the
    /// persisted user toggles.
    pub fn seed_hidden_repos(&self, hide: &[String]) {
        if hide.is_empty() {
            return;
        }
        self.write(|state| {
            for name in hide {
                state.hidden_repos.insert(name.clone());
            }
        });
    }

    /// Toggle hidden state of a repo
    pub fn toggle_hidden(&self, name: &str) -> bool {
        self.write(|state| {
            if state.hidden_repos.contains(name) {
                state.hidden_repos.remove(name);
                false // now visible
            } else {
                state.hidden_repos.insert(name.to_string());
                true // now hidden
            }
        })
    }

    /// Check if a repo is hidden
    #[allow(dead_code)]
    pub fn is_repo_hidden(&self, name: &str) -> bool {
        self.read(|state| state.hidden_repos.contains(name))
    }

    /// Get the latest available version, if newer than current.
    pub fn get_update_available(&self) -> Option<LatestVersion> {
        // Use try_read to avoid blocking the UI thread if a writer is waiting
        let guard = match self.inner.try_read() {
            Ok(g) => g,
            Err(_) => return None,
        };
        guard.latest_version.as_ref().and_then(|lv| {
            let current = crate::app_version();
            if version_is_newer(&lv.version, current) {
                Some(lv.clone())
            } else {
                None
            }
        })
    }

    pub fn set_latest_version(&self, version: LatestVersion) {
        if let Ok(mut guard) = self.inner.try_write() {
            guard.latest_version = Some(version);
        } else {
            // If we can't get the lock, spawn a retry after a short delay
            let state = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let mut guard = state.inner.write().unwrap();
                guard.latest_version = Some(version);
            });
        }
    }

    /// Record progress of a Flux sync-with-source operation.
    pub fn set_flux_sync(&self, env: &str, sync: FluxSyncState) {
        self.write(|state| {
            state.flux_syncs.insert(env.to_string(), sync);
        });
    }

    /// Snapshot of all Flux sync operations.
    pub fn get_flux_syncs(&self) -> HashMap<String, FluxSyncState> {
        self.read(|state| state.flux_syncs.clone())
    }

    /// Whether a Flux sync is currently running for this env.
    pub fn flux_sync_running(&self, env: &str) -> bool {
        self.read(|state| {
            matches!(
                state.flux_syncs.get(env),
                Some(FluxSyncState::Running { .. })
            )
        })
    }

    /// Record how many commits main is ahead of the env's applied revision.
    pub fn set_applied_distance(&self, env: &str, commits: u64) {
        self.write(|state| {
            state.applied_distance.insert(env.to_string(), commits);
        });
    }

    /// Commits on main not yet applied in the env, None until computed.
    pub fn commits_behind_main(&self, env: &str) -> Option<u64> {
        self.read(|state| state.applied_distance.get(env).copied())
    }

    /// Collect all problems across all environments.
    /// Aggregates noisy per-pod problems into summaries (e.g. "5 fleet instances restarting").
    pub fn get_problems(&self) -> Vec<Problem> {
        let ladder = crate::models::environments::current();
        let fleet_name = crate::fleet::multi_image_repo()
            .unwrap_or("instances")
            .to_string();
        self.read(|state| {
            let mut problems = Vec::new();

            for (env_name, env) in &state.environments {
                // Connection errors
                if let Some(ref err) = env.error {
                    problems.push(Problem {
                        severity: Severity::Critical,
                        environment: env_name.clone(),
                        service: None,
                        message: format!("Cluster unreachable: {}", err),
                        timestamp: None,
                    });
                }

                // Unhealthy HelmReleases — skip Suspended (intentional) and aggregate fleet instances
                let mut fleet_degraded: Vec<&HelmReleaseInfo> = Vec::new();
                let mut fleet_failed: Vec<&HelmReleaseInfo> = Vec::new();

                for hr in &env.helm_releases {
                    match hr.status {
                        DeploymentStatus::Healthy | DeploymentStatus::Suspended => continue,
                        _ => {}
                    }

                    // Aggregate fleet instances
                    if hr.is_fleet_instance() {
                        match hr.status {
                            DeploymentStatus::Failed => fleet_failed.push(hr),
                            DeploymentStatus::Degraded | DeploymentStatus::Progressing => {
                                fleet_degraded.push(hr)
                            }
                            _ => fleet_degraded.push(hr),
                        }
                        continue;
                    }

                    let sev = if ladder.is_reference(env_name) {
                        Severity::Critical
                    } else {
                        Severity::Warning
                    };
                    let status_detail = match &hr.status_message {
                        Some(msg) => {
                            // Extract the useful part from Flux messages
                            // e.g. "Helm upgrade failed: ... timeout" → just the error
                            if let Some(pos) = msg.find("failed:") {
                                msg[pos..].chars().take(100).collect::<String>()
                            } else if msg.len() > 100 {
                                format!("{}…", &msg[..100])
                            } else {
                                msg.clone()
                            }
                        }
                        None => format!("{}", hr.status),
                    };
                    let service_name = hr.chart_name.as_deref().unwrap_or(&hr.name);
                    problems.push(Problem {
                        severity: sev,
                        environment: env_name.clone(),
                        service: Some(service_name.to_string()),
                        message: format!("{} is {}", service_name, status_detail),
                        timestamp: hr.last_transition_time,
                    });
                }

                // Summarize fleet problems
                if !fleet_failed.is_empty() {
                    let sev = if ladder.is_reference(env_name) {
                        Severity::Critical
                    } else {
                        Severity::Warning
                    };
                    let most_recent = fleet_failed
                        .iter()
                        .filter_map(|hr| hr.last_transition_time)
                        .max();
                    problems.push(Problem {
                        severity: sev,
                        environment: env_name.clone(),
                        service: Some(fleet_name.clone()),
                        message: format!(
                            "{} {} instance{} failed",
                            fleet_failed.len(),
                            fleet_name,
                            if fleet_failed.len() == 1 { "" } else { "s" }
                        ),
                        timestamp: most_recent,
                    });
                }
                if !fleet_degraded.is_empty() {
                    let sev = if ladder.is_reference(env_name) {
                        Severity::Warning
                    } else {
                        Severity::Info
                    };
                    let most_recent = fleet_degraded
                        .iter()
                        .filter_map(|hr| hr.last_transition_time)
                        .max();
                    problems.push(Problem {
                        severity: sev,
                        environment: env_name.clone(),
                        service: Some(fleet_name.clone()),
                        message: format!(
                            "{} {} instance{} degraded",
                            fleet_degraded.len(),
                            fleet_name,
                            if fleet_degraded.len() == 1 { "" } else { "s" }
                        ),
                        timestamp: most_recent,
                    });
                }

                // Bad pods — aggregate by service (namespace) and problem type
                let mut pod_problems: HashMap<(String, String), Vec<&PodInfo>> = HashMap::new();
                for pod in &env.pods {
                    if pod.is_problematic() {
                        // Determine the problem type
                        let problem_type = if pod.status.contains("CrashLoopBackOff") {
                            "crash-looping"
                        } else if pod.status.contains("OOMKilled") {
                            "OOM killed"
                        } else if pod.status.contains("ImagePullBackOff")
                            || pod.status.contains("ErrImagePull")
                        {
                            "image pull error"
                        } else if pod.restarts > 5 {
                            "excessive restarts"
                        } else {
                            "unhealthy"
                        };

                        // Use chart name or namespace as the service, skip fleet instance UUIDs
                        let service = if pod.namespace.starts_with("project-") {
                            fleet_name.clone()
                        } else {
                            pod.namespace.clone()
                        };

                        pod_problems
                            .entry((service, problem_type.to_string()))
                            .or_default()
                            .push(pod);
                    }
                }

                for ((service, problem_type), pods) in &pod_problems {
                    let sev = if ladder.is_reference(env_name) {
                        Severity::Critical
                    } else {
                        Severity::Warning
                    };
                    let max_restarts = pods.iter().map(|p| p.restarts).max().unwrap_or(0);
                    let message = if pods.len() == 1 {
                        if max_restarts > 5 {
                            format!("{}: {} ({} restarts)", service, problem_type, max_restarts)
                        } else {
                            format!("{}: {}", service, problem_type)
                        }
                    } else {
                        let restart_info = if max_restarts > 5 {
                            format!(", up to {} restarts", max_restarts)
                        } else {
                            String::new()
                        };
                        format!(
                            "{}: {} pod{} {}{}",
                            service,
                            pods.len(),
                            if pods.len() == 1 { "" } else { "s" },
                            problem_type,
                            restart_info
                        )
                    };
                    let most_recent = pods.iter().filter_map(|p| p.created).max();
                    problems.push(Problem {
                        severity: sev,
                        environment: env_name.clone(),
                        service: Some(service.clone()),
                        message,
                        timestamp: most_recent,
                    });
                }
            }

            // Vault: pods are the truth for sealed; unreachable only when there
            // is no pod data and the HTTP probe failed too.
            for (env_name, vault) in &state.vault_states {
                if vault.is_sealed_by_pods() {
                    let sev = if ladder.is_reference(env_name) {
                        Severity::Critical
                    } else {
                        Severity::Warning
                    };
                    problems.push(Problem {
                        severity: sev,
                        environment: env_name.clone(),
                        service: Some("vault".to_string()),
                        message: format!(
                            "Vault sealed: {} of {} pods",
                            vault.sealed_pod_count(),
                            vault.pods.len()
                        ),
                        timestamp: vault.sealed_since.or(vault.last_poll),
                    });
                } else if vault.is_unreachable() {
                    problems.push(Problem {
                        severity: Severity::Warning,
                        environment: env_name.clone(),
                        service: Some("vault".to_string()),
                        message: "Vault unreachable".to_string(),
                        timestamp: vault.last_poll,
                    });
                }
            }

            problems.sort_by(|a, b| {
                a.severity
                    .cmp(&b.severity)
                    .then_with(|| a.environment.cmp(&b.environment))
                    .then_with(|| a.service.cmp(&b.service))
                    .then_with(|| a.message.cmp(&b.message))
            });
            problems
        })
    }

    /// The newest vault poll across environments, for the top-bar breakdown.
    pub fn vault_last_poll(&self) -> Option<DateTime<Utc>> {
        self.read(|state| {
            state
                .vault_states
                .values()
                .filter_map(|v| v.last_poll)
                .max()
        })
    }
}

#[derive(Debug, Clone)]
pub struct Problem {
    pub severity: Severity,
    pub environment: String,
    pub service: Option<String>,
    pub message: String,
    /// When this problem was first observed (from K8s transition time, pod creation, etc.)
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Critical,
    Warning,
    Info,
}

/// Simple semver comparison: is `candidate` newer than `current`?
fn version_is_newer(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    };
    parse(candidate) > parse(current)
}
