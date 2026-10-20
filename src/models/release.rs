use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A GitHub Release (tag-based) from any tracked repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubRelease {
    /// Repository name (e.g. "notes-api", "goatrodeo")
    pub repo: String,
    /// Release tag (e.g. "v0.8.2")
    pub tag: String,
    /// Release title / name
    pub name: Option<String>,
    /// Release body / notes (markdown)
    pub body: Option<String>,
    /// Whether this is a draft
    pub draft: bool,
    /// Whether this is a prerelease
    pub prerelease: bool,
    /// Who created the release (GitHub login)
    pub author: Option<String>,
    /// When the release was published
    pub published_at: Option<DateTime<Utc>>,
    /// When the release was created
    pub created_at: DateTime<Utc>,
    /// GitHub URL for this release
    pub html_url: String,
    /// GitHub release ID
    pub id: u64,
    /// The commit SHA this release tag points to
    pub target_commitish: String,
}

#[allow(dead_code)]
impl GitHubRelease {
    /// Check if this release's commit matches a given SHA (full or abbreviated)
    pub fn matches_sha(&self, sha: &str) -> bool {
        let clean = sha.strip_prefix("sha-").unwrap_or(sha);
        self.target_commitish.starts_with(clean) || clean.starts_with(&self.target_commitish)
    }
}

/// A GitHub Actions workflow run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    /// Repository name (e.g. "notes-api")
    pub repo: String,
    /// Workflow name (e.g. "Build, Publish, and Release")
    pub workflow_name: String,
    /// Workflow file name (e.g. "publish.yml")
    pub workflow_file: Option<String>,
    /// Run number
    pub run_number: u64,
    /// Run ID
    pub id: u64,
    /// Status: "queued", "in_progress", "completed"
    pub status: String,
    /// Conclusion (when completed): "success", "failure", "cancelled", etc.
    pub conclusion: Option<String>,
    /// Branch that triggered the run
    pub head_branch: Option<String>,
    /// Commit SHA
    pub head_sha: String,
    /// Event that triggered the run (push, pull_request, release, workflow_dispatch)
    pub event: String,
    /// When the run was created
    pub created_at: DateTime<Utc>,
    /// When the run was last updated
    pub updated_at: DateTime<Utc>,
    /// GitHub URL for the run
    pub html_url: String,
}

#[allow(dead_code)]
impl WorkflowRun {
    /// Short (8-char) commit SHA
    pub fn short_sha(&self) -> &str {
        &self.head_sha[..self.head_sha.len().min(8)]
    }

    /// Whether this run is currently in progress
    pub fn is_in_progress(&self) -> bool {
        self.status == "in_progress" || self.status == "queued"
    }

    /// Whether this run failed
    pub fn is_failed(&self) -> bool {
        self.conclusion.as_deref() == Some("failure")
    }

    /// Whether this run succeeded
    pub fn is_success(&self) -> bool {
        self.conclusion.as_deref() == Some("success")
    }
}

/// A GitHub Pull Request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    /// Repository name (e.g. "gitops", "notes-api")
    pub repo: String,
    /// PR number
    pub number: u64,
    /// PR title
    pub title: String,
    /// PR state: "open", "closed"
    pub state: String,
    /// Whether the PR is a draft
    pub draft: bool,
    /// Who opened the PR (GitHub login)
    pub author: Option<String>,
    /// Head branch
    pub head_branch: String,
    /// Base branch (usually "main")
    pub base_branch: String,
    /// Head commit SHA
    pub head_sha: String,
    /// When the PR was created
    pub created_at: DateTime<Utc>,
    /// When the PR was last updated
    pub updated_at: DateTime<Utc>,
    /// When the PR was merged (if merged)
    pub merged_at: Option<DateTime<Utc>>,
    /// GitHub URL
    pub html_url: String,
    /// PR labels
    pub labels: Vec<String>,
    /// Number of review comments
    pub review_comments: u64,
    /// Mergeable state if known
    pub mergeable: Option<bool>,
    /// Requested reviewers (GitHub logins) — people asked but haven't reviewed yet
    pub reviewers: Vec<String>,
    /// Completed reviews: (login, state) — people who submitted a review
    pub reviews: Vec<(String, String)>,
}

impl PullRequest {
    /// Whether this is a release PR on the gitops repo (opened by a release action)
    pub fn is_release_pr(&self) -> bool {
        self.repo == crate::config::gitops_repo()
            && (self.title.starts_with("release:")
                || self.title.starts_with("Release:")
                || self.author.as_deref() == Some("github-actions[bot]")
                || self.author.as_deref() == Some("github-actions"))
    }

    /// Whether this PR is open
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    /// Whether this PR has been merged
    #[allow(dead_code)]
    pub fn is_merged(&self) -> bool {
        self.merged_at.is_some()
    }

    /// Extract the service name from a release PR title like "release: api 0.8.2"
    pub fn release_service_name(&self) -> Option<&str> {
        if !self.is_release_pr() {
            return None;
        }
        let title = self.title.trim();
        let after_colon = title.split(':').nth(1)?.trim();
        after_colon.split_whitespace().next()
    }

    /// Extract the version from a release PR title like "release: api 0.8.2"
    pub fn release_version(&self) -> Option<&str> {
        if !self.is_release_pr() {
            return None;
        }
        let title = self.title.trim();
        let after_colon = title.split(':').nth(1)?.trim();
        after_colon.split_whitespace().nth(1)
    }

    /// Short (8-char) head commit SHA
    #[allow(dead_code)]
    pub fn short_sha(&self) -> &str {
        &self.head_sha[..self.head_sha.len().min(8)]
    }
}
