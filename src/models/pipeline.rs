use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Tracks the journey of a commit through the full release pipeline.
/// A commit progresses through these stages from PR merge to production.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PipelineStage {
    /// PR opened but not yet merged
    InPR,
    /// PR merged, CI build running
    Building,
    /// Image and chart published to GHCR
    Published,
    /// Deployed to non-reference environments via gitops commit
    InStaging,
    /// GitHub Release created, release PR opened on app-gitops
    ReleasePROpen,
    /// Release PR merged, flux deploying to prod
    ReleasePRMerged,
    /// Successfully deployed to production
    InProd,
    /// Something failed in the pipeline
    Failed,
    /// Superseded by a newer commit (never reached prod independently)
    Superseded,
}

impl std::fmt::Display for PipelineStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InPR => write!(f, "In PR"),
            Self::Building => write!(f, "Building"),
            Self::Published => write!(f, "Published"),
            Self::InStaging => write!(f, "In Staging"),
            Self::ReleasePROpen => write!(f, "Release PR Open"),
            Self::ReleasePRMerged => write!(f, "Release PR Merged"),
            Self::InProd => write!(f, "In Prod"),
            Self::Failed => write!(f, "Failed"),
            Self::Superseded => write!(f, "Superseded"),
        }
    }
}

/// Tracks a single commit's journey through the pipeline.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitPipeline {
    /// Repository this commit belongs to
    pub repo: String,
    /// Full commit SHA
    pub sha: String,
    /// Commit message (first line)
    pub message: Option<String>,
    /// Author of the commit
    pub author: Option<String>,
    /// Current pipeline stage
    pub stage: PipelineStage,
    /// When each stage was reached
    pub stage_times: Vec<(PipelineStage, DateTime<Utc>)>,
    /// Associated workflow run IDs
    pub workflow_run_ids: Vec<u64>,
    /// Associated PR number (if from a PR)
    pub pr_number: Option<u64>,
    /// Associated release tag (if released)
    pub release_tag: Option<String>,
    /// Associated app-gitops PR number (for prod release)
    pub gitops_pr_number: Option<u64>,
}

#[allow(dead_code)]
impl CommitPipeline {
    /// Short (8-char) commit SHA
    pub fn short_sha(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }
}
