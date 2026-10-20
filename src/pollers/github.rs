use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::cache::{OrgRepo, RepoKind};
use crate::config::Config;
use crate::models::image::{tag_kind, TagKind};
use crate::models::issue::{GitHubIssue, IssueLabel};
use crate::models::release::{GitHubRelease, PullRequest, WorkflowRun};
use crate::pollers::gh_http::GhClient;
use crate::state::{AppState, CommitRelation, RateLimitInfo};

/// The gitops repo where release PRs are opened (config `github.gitops_repo`).
fn gitops_repo() -> String {
    crate::config::gitops_repo()
}

/// Fan-out concurrency for per-repo / per-PR fetches.
const CONCURRENCY: usize = 6;

/// Repos pushed within this window get their workflow runs polled every
/// cycle; idle repos are only swept on full-sweep cycles.
const ACTIVE_PUSH_WINDOW_HOURS: i64 = 24;

/// Every Nth cycle is a full sweep: org listing, every workflow repo, and
/// the cold-start release probe.
const FULL_SWEEP_EVERY: u64 = 5;

/// How often an unclassified repo is probed for releases and tags.
const COLD_PROBE_INTERVAL_HOURS: i64 = 24;

/// Unclassified repos probed per full sweep, so a fresh cache spreads its
/// one-off cost over a few sweeps instead of one burst.
const COLD_PROBE_PER_SWEEP: usize = 20;

/// Sidebar view names whose data is polled only while they are on screen.
const VIEW_PULL_REQUESTS: &str = "pull_requests";
const VIEW_ISSUES: &str = "issues";

/// Which fetch tiers one poll cycle runs. Decided up front from the cycle
/// number, the view on screen and what has been warmed since startup, so
/// the decision is testable without a network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CyclePlan {
    /// Org listing, every workflow repo and the cold-start release probe
    pub full_sweep: bool,
    /// Org-wide PR search plus per-PR detail
    pub pull_requests: bool,
    /// Org-wide issue searches
    pub issues: bool,
    /// Projects V2 GraphQL (only ever alongside issues)
    pub projects_v2: bool,
    /// Releases and tags for Service + Library repos
    pub releases: bool,
    /// Releases and tags for unclassified repos, once per day each
    pub cold_probe: bool,
}

pub fn plan_cycle(
    cycle: u64,
    active_view: &str,
    force: bool,
    warmed_prs: bool,
    warmed_issues: bool,
    releases_every: u64,
) -> CyclePlan {
    let full_sweep = force || cycle.is_multiple_of(FULL_SWEEP_EVERY);
    let issues = force || !warmed_issues || active_view == VIEW_ISSUES;
    CyclePlan {
        full_sweep,
        pull_requests: force || !warmed_prs || active_view == VIEW_PULL_REQUESTS,
        issues,
        projects_v2: issues,
        releases: force || cycle.is_multiple_of(releases_every.max(1)),
        cold_probe: full_sweep,
    }
}

/// Entry point: start the GitHub poller as a long-running task.
pub async fn start_polling(
    config: Config,
    state: AppState,
    cache: Option<std::sync::Arc<crate::cache::Cache>>,
) {
    let org = config.github.org.clone();
    let interval = Duration::from_secs(config.polling.github_interval_secs.max(30));

    // Resolve the GitHub token
    let token = match config.resolve_github_token() {
        Ok(Some(t)) => t,
        Ok(None) => {
            error!("No GitHub token configured, GitHub poller will not start");
            return;
        }
        Err(e) => {
            error!(err = %e, "Failed to resolve GitHub token, GitHub poller will not start");
            return;
        }
    };

    let client = match GhClient::new(token, cache.clone(), state.clone()) {
        Ok(client) => client,
        Err(e) => {
            error!(err = %e, "Failed to build GitHub client");
            return;
        }
    };

    // Releases change far less often than CI runs, so they poll on their
    // own cadence expressed in base cycles.
    let releases_every = (config.polling.github_releases_interval_secs / interval.as_secs()).max(1);

    info!(
        org = %org,
        interval_secs = interval.as_secs(),
        releases_every,
        "Starting GitHub poller"
    );

    let mut poll_count: u64 = 0;
    let mut warmed_prs = false;
    let mut warmed_issues = false;
    let mut backoff_until: Option<tokio::time::Instant> = None;
    let mut relation_memo: RelationMemo = HashMap::new();
    let mut cold_probed: HashMap<String, DateTime<Utc>> = HashMap::new();
    loop {
        // Skip if we're in a rate-limit backoff
        if let Some(until) = backoff_until {
            if tokio::time::Instant::now() < until {
                info!(
                    "GitHub rate limited, backing off for {}s",
                    (until - tokio::time::Instant::now()).as_secs()
                );
                tokio::time::sleep(interval).await;
                continue;
            }
            backoff_until = None;
        }

        let force = state.take_refresh_requested();

        // Free endpoint, keeps the displayed budget honest even when most
        // requests are 304s.
        if let Err(e) = client.refresh_rate_limit().await {
            debug!(err = %e, "Failed to refresh rate limit");
        }

        let active_view = state.active_view();
        let plan = plan_cycle(
            poll_count,
            &active_view,
            force,
            warmed_prs,
            warmed_issues,
            releases_every,
        );
        debug!(cycle = poll_count, view = %active_view, ?plan, "GitHub cycle plan");

        if plan.full_sweep {
            match discover_org_repos(
                &client,
                &config.github.org,
                &state,
                &config.repos.service_names,
            )
            .await
            {
                Ok(repos) => {
                    info!("Discovered {} org repos", repos.len());
                    if let Some(ref cache) = cache {
                        if let Err(e) = cache.save_org_repos(&repos) {
                            tracing::warn!(err = %e, "Failed to cache org repos");
                        }
                    }
                    state.update_org_repos(repos);
                    // Auto-hide stale repos on first discovery
                    if let Some(ref cache) = cache {
                        state.auto_hide_stale_repos(cache);
                    }
                }
                Err(e) => warn!(err = %e, "Failed to discover org repos"),
            }
        }

        let universe = workflow_universe(&state);
        let workflow_repos = if plan.full_sweep {
            universe.clone()
        } else {
            active_workflow_repos(&state, &universe)
        };
        let release_repos = plan.releases.then(|| release_repo_list(&state));

        // Poll workflows, PRs, releases. Each branch inside poll_all carries
        // its own timeout, so a slow branch can't discard the others' results.
        let outcome = poll_all(
            &client,
            &org,
            &state,
            &workflow_repos,
            release_repos.as_deref(),
            plan.pull_requests,
        )
        .await;
        if plan.pull_requests && outcome.prs_ok {
            warmed_prs = true;
        }
        if let Some(e) = outcome.error {
            if e.contains("403") || e.to_lowercase().contains("rate limit") {
                warn!("GitHub rate limited, backing off for 5 minutes");
                backoff_until = Some(tokio::time::Instant::now() + Duration::from_secs(300));
            } else {
                warn!(err = %e, "GitHub poll cycle failed");
            }
            state.add_poller_error("github", &format!("Poll failed: {}", e));
        } else {
            state.write(|inner| {
                inner.last_github_poll = Some(Utc::now());
            });
        }

        if plan.cold_probe {
            cold_start_probe(&client, &org, &state, cache.as_deref(), &mut cold_probed).await;
        }

        // Cache whatever the poll landed (partial failures keep the other
        // branches' fresh data in state).
        if let Some(ref cache) = cache {
            let (runs, prs, releases, gitops_prs, tags) = state.read(|s| {
                (
                    s.workflow_runs.clone(),
                    s.pull_requests.clone(),
                    s.releases.clone(),
                    s.open_gitops_prs.clone(),
                    s.repo_tags.clone(),
                )
            });
            if let Err(e) = cache.save_github(&runs, &prs, &releases, &gitops_prs) {
                tracing::warn!(err = %e, "Failed to cache GitHub data");
            }
            if let Err(e) = cache.save_repo_tags(&tags) {
                tracing::warn!(err = %e, "Failed to cache repo tags");
            }
        }

        if plan.issues {
            match poll_issues(&client, &org, &state).await {
                Ok(issues) => {
                    info!("Fetched {} issues", issues.len());
                    warmed_issues = true;
                    state.update_issues(issues);
                    if let Some(ref cache) = cache {
                        let issues = state.get_issues();
                        if let Err(e) = cache.save_issues(&issues) {
                            tracing::warn!(err = %e, "Failed to cache issues");
                        }
                    }
                }
                Err(e) => {
                    warn!(err = %e, "Failed to poll issues");
                }
            }
        }

        // Projects V2 rides with the issues tier (GraphQL, separate budget)
        if plan.projects_v2 && (force || state.has_project_scope() != Some(false)) {
            match poll_projects_v2(&client, &org).await {
                Ok(fields) => {
                    if !fields.is_empty() {
                        state.set_has_project_scope(true);
                        state.merge_project_fields(fields);
                        // Re-cache issues with updated project fields
                        if let Some(ref cache) = cache {
                            let issues = state.get_issues();
                            if let Err(e) = cache.save_issues(&issues) {
                                tracing::warn!(err = %e, "Failed to cache issues after project merge");
                            }
                        }
                    }
                }
                Err(e) => {
                    let err_str = format!("{}", e);
                    if err_str.contains("INSUFFICIENT_SCOPES")
                        || err_str.contains("insufficient scopes")
                        || err_str.contains("Resource not accessible")
                    {
                        info!("PAT lacks read:project scope, disabling Projects V2 polling");
                        state.set_has_project_scope(false);
                    } else {
                        warn!(err = %e, "Failed to poll Projects V2");
                    }
                }
            }
        }

        // Commit relations every cycle: the per-SHA-pair memo means unchanged
        // services cost no API calls, and stale markers clear quickly.
        {
            resolve_pr_builds(&client, &org, &state).await;
            let (relations, comparisons) =
                fetch_commit_relations(&client, &org, &state, &mut relation_memo).await;
            if let Some(ref cache) = cache {
                if let Err(e) = cache.save_commit_comparisons(&comparisons) {
                    tracing::warn!(err = %e, "Failed to cache commit comparisons");
                }
                if let Err(e) = cache.save_commit_relations(&relations) {
                    tracing::warn!(err = %e, "Failed to cache commit relations");
                }
                let builds = state.read(|s| s.pr_builds.clone());
                if let Err(e) = cache.save_pr_builds(&builds) {
                    tracing::warn!(err = %e, "Failed to cache PR builds");
                }
            }
            state.set_commit_comparisons(comparisons);
            state.set_commit_relations(relations);
        }

        let (fetched, unchanged) = client.take_request_counts();
        let by_tier = client.take_tier_counts();
        info!(
            fetched,
            unchanged, "GitHub cycle requests ({} counted against rate limit)", fetched
        );
        debug!(
            breakdown = %format_tier_breakdown(&by_tier),
            "GitHub cycle requests by tier (billed/unchanged)"
        );

        poll_count += 1;
        // Sleep until next cycle, stretched when the rate budget is low, or
        // wake immediately on manual refresh or when a lazy view opens.
        let sleep_dur = adaptive_interval(interval, state.read(|s| s.gh_rate_core));
        if sleep_dur > interval {
            info!(
                secs = sleep_dur.as_secs(),
                "Rate budget low, stretching poll interval"
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(sleep_dur) => {},
            _ = state.github_refresh.notified() => {
                info!("Refresh requested, running immediately");
            },
        }
    }
}

fn format_tier_breakdown(by_tier: &BTreeMap<&'static str, (u64, u64)>) -> String {
    by_tier
        .iter()
        .map(|(tier, (fetched, unchanged))| format!("{tier}={fetched}/{unchanged}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Stretch the poll interval as the core rate budget runs down: double it
/// below 10% remaining, and below 100 requests wait for the reset itself.
fn adaptive_interval(base: Duration, core: Option<RateLimitInfo>) -> Duration {
    let Some(rl) = core else { return base };
    if rl.limit == 0 {
        return base;
    }
    if rl.remaining < 100 {
        let until_reset = rl
            .reset
            .map(|r| (r - Utc::now()).num_seconds().max(0) as u64 + 30)
            .unwrap_or(base.as_secs() * 4);
        return Duration::from_secs(until_reset.clamp(base.as_secs(), 1800));
    }
    if rl.remaining < rl.limit / 10 {
        return base * 2;
    }
    base
}

/// Repos whose workflow runs matter: deployed services plus the multi-image
/// monorepo, which builds the project components.
fn workflow_universe(state: &AppState) -> Vec<String> {
    state.read(|s| {
        s.org_repos
            .iter()
            .filter(|r| {
                (r.kind == RepoKind::Service
                    || Some(r.name.as_str()) == crate::fleet::multi_image_repo())
                    && !s.hidden_repos.contains(&r.name)
            })
            .map(|r| r.name.clone())
            .collect()
    })
}

/// Repos polled for releases and tags on the slow tier.
fn release_repo_list(state: &AppState) -> Vec<String> {
    state.read(|s| {
        s.org_repos
            .iter()
            .filter(|r| {
                matches!(r.kind, RepoKind::Service | RepoKind::Library)
                    && !s.hidden_repos.contains(&r.name)
            })
            .map(|r| r.name.clone())
            .collect()
    })
}

/// Repos worth polling for workflow runs off-sweep: pushed recently, or
/// with a run still queued/in progress whose completion we want promptly.
fn active_workflow_repos(state: &AppState, universe: &[String]) -> Vec<String> {
    let cutoff = Utc::now() - chrono::Duration::hours(ACTIVE_PUSH_WINDOW_HOURS);
    state.read(|s| {
        let pushed_recently: HashSet<&str> = s
            .org_repos
            .iter()
            .filter(|r| r.pushed_at.is_some_and(|t| t > cutoff))
            .map(|r| r.name.as_str())
            .collect();
        let running: HashSet<&str> = s
            .workflow_runs
            .iter()
            .filter(|r| r.status != "completed")
            .map(|r| r.repo.as_str())
            .collect();
        universe
            .iter()
            .filter(|r| pushed_recently.contains(r.as_str()) || running.contains(r.as_str()))
            .cloned()
            .collect()
    })
}

/// Cap a single poll branch so it fails alone instead of hanging the cycle.
async fn branch_timeout<T>(
    name: &str,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(Duration::from_secs(120), fut).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!("{} poll timed out", name)),
    }
}

/// What the PR branch of a cycle fetched.
enum PrBatch {
    /// Tracked-repo PRs with full detail, plus open gitops release PRs
    Full(Vec<PullRequest>, Vec<PullRequest>),
    /// Only the open gitops release PRs, from one search
    GitopsOnly(Vec<PullRequest>),
}

/// Per-branch result of one poll cycle.
struct PollOutcome {
    prs_ok: bool,
    error: Option<String>,
}

/// Run the base tiers of one cycle: workflow runs, the PR branch (full while
/// the view is open, otherwise gitops release PRs only) and, when due,
/// releases.
async fn poll_all(
    client: &GhClient,
    org: &str,
    state: &AppState,
    workflow_repos: &[String],
    release_repos: Option<&[String]>,
    prs_full: bool,
) -> PollOutcome {
    // join (not try_join): a failure in one branch must not throw away the
    // others' results. Releases/tags landing is what turns bare SHAs into
    // version labels. Per-branch timeouts for the same reason: one slow
    // branch times out alone instead of cancelling the whole cycle.
    let (workflow_runs, pr_results, releases_and_tags) = tokio::join!(
        branch_timeout("workflows", poll_workflow_runs(client, org, workflow_repos)),
        async {
            if prs_full {
                branch_timeout("PRs", poll_pull_requests(client, org, state))
                    .await
                    .map(|(prs, gitops)| PrBatch::Full(prs, gitops))
            } else {
                branch_timeout("gitops PRs", poll_gitops_prs(client, org))
                    .await
                    .map(PrBatch::GitopsOnly)
            }
        },
        async {
            match release_repos {
                Some(repos) => branch_timeout("releases", poll_releases(client, org, repos))
                    .await
                    .map(|r| Some((repos, r))),
                None => Ok(None),
            }
        },
    );

    let polled: HashSet<&str> = workflow_repos.iter().map(String::as_str).collect();
    let mut errors = Vec::new();
    let mut prs_ok = false;
    state.write(|s| {
        match workflow_runs {
            // Off-sweep cycles only poll active repos: keep the idle repos'
            // last-known runs instead of dropping them.
            Ok(runs) => {
                s.workflow_runs
                    .retain(|r| !polled.contains(r.repo.as_str()));
                s.workflow_runs.extend(runs);
            }
            Err(e) => errors.push(format!("workflows: {}", e)),
        }
        match pr_results {
            Ok(PrBatch::Full(prs, gitops_prs)) => {
                s.pull_requests = prs;
                s.open_gitops_prs = gitops_prs;
                prs_ok = true;
            }
            Ok(PrBatch::GitopsOnly(gitops_prs)) => {
                s.open_gitops_prs = gitops_prs;
            }
            Err(e) => errors.push(format!("PRs: {}", e)),
        }
        match releases_and_tags {
            Ok(Some((repos, (releases, tags)))) => {
                merge_releases(&mut s.releases, &mut s.repo_tags, repos, releases, tags);
            }
            Ok(None) => {}
            Err(e) => errors.push(format!("releases: {}", e)),
        }
    });

    if !errors.is_empty() {
        return PollOutcome {
            prs_ok,
            error: Some(errors.join("; ")),
        };
    }

    info!(
        "GitHub poll complete: {} workflow runs, {} PRs, {} releases, {} gitops PRs",
        state.read(|s| s.workflow_runs.len()),
        state.read(|s| s.pull_requests.len()),
        state.read(|s| s.releases.len()),
        state.read(|s| s.open_gitops_prs.len()),
    );

    PollOutcome {
        prs_ok,
        error: None,
    }
}

/// Replace the releases and tags of the given repos, keeping every other
/// repo's. Tiers fetch different subsets, so a wholesale replacement would
/// drop what another tier landed.
fn merge_releases(
    all_releases: &mut Vec<GitHubRelease>,
    all_tags: &mut HashMap<String, Vec<(String, String)>>,
    repos: &[String],
    releases: Vec<GitHubRelease>,
    tags: HashMap<String, Vec<(String, String)>>,
) {
    let set: HashSet<&str> = repos.iter().map(String::as_str).collect();
    all_releases.retain(|r| !set.contains(r.repo.as_str()));
    all_releases.extend(releases);
    for repo in repos {
        all_tags.remove(repo);
    }
    all_tags.extend(tags);
}

/// Unclassified repos worth probing for releases and tags: not forks, not
/// archived, not hidden, not probed within the last day. Bounded per sweep.
fn cold_probe_candidates(
    repos: &[OrgRepo],
    hidden: &HashSet<String>,
    last_probed: impl Fn(&str) -> Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Vec<String> {
    let cutoff = now - chrono::Duration::hours(COLD_PROBE_INTERVAL_HOURS);
    repos
        .iter()
        .filter(|r| r.kind == RepoKind::Other && !r.fork && !r.archived)
        .filter(|r| !hidden.contains(&r.name))
        .filter(|r| last_probed(&r.name).is_none_or(|t| t <= cutoff))
        .take(COLD_PROBE_PER_SWEEP)
        .map(|r| r.name.clone())
        .collect()
}

/// Cold-start path for a fresh cache: a repo only becomes a Library once a
/// release or tag is known for it, so unclassified repos are probed once a
/// day; the next org poll reclassifies any that turned out to have one.
async fn cold_start_probe(
    client: &GhClient,
    org: &str,
    state: &AppState,
    cache: Option<&crate::cache::Cache>,
    probed: &mut HashMap<String, DateTime<Utc>>,
) {
    let (repos, hidden) = state.read(|s| (s.org_repos.clone(), s.hidden_repos.clone()));
    let now = Utc::now();
    let candidates = cold_probe_candidates(
        &repos,
        &hidden,
        |name| {
            probed.get(name).copied().or_else(|| {
                cache
                    .and_then(|c| c.get_meta(&probe_meta_key(name)).ok().flatten())
                    .and_then(|s| parse_dt_str(&s))
            })
        },
        now,
    );
    if candidates.is_empty() {
        return;
    }
    debug!(
        count = candidates.len(),
        "Probing unclassified repos for releases"
    );
    match branch_timeout("cold probe", poll_releases(client, org, &candidates)).await {
        Ok((releases, tags)) => {
            let found = releases
                .iter()
                .map(|r| r.repo.as_str())
                .chain(tags.keys().map(String::as_str))
                .collect::<HashSet<_>>()
                .len();
            info!(
                probed = candidates.len(),
                with_versions = found,
                "Cold-start release probe done"
            );
            state.write(|s| {
                merge_releases(
                    &mut s.releases,
                    &mut s.repo_tags,
                    &candidates,
                    releases,
                    tags,
                );
            });
            for name in &candidates {
                probed.insert(name.clone(), now);
                if let Some(c) = cache {
                    let _ = c.set_meta(&probe_meta_key(name), &now.to_rfc3339());
                }
            }
        }
        Err(e) => warn!(err = %e, "Cold-start release probe failed"),
    }
}

fn probe_meta_key(repo: &str) -> String {
    format!("release_probe:{repo}")
}

fn parse_dt_str(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Poll recent workflow runs across the given repos.
async fn poll_workflow_runs(
    client: &GhClient,
    org: &str,
    repos: &[String],
) -> Result<Vec<WorkflowRun>> {
    let results: Vec<_> = stream::iter(repos.iter().cloned())
        .map(|repo| async move {
            let result = fetch_workflow_runs(client, org, &repo).await;
            (repo, result)
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    let mut all_runs = Vec::new();
    for (repo, result) in results {
        match result {
            Ok(runs) => all_runs.extend(runs),
            Err(e) => {
                warn!(repo = %repo, err = %e, "Failed to fetch workflow runs");
            }
        }
    }

    Ok(all_runs)
}

/// Fetch recent workflow runs for a single repo (last 20).
async fn fetch_workflow_runs(client: &GhClient, org: &str, repo: &str) -> Result<Vec<WorkflowRun>> {
    let route = format!("/repos/{org}/{repo}/actions/runs?per_page=20");
    let response = client
        .get(&route)
        .await
        .with_context(|| format!("Failed to fetch workflow runs for {}/{}", org, repo))?;

    let runs = response
        .get("workflow_runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut results = Vec::new();
    for run in runs {
        if let Some(wr) = parse_workflow_run(repo, &run) {
            results.push(wr);
        }
    }

    Ok(results)
}

fn parse_workflow_run(repo: &str, run: &serde_json::Value) -> Option<WorkflowRun> {
    Some(WorkflowRun {
        repo: repo.to_string(),
        workflow_name: run.get("name")?.as_str()?.to_string(),
        workflow_file: run.get("path").and_then(|v| v.as_str()).map(|p| {
            // "path" is like ".github/workflows/publish.yml" — extract filename
            p.rsplit('/').next().unwrap_or(p).to_string()
        }),
        run_number: run.get("run_number")?.as_u64()?,
        id: run.get("id")?.as_u64()?,
        status: run.get("status")?.as_str()?.to_string(),
        conclusion: run
            .get("conclusion")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        head_branch: run
            .get("head_branch")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        head_sha: run.get("head_sha")?.as_str()?.to_string(),
        event: run.get("event")?.as_str()?.to_string(),
        created_at: run
            .get("created_at")?
            .as_str()?
            .parse::<DateTime<Utc>>()
            .ok()?,
        updated_at: run
            .get("updated_at")?
            .as_str()?
            .parse::<DateTime<Utc>>()
            .ok()?,
        html_url: run.get("html_url")?.as_str()?.to_string(),
    })
}

/// Poll PRs org-wide via the search API: two searches replace a pair of list
/// calls per repo. Full PR data (branches, mergeable, reviews) comes from
/// per-PR fetches, which are conditional requests — unchanged PRs cost
/// nothing against the rate limit.
/// Returns (tracked-repo PRs, open gitops release PRs).
async fn poll_pull_requests(
    client: &GhClient,
    org: &str,
    state: &AppState,
) -> Result<(Vec<PullRequest>, Vec<PullRequest>)> {
    let tracked: HashSet<String> = state.get_tracked_repo_names().into_iter().collect();

    let open_items = client
        .search_issues(&format!("org:{org} is:pr is:open"), 5)
        .await?;
    // Hour-aligned window start keeps the search route (and its ETag) stable
    // between cycles; the exact 6h filter is applied client-side below.
    let window_start = (Utc::now() - chrono::Duration::hours(7)).format("%Y-%m-%dT%H:00:00Z");
    let closed_items = client
        .search_issues(
            &format!("org:{org} is:pr is:closed updated:>{window_start}"),
            3,
        )
        .await?;

    let cutoff = Utc::now() - chrono::Duration::hours(6);
    let mut wanted: Vec<(String, u64)> = Vec::new();
    for item in &open_items {
        let Some(repo) = repo_from_search_item(item) else {
            continue;
        };
        if !tracked.contains(&repo) && repo != gitops_repo() {
            continue;
        }
        if let Some(number) = item.get("number").and_then(|v| v.as_u64()) {
            wanted.push((repo, number));
        }
    }
    for item in &closed_items {
        let Some(repo) = repo_from_search_item(item) else {
            continue;
        };
        if !tracked.contains(&repo) {
            continue;
        }
        let recently = parse_dt_field(item, "updated_at").is_some_and(|t| t >= cutoff);
        if !recently {
            continue;
        }
        if let Some(number) = item.get("number").and_then(|v| v.as_u64()) {
            wanted.push((repo, number));
        }
    }

    let results: Vec<_> = stream::iter(wanted)
        .map(|(repo, number)| async move {
            let result = fetch_pull_request(client, org, &repo, number).await;
            (repo, number, result)
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    let mut prs = Vec::new();
    let mut gitops_prs = Vec::new();
    for (repo, number, result) in results {
        match result {
            Ok(pr) => {
                if repo == gitops_repo() {
                    if pr.is_open() {
                        gitops_prs.push(pr);
                    }
                } else {
                    prs.push(pr);
                }
            }
            Err(e) => {
                warn!(repo = %repo, number, err = %e, "Failed to fetch PR");
            }
        }
    }

    Ok((prs, gitops_prs))
}

/// Open release PRs on app-gitops from one search, for the home page's
/// release marker when the Pull Requests view is closed. Search items lack
/// branches, mergeability and reviews; the full PR tier fills those in
/// while the view is open.
async fn poll_gitops_prs(client: &GhClient, org: &str) -> Result<Vec<PullRequest>> {
    let gitops = gitops_repo();
    let items = client
        .search_issues(&format!("repo:{org}/{gitops} is:pr is:open"), 1)
        .await?;
    Ok(items
        .iter()
        .filter_map(|item| parse_search_pr(&gitops, item))
        .filter(|pr| pr.is_open())
        .collect())
}

/// A PR from a search result item: everything the list views show except
/// branches, mergeability and reviews.
fn parse_search_pr(repo: &str, item: &serde_json::Value) -> Option<PullRequest> {
    let merged_at = parse_dt_field(item.get("pull_request")?, "merged_at");
    let state = if merged_at.is_some() {
        "merged"
    } else {
        item.get("state").and_then(|s| s.as_str()).unwrap_or("open")
    };
    let str_field = |key: &str| {
        item.get(key)
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(PullRequest {
        repo: repo.to_string(),
        number: item.get("number")?.as_u64()?,
        title: str_field("title"),
        state: state.to_string(),
        draft: item.get("draft").and_then(|b| b.as_bool()).unwrap_or(false),
        author: item
            .pointer("/user/login")
            .and_then(|s| s.as_str())
            .map(str::to_string),
        head_branch: String::new(),
        base_branch: String::new(),
        head_sha: String::new(),
        created_at: parse_dt_field(item, "created_at").unwrap_or_else(Utc::now),
        updated_at: parse_dt_field(item, "updated_at").unwrap_or_else(Utc::now),
        merged_at,
        html_url: str_field("html_url"),
        labels: item
            .get("labels")
            .and_then(|l| l.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        review_comments: 0,
        mergeable: None,
        reviewers: Vec::new(),
        reviews: Vec::new(),
    })
}

fn repo_from_search_item(item: &serde_json::Value) -> Option<String> {
    item.get("repository_url")?
        .as_str()?
        .rsplit('/')
        .next()
        .map(str::to_string)
}

/// Fetch one PR's full data plus its reviews.
async fn fetch_pull_request(
    client: &GhClient,
    org: &str,
    repo: &str,
    number: u64,
) -> Result<PullRequest> {
    let pr = client
        .get(&format!("/repos/{org}/{repo}/pulls/{number}"))
        .await?;
    let reviews = fetch_pr_reviews(client, org, repo, number).await;
    parse_pull_request(repo, &pr, reviews)
        .ok_or_else(|| anyhow::anyhow!("Unparseable PR {}#{}", repo, number))
}

fn parse_pull_request(
    repo: &str,
    v: &serde_json::Value,
    reviews: Vec<(String, String)>,
) -> Option<PullRequest> {
    let merged_at = parse_dt_field(v, "merged_at");
    let state = if merged_at.is_some() {
        "merged"
    } else {
        v.get("state").and_then(|s| s.as_str()).unwrap_or("open")
    };
    let str_field = |ptr: &str| {
        v.pointer(ptr)
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let logins = |key: &str| {
        v.get(key)
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|u| u.get("login").and_then(|s| s.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(PullRequest {
        repo: repo.to_string(),
        number: v.get("number")?.as_u64()?,
        title: str_field("/title"),
        state: state.to_string(),
        draft: v.get("draft").and_then(|b| b.as_bool()).unwrap_or(false),
        author: v
            .pointer("/user/login")
            .and_then(|s| s.as_str())
            .map(str::to_string),
        head_branch: str_field("/head/ref"),
        base_branch: str_field("/base/ref"),
        head_sha: str_field("/head/sha"),
        created_at: parse_dt_field(v, "created_at").unwrap_or_else(Utc::now),
        updated_at: parse_dt_field(v, "updated_at").unwrap_or_else(Utc::now),
        merged_at,
        html_url: str_field("/html_url"),
        labels: v
            .get("labels")
            .and_then(|l| l.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        review_comments: v
            .get("review_comments")
            .and_then(|n| n.as_u64())
            .unwrap_or(0),
        mergeable: v.get("mergeable").and_then(|b| b.as_bool()),
        reviewers: logins("requested_reviewers"),
        reviews,
    })
}

/// Fetch reviews for a PR, returning the latest review per user as (login, state).
async fn fetch_pr_reviews(
    client: &GhClient,
    org: &str,
    repo: &str,
    pr_number: u64,
) -> Vec<(String, String)> {
    let route = format!("/repos/{org}/{repo}/pulls/{pr_number}/reviews?per_page=100");
    let Ok(response) = client.get(&route).await else {
        return Vec::new();
    };

    // Keep only the latest review per user (they may review multiple times)
    let mut latest: HashMap<String, String> = HashMap::new();
    for review in response.as_array().cloned().unwrap_or_default() {
        let Some(user) = review
            .pointer("/user/login")
            .and_then(|s| s.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let state = normalize_review_state(
            review
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or("COMMENTED"),
        );
        // Skip "Pending" if there's already a more meaningful state
        if state == "Pending" {
            continue;
        }
        latest.insert(user, state);
    }
    latest.into_iter().collect()
}

/// Match the strings the UI expects (octocrab's Debug formatting, which the
/// sqlite cache also stores).
fn normalize_review_state(raw: &str) -> String {
    match raw {
        "APPROVED" => "Approved",
        "CHANGES_REQUESTED" => "ChangesRequested",
        "COMMENTED" => "Commented",
        "DISMISSED" => "Dismissed",
        "PENDING" => "Pending",
        other => other,
    }
    .to_string()
}

/// Poll recent GitHub Releases across service + library repos.
/// Also returns each repo's recent git tags (tag -> SHA) so the UI can
/// resolve tags that have no GitHub Release.
async fn poll_releases(
    client: &GhClient,
    org: &str,
    repos: &[String],
) -> Result<(Vec<GitHubRelease>, HashMap<String, Vec<(String, String)>>)> {
    let results: Vec<_> = stream::iter(repos.iter().cloned())
        .map(|repo| async move {
            let result = fetch_releases(client, org, &repo).await;
            (repo, result)
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    let mut all_releases = Vec::new();
    let mut all_tags = HashMap::new();
    for (repo, result) in results {
        match result {
            Ok((releases, tags)) => {
                all_releases.extend(releases);
                if !tags.is_empty() {
                    all_tags.insert(repo, tags);
                }
            }
            Err(e) => {
                warn!(repo = %repo, err = %e, "Failed to fetch releases");
            }
        }
    }

    Ok((all_releases, all_tags))
}

/// Fetch recent releases and tags for a single repo.
/// Resolves each release tag to its actual commit SHA via the tags API,
/// since GitHub sets target_commitish to the branch name (e.g. "main")
/// rather than the commit SHA.
async fn fetch_releases(
    client: &GhClient,
    org: &str,
    repo: &str,
) -> Result<(Vec<GitHubRelease>, Vec<(String, String)>)> {
    let response = client
        .get(&format!("/repos/{org}/{repo}/releases?per_page=30"))
        .await
        .with_context(|| format!("Failed to fetch releases for {}/{}", org, repo))?;
    let items = response.as_array().cloned().unwrap_or_default();

    // Tags are fetched even when there are no releases — a bare tag is still
    // useful for labeling deployed SHAs.
    let tags = fetch_repo_tags(client, org, repo).await;
    let tag_sha_map: HashMap<&str, &str> =
        tags.iter().map(|(t, s)| (t.as_str(), s.as_str())).collect();

    let mut resolved_count = 0u32;
    let mut results = Vec::new();
    for release in &items {
        let Some(tag_name) = release.get("tag_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let commitish = if let Some(sha) = tag_sha_map.get(tag_name) {
            resolved_count += 1;
            sha.to_string()
        } else {
            release
                .get("target_commitish")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        if let Some(r) = parse_release(repo, release, commitish) {
            results.push(r);
        }
    }

    debug!(
        repo = repo,
        total = items.len(),
        resolved = resolved_count,
        "Fetched releases and resolved tag SHAs"
    );

    Ok((results, tags))
}

fn parse_release(repo: &str, v: &serde_json::Value, commitish: String) -> Option<GitHubRelease> {
    Some(GitHubRelease {
        repo: repo.to_string(),
        tag: v.get("tag_name")?.as_str()?.to_string(),
        name: v.get("name").and_then(|s| s.as_str()).map(str::to_string),
        body: v.get("body").and_then(|s| s.as_str()).map(str::to_string),
        draft: v.get("draft").and_then(|b| b.as_bool()).unwrap_or(false),
        prerelease: v
            .get("prerelease")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        author: v
            .pointer("/author/login")
            .and_then(|s| s.as_str())
            .map(str::to_string),
        published_at: parse_dt_field(v, "published_at"),
        created_at: parse_dt_field(v, "created_at").unwrap_or_else(Utc::now),
        html_url: v
            .get("html_url")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        id: v.get("id")?.as_u64()?,
        target_commitish: commitish,
    })
}

/// Fetch recent tags for a repo as (tag_name, commit SHA) pairs, newest first.
async fn fetch_repo_tags(client: &GhClient, org: &str, repo: &str) -> Vec<(String, String)> {
    let route = format!("/repos/{org}/{repo}/tags?per_page=100");
    match client.get(&route).await {
        Ok(response) => {
            let mut list = Vec::new();
            for tag in response.as_array().cloned().unwrap_or_default() {
                if let (Some(name), Some(sha)) = (
                    tag.get("name").and_then(|v| v.as_str()),
                    tag.get("commit")
                        .and_then(|c| c.get("sha"))
                        .and_then(|v| v.as_str()),
                ) {
                    list.push((name.to_string(), sha.to_string()));
                }
            }
            debug!(repo = repo, tag_count = list.len(), "Fetched tags");
            list
        }
        Err(e) => {
            warn!(repo = repo, err = %e, "Failed to fetch tags for SHA resolution; falling back to target_commitish");
            Vec::new()
        }
    }
}

/// Env pairs whose deployed commits the matrix relates: (ancestor, descendant)
/// for every ancestor on the ladder.
fn relation_pairs() -> Vec<(String, String)> {
    crate::models::environments::current().relation_pairs()
}

/// (service, base_env, head_env) -> (base_sha, head_sha, relation), so an
/// unchanged SHA pair never re-hits the compare API.
type RelationMemo = HashMap<(String, String, String), (String, String, CommitRelation)>;

/// A service or project component with its deployed commit per env.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Subject {
    service: String,
    repo: String,
    shas: BTreeMap<String, String>,
}

/// Relate every service's and component's deployed commits across the env
/// pairs. Returns the relation map and, for compatibility, the old
/// (service, "prod", env) -> ahead map.
async fn fetch_commit_relations(
    client: &GhClient,
    org: &str,
    state: &AppState,
    memo: &mut RelationMemo,
) -> (
    HashMap<(String, String, String), CommitRelation>,
    HashMap<(String, String, String), i64>,
) {
    let (mut subjects, known_repos) = deployed_subjects(state);
    let ladder = crate::models::environments::current();

    // Fleet instances aggregate under one row: their names are
    // per-env UUIDs that never match across environments.
    let projects_shas: BTreeMap<String, String> = state.read(|s| {
        s.environments
            .iter()
            .filter_map(|(env, e)| dominant_instance_sha(e).map(|sha| (env.clone(), sha)))
            .collect()
    });
    if let (Some(row), Some(repo), false) = (
        crate::fleet::instances_row(),
        crate::fleet::multi_image_repo(),
        projects_shas.is_empty(),
    ) {
        subjects.push(Subject {
            service: row.to_string(),
            repo: repo.to_string(),
            shas: projects_shas,
        });
    }

    let mut relations = HashMap::new();
    let mut comparisons = HashMap::new();
    for subject in &subjects {
        // A compare against a repo that is not in the org is a billed 404.
        if !known_repos.contains(&subject.repo) {
            continue;
        }
        for (base_env, head_env) in &relation_pairs() {
            let (base_env, head_env) = (base_env.as_str(), head_env.as_str());
            let (Some(base_sha), Some(head_sha)) =
                (subject.shas.get(base_env), subject.shas.get(head_env))
            else {
                continue;
            };
            let Some(rel) = compare_pair(
                client,
                org,
                &subject.repo,
                &subject.service,
                (base_env, head_env),
                (base_sha, head_sha),
                memo,
            )
            .await
            else {
                continue;
            };
            let key = (
                subject.service.clone(),
                base_env.to_string(),
                head_env.to_string(),
            );
            if ladder.is_reference(base_env) {
                comparisons.insert(key.clone(), rel.ahead);
            }
            relations.insert(key, rel);
        }
    }

    (relations, comparisons)
}

/// Every ordinary HelmRelease and every operator component with a resolvable
/// deployed commit per env, plus the set of org repo names.
fn deployed_subjects(state: &AppState) -> (Vec<Subject>, HashSet<String>) {
    state.read(|s| {
        let mut by_name: BTreeMap<String, Subject> = BTreeMap::new();
        let mut offer = |name: &str, repo: &str, env: &str, sha: String| {
            by_name
                .entry(name.to_string())
                .or_insert_with(|| Subject {
                    service: name.to_string(),
                    repo: repo.to_string(),
                    shas: BTreeMap::new(),
                })
                .shas
                .insert(env.to_string(), sha);
        };
        for (env, e) in &s.environments {
            for hr in &e.helm_releases {
                if hr.is_fleet_instance() {
                    continue;
                }
                let repo = &crate::fleet::repo_for_service(&hr.name);
                let pr_sha = hr
                    .image_tag
                    .as_deref()
                    .and_then(crate::models::image::pr_number)
                    .and_then(|n| s.pr_builds.get(&(repo.to_string(), n)))
                    .map(|b| b.compare_sha().to_string());
                if let Some(sha) = hr.full_commit_sha().or(pr_sha) {
                    offer(&hr.name, repo, env, sha);
                }
            }
        }
        for (env, pins) in &s.operator_pins {
            for (component, image) in &pins.components {
                let Some(tag) = image.tag.as_deref() else {
                    continue;
                };
                let repo = &crate::fleet::repo_for_image(&image.name);
                let sha = resolve_tag_sha(tag, repo, &s.repo_tags, &s.releases).or_else(|| {
                    crate::models::image::pr_number(tag)
                        .and_then(|n| s.pr_builds.get(&(repo.to_string(), n)))
                        .map(|b| b.compare_sha().to_string())
                });
                if let Some(sha) = sha {
                    offer(component, repo, env, sha);
                }
            }
        }
        let known: HashSet<String> = s.org_repos.iter().map(|r| r.name.clone()).collect();
        (by_name.into_values().collect(), known)
    })
}

/// Resolve every deployed `pr-N` tag (service HelmReleases and operator pins)
/// to the pull request's head commit and state. One conditional request per
/// PR, free while the PR is unchanged.
async fn resolve_pr_builds(client: &GhClient, org: &str, state: &AppState) {
    let mut wanted: BTreeSet<(String, u64)> = BTreeSet::new();
    state.read(|s| {
        for e in s.environments.values() {
            for hr in &e.helm_releases {
                if hr.is_fleet_instance() {
                    continue;
                }
                if let Some(n) = hr
                    .image_tag
                    .as_deref()
                    .and_then(crate::models::image::pr_number)
                {
                    let repo = &crate::fleet::repo_for_service(&hr.name);
                    wanted.insert((repo.to_string(), n));
                }
            }
        }
        for pins in s.operator_pins.values() {
            for image in pins.components.values() {
                if let Some(n) = image
                    .tag
                    .as_deref()
                    .and_then(crate::models::image::pr_number)
                {
                    let repo = &crate::fleet::repo_for_image(&image.name);
                    wanted.insert((repo.to_string(), n));
                }
            }
        }
    });
    if wanted.is_empty() {
        return;
    }
    let mut builds = HashMap::new();
    for (repo, n) in wanted {
        let route = format!("/repos/{org}/{repo}/pulls/{n}");
        match client.get(&route).await {
            Ok(v) => {
                let head_sha = v
                    .get("head")
                    .and_then(|h| h.get("sha"))
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                if head_sha.is_empty() {
                    continue;
                }
                let open = v.get("state").and_then(|s| s.as_str()) == Some("open");
                let merged = v.get("merged").and_then(|m| m.as_bool()).unwrap_or(false);
                let merge_sha = v
                    .get("merge_commit_sha")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                builds.insert(
                    (repo, n),
                    crate::state::PrBuild {
                        head_sha,
                        open,
                        merged,
                        merge_sha,
                    },
                );
            }
            Err(e) => {
                tracing::debug!(repo = %repo, pr = n, err = %e, "PR build lookup failed");
            }
        }
    }
    state.set_pr_builds(builds);
}

/// The commit a component tag points at: sha tags carry it, version tags
/// resolve through the repo's tags or releases. Pr tags resolve through
/// the PR lookup in deployed_subjects; tree tags stay unresolved.
fn resolve_tag_sha(
    tag: &str,
    repo: &str,
    repo_tags: &HashMap<String, Vec<(String, String)>>,
    releases: &[GitHubRelease],
) -> Option<String> {
    match tag_kind(tag) {
        TagKind::Sha => Some(tag.strip_prefix("sha-").unwrap_or(tag).to_string()),
        TagKind::Version => {
            let bare = tag.strip_prefix('v').unwrap_or(tag);
            let matches = |t: &str| t == tag || t.strip_prefix('v').unwrap_or(t) == bare;
            if let Some(tags) = repo_tags.get(repo) {
                if let Some((_, sha)) = tags.iter().find(|(t, _)| matches(t)) {
                    return Some(sha.clone());
                }
            }
            releases
                .iter()
                .find(|r| r.repo == repo && matches(&r.tag))
                .map(|r| r.target_commitish.as_str())
                .filter(|c| c.len() >= 7 && c.chars().all(|ch| ch.is_ascii_hexdigit()))
                .map(str::to_string)
        }
        TagKind::Pr | TagKind::Tree | TagKind::Other => None,
    }
}

/// The most common fleet instance SHA in an environment.
fn dominant_instance_sha(env: &crate::state::EnvironmentState) -> Option<String> {
    // The HelmRelease chart version SHA is the chart repo's commit; the app
    // version is the deployed multi-image repo image in the project pods. Exact name
    // match so a sidecar image doesn't count; non-SHA tags (e.g. pr-505)
    // can't be compared and are skipped.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for pod in env
        .pods
        .iter()
        .filter(|p| p.namespace.starts_with("project-"))
    {
        for image in &pod.images {
            let (name, tag) = crate::ui::app_table::parse_image(image);
            if Some(name.as_str()) == crate::fleet::multi_image_repo() {
                if let Some(sha) = tag.strip_prefix("sha-") {
                    *counts.entry(sha.to_string()).or_default() += 1;
                }
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(sha, _)| sha)
}

/// ahead_by / behind_by of a compare response: how far head is ahead of and
/// behind base.
fn parse_relation(resp: &serde_json::Value) -> Option<CommitRelation> {
    Some(CommitRelation {
        ahead: resp.get("ahead_by")?.as_i64()?,
        behind: resp.get("behind_by")?.as_i64()?,
    })
}

/// Relate two SHAs in a repo via the GitHub compare API, memoized per
/// (service, env pair) SHA pair. Equal SHAs are (0, 0) without a call.
async fn compare_pair(
    client: &GhClient,
    org: &str,
    repo: &str,
    service: &str,
    (base_env, head_env): (&str, &str),
    (base_sha, head_sha): (&str, &str),
    memo: &mut RelationMemo,
) -> Option<CommitRelation> {
    if base_sha == head_sha {
        return Some(CommitRelation::default());
    }
    let key = (
        service.to_string(),
        base_env.to_string(),
        head_env.to_string(),
    );
    if let Some((b, h, rel)) = memo.get(&key) {
        if b == base_sha && h == head_sha {
            return Some(*rel);
        }
    }
    let route = format!(
        "/repos/{}/{}/compare/{}...{}",
        org,
        repo,
        &base_sha[..base_sha.len().min(12)],
        &head_sha[..head_sha.len().min(12)]
    );
    match client.get(&route).await {
        Ok(resp) => {
            let rel = parse_relation(&resp)?;
            memo.insert(key, (base_sha.to_string(), head_sha.to_string(), rel));
            Some(rel)
        }
        Err(e) => {
            debug!(
                repo = repo,
                service = service,
                base = base_env,
                head = head_env,
                "Failed to compare commits: {}",
                e
            );
            None
        }
    }
}

/// Poll open + recently closed issues across tracked repos + internal-docs +
/// product, using two org-wide searches instead of two calls per repo.
async fn poll_issues(client: &GhClient, org: &str, state: &AppState) -> Result<Vec<GitHubIssue>> {
    let tracked_repos = state.get_tracked_repo_names();

    let mut repos_to_poll: HashSet<String> = tracked_repos.iter().cloned().collect();
    for extra in ["internal-docs", "product"] {
        repos_to_poll.insert(extra.to_string());
    }

    let open_items = client
        .search_issues(&format!("org:{org} is:issue is:open"), 10)
        .await?;
    // Day-aligned window start keeps the search route stable so its ETag can
    // hit; matches the old 7-day recently-closed window.
    let window_start = (Utc::now() - chrono::Duration::days(7)).format("%Y-%m-%d");
    let closed_items = client
        .search_issues(
            &format!("org:{org} is:issue is:closed updated:>{window_start}"),
            3,
        )
        .await?;

    let mut all_issues = Vec::new();
    for item in open_items.iter().chain(closed_items.iter()) {
        let Some(repo) = repo_from_search_item(item) else {
            continue;
        };
        if !repos_to_poll.contains(&repo) {
            continue;
        }
        // Search with is:issue shouldn't return PRs, but keep the guard
        if item.get("pull_request").is_some() {
            continue;
        }
        if let Some(mut issue) = parse_issue(&repo, item) {
            // For internal-docs issues, match repo names mentioned in title/body
            if repo == "internal-docs" {
                issue.matched_repos =
                    GitHubIssue::match_repos(&issue.title, issue.body.as_deref(), &tracked_repos);
            }
            all_issues.push(issue);
        }
    }

    Ok(all_issues)
}

/// Parse a single issue from GitHub REST API JSON.
fn parse_issue(repo: &str, item: &serde_json::Value) -> Option<GitHubIssue> {
    let number = item.get("number")?.as_u64()?;
    let title = item.get("title")?.as_str()?.to_string();
    let state = item
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("open")
        .to_string();

    let author = item
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let assignees: Vec<String> = item
        .get("assignees")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    a.get("login")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    let labels: Vec<IssueLabel> = item
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    let name = l.get("name")?.as_str()?.to_string();
                    let color = l
                        .get("color")
                        .and_then(|v| v.as_str())
                        .unwrap_or("808080")
                        .to_string();
                    Some(IssueLabel { name, color })
                })
                .collect()
        })
        .unwrap_or_default();

    let created_at = parse_dt_field(item, "created_at").unwrap_or_else(Utc::now);
    let updated_at = parse_dt_field(item, "updated_at").unwrap_or_else(Utc::now);
    let closed_at = parse_dt_field(item, "closed_at");

    let html_url = item
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let body = item
        .get("body")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let milestone = item
        .get("milestone")
        .and_then(|m| m.get("title"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let is_epic = GitHubIssue::detect_epic(&title);
    let epic_progress = if is_epic {
        GitHubIssue::parse_task_list_progress(body.as_deref())
    } else {
        None
    };

    Some(GitHubIssue {
        repo: repo.to_string(),
        number,
        title,
        state,
        author,
        assignees,
        labels,
        created_at,
        updated_at,
        closed_at,
        html_url,
        body,
        milestone,
        is_epic,
        epic_progress,
        matched_repos: Vec::new(), // Filled in later for internal-docs
        project_status: None,      // Filled in from Projects V2 later
        project_priority: None,
        on_project: false,
    })
}

fn parse_dt_field(v: &serde_json::Value, key: &str) -> Option<DateTime<Utc>> {
    v.get(key)?
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Poll GitHub Projects V2 for the org to get issue status/priority fields.
/// Returns a map of (repo, issue_number) → (status, priority).
async fn poll_projects_v2(
    client: &GhClient,
    org: &str,
) -> Result<HashMap<(String, u64), (Option<String>, Option<String>)>> {
    // GraphQL query to fetch all org-level projects and their items with field values.
    // We paginate through items with a cursor.
    let query = r#"
        query($org: String!, $cursor: String) {
            organization(login: $org) {
                projectsV2(first: 20) {
                    nodes {
                        id
                        title
                        items(first: 100, after: $cursor) {
                            pageInfo {
                                hasNextPage
                                endCursor
                            }
                            nodes {
                                content {
                                    ... on Issue {
                                        number
                                        repository {
                                            name
                                        }
                                    }
                                }
                                fieldValues(first: 10) {
                                    nodes {
                                        ... on ProjectV2ItemFieldSingleSelectValue {
                                            field {
                                                ... on ProjectV2SingleSelectField {
                                                    name
                                                }
                                            }
                                            name
                                        }
                                        ... on ProjectV2ItemFieldTextValue {
                                            field {
                                                ... on ProjectV2Field {
                                                    name
                                                }
                                            }
                                            text
                                        }
                                        ... on ProjectV2ItemFieldIterationValue {
                                            field {
                                                ... on ProjectV2IterationField {
                                                    name
                                                }
                                            }
                                            title
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    "#;

    let mut all_fields: HashMap<(String, u64), (Option<String>, Option<String>)> = HashMap::new();
    let mut cursor: Option<String> = None;

    // Paginate through project items (up to 5 pages = 500 items)
    for _ in 0..10 {
        let variables = serde_json::json!({
            "org": org,
            "cursor": cursor,
        });

        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let response = client
            .graphql(&body)
            .await
            .context("GraphQL request for Projects V2 failed")?;

        // Check for errors
        if let Some(errors) = response.get("errors").and_then(|e| e.as_array()) {
            for err in errors {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown");
                let err_type = err.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if err_type == "INSUFFICIENT_SCOPES" || msg.contains("insufficient scopes") {
                    return Err(anyhow::anyhow!("INSUFFICIENT_SCOPES: {}", msg));
                }
            }
            // If there are errors but not scope-related, log and try to continue with partial data
            let first_msg = errors
                .first()
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            warn!("GraphQL errors (continuing): {}", first_msg);
        }

        // Check for "Resource not accessible" in the message field (403-like)
        if let Some(msg) = response.get("message").and_then(|m| m.as_str()) {
            if msg.contains("Resource not accessible") {
                return Err(anyhow::anyhow!("Resource not accessible by integration"));
            }
        }

        let data = match response.get("data") {
            Some(d) => d,
            None => break,
        };

        let projects = data
            .pointer("/organization/projectsV2/nodes")
            .and_then(|n| n.as_array());

        let Some(projects) = projects else { break };

        let mut has_next = false;

        for project in projects {
            let items = project.pointer("/items/nodes").and_then(|n| n.as_array());

            let page_info = project.pointer("/items/pageInfo");
            if let Some(pi) = page_info {
                if pi.get("hasNextPage").and_then(|v| v.as_bool()) == Some(true) {
                    cursor = pi
                        .get("endCursor")
                        .and_then(|c| c.as_str())
                        .map(|s| s.to_string());
                    has_next = true;
                }
            }

            let Some(items) = items else { continue };

            for item in items {
                let content = item.get("content");
                let Some(content) = content else { continue };

                let number = content.get("number").and_then(|n| n.as_u64());
                let repo = content.pointer("/repository/name").and_then(|n| n.as_str());

                let (Some(number), Some(repo)) = (number, repo) else {
                    continue;
                };

                // Extract field values
                let mut status: Option<String> = None;
                let mut priority: Option<String> = None;

                if let Some(field_nodes) = item
                    .pointer("/fieldValues/nodes")
                    .and_then(|n| n.as_array())
                {
                    for fv in field_nodes {
                        let field_name = fv
                            .pointer("/field/name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("");

                        match field_name.to_lowercase().as_str() {
                            "status" => {
                                status = fv
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .map(|s| s.to_string());
                            }
                            "priority" => {
                                priority = fv
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .map(|s| s.to_string());
                            }
                            _ => {}
                        }
                    }
                }

                all_fields.insert((repo.to_string(), number), (status, priority));
            }
        }

        if !has_next {
            break;
        }
    }

    info!("Projects V2: fetched fields for {} items", all_fields.len());
    Ok(all_fields)
}

/// Discover all repos in the org and classify them.
/// Uses paginated list (1 API call per 100 repos) + classification from state data.
async fn discover_org_repos(
    client: &GhClient,
    org: &str,
    state: &AppState,
    service_names: &HashMap<String, String>,
) -> Result<Vec<OrgRepo>> {
    let mut all_repos = Vec::new();
    let mut page = 1u32;

    // Known services: gitops service dirs, operator components, HelmRelease
    // names in any env, and configured name overrides.
    let (known_services, has_release_or_tag) = state.read(|s| {
        let mut names: std::collections::HashSet<String> =
            s.gitops_services.iter().cloned().collect();
        names.extend(s.declared_versions.keys().cloned());
        for pins in s.operator_pins.values() {
            names.extend(pins.components.keys().cloned());
            for image in pins.components.values() {
                names.insert(crate::fleet::repo_for_image(&image.name).to_string());
            }
        }
        for env in s.environments.values() {
            for hr in &env.helm_releases {
                if !hr.is_fleet_instance() {
                    names.insert(hr.name.clone());
                    if let Some(chart) = &hr.chart_name {
                        names.insert(chart.clone());
                    }
                }
            }
        }
        names.extend(service_names.values().cloned());
        let mut with_versions: std::collections::HashSet<String> =
            s.releases.iter().map(|r| r.repo.clone()).collect();
        with_versions.extend(
            s.repo_tags
                .iter()
                .filter(|(_, tags)| !tags.is_empty())
                .map(|(repo, _)| repo.clone()),
        );
        (names, with_versions)
    });

    loop {
        let route = format!(
            "/orgs/{}/repos?per_page=100&page={}&type=all&sort=pushed&direction=desc",
            org, page
        );
        let response = client
            .get(&route)
            .await
            .context("Failed to list org repos")?;

        let repos = response.as_array().context("Expected array of repos")?;
        if repos.is_empty() {
            break;
        }

        for repo in repos {
            let name = repo
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }

            let archived = repo
                .get("archived")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let fork = repo.get("fork").and_then(|v| v.as_bool()).unwrap_or(false);
            let private = repo
                .get("private")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let license = repo
                .get("license")
                .and_then(|v| v.get("spdx_id"))
                .and_then(|v| v.as_str())
                .filter(|s| *s != "NOASSERTION")
                .map(|s| s.to_string());
            let description = repo
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let language = repo
                .get("language")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let updated_at = parse_dt_field(repo, "updated_at");
            let pushed_at = parse_dt_field(repo, "pushed_at");

            let kind = classify_repo(
                &name,
                archived,
                fork,
                &known_services,
                &has_release_or_tag,
                service_names,
            );

            all_repos.push(OrgRepo {
                name,
                description,
                language,
                archived,
                fork,
                private,
                license,
                updated_at,
                pushed_at,
                kind,
            });
        }

        let count = repos.len();
        if count < 100 {
            break;
        }
        page += 1;
    }

    Ok(all_repos)
}

/// Repo kind from its metadata and what the rest of the app knows. A repo
/// is a Library only once a release or tag is known for it; the rest is
/// Other and never polled for runs.
fn classify_repo(
    name: &str,
    archived: bool,
    fork: bool,
    known_services: &HashSet<String>,
    has_release_or_tag: &HashSet<String>,
    service_names: &HashMap<String, String>,
) -> RepoKind {
    let service_name = service_names.get(name).map(String::as_str).unwrap_or(name);
    if archived {
        RepoKind::Other
    } else if name == gitops_repo() || name.ends_with("-gitops") {
        RepoKind::Gitops
    } else if name.starts_with(&crate::config::action_repo_prefix()) {
        RepoKind::Action
    } else if known_services.contains(name) || known_services.contains(service_name) {
        RepoKind::Service
    } else if !fork && has_release_or_tag.contains(name) {
        RepoKind::Library
    } else {
        RepoKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(cycle: u64, view: &str, warmed_prs: bool, warmed_issues: bool) -> CyclePlan {
        plan_cycle(cycle, view, false, warmed_prs, warmed_issues, 7)
    }

    #[test]
    fn idle_home_cycle_runs_only_base_tiers() {
        let p = plan(1, "apps", true, true);
        assert!(!p.full_sweep);
        assert!(!p.pull_requests);
        assert!(!p.issues);
        assert!(!p.projects_v2);
        assert!(!p.releases);
        assert!(!p.cold_probe);
    }

    #[test]
    fn startup_warms_lazy_tiers_once() {
        let p = plan(0, "apps", false, false);
        assert!(p.full_sweep);
        assert!(p.pull_requests);
        assert!(p.issues);
        assert!(p.projects_v2);
        assert!(p.releases);
        assert!(p.cold_probe);
        let p = plan(1, "apps", true, true);
        assert!(!p.pull_requests && !p.issues);
    }

    #[test]
    fn open_view_polls_its_tier_every_cycle() {
        let p = plan(3, "pull_requests", true, true);
        assert!(p.pull_requests);
        assert!(!p.issues);
        let p = plan(3, "issues", true, true);
        assert!(p.issues);
        assert!(p.projects_v2);
        assert!(!p.pull_requests);
        let p = plan(3, "libraries", true, true);
        assert!(!p.pull_requests && !p.issues);
    }

    #[test]
    fn full_sweep_every_fifth_cycle_and_releases_on_their_tier() {
        assert!(plan(5, "apps", true, true).full_sweep);
        assert!(plan(10, "apps", true, true).cold_probe);
        assert!(!plan(6, "apps", true, true).full_sweep);
        assert!(plan(7, "apps", true, true).releases);
        assert!(!plan(8, "apps", true, true).releases);
    }

    #[test]
    fn manual_refresh_runs_everything() {
        let p = plan_cycle(3, "apps", true, true, true, 7);
        assert_eq!(
            p,
            CyclePlan {
                full_sweep: true,
                pull_requests: true,
                issues: true,
                projects_v2: true,
                releases: true,
                cold_probe: true,
            }
        );
    }

    #[test]
    fn relation_from_compare_response() {
        let rel = parse_relation(&json!({"ahead_by": 3, "behind_by": 1, "status": "diverged"}));
        assert_eq!(
            rel,
            Some(CommitRelation {
                ahead: 3,
                behind: 1
            })
        );
        assert_eq!(parse_relation(&json!({"ahead_by": 3})), None);
    }

    #[test]
    fn tag_sha_resolution() {
        let mut repo_tags = HashMap::new();
        repo_tags.insert(
            "engine".to_string(),
            vec![("v0.17.1".to_string(), "abcdef1234567890".to_string())],
        );
        let releases = vec![GitHubRelease {
            repo: "notes-api".to_string(),
            tag: "v1.5.5".to_string(),
            name: None,
            body: None,
            draft: false,
            prerelease: false,
            author: None,
            published_at: None,
            created_at: Utc::now(),
            html_url: String::new(),
            id: 1,
            target_commitish: "0123456789abcdef".to_string(),
        }];
        assert_eq!(
            resolve_tag_sha("sha-e3f01e86", "engine", &repo_tags, &releases).as_deref(),
            Some("e3f01e86")
        );
        assert_eq!(
            resolve_tag_sha("0.17.1", "engine", &repo_tags, &releases).as_deref(),
            Some("abcdef1234567890")
        );
        assert_eq!(
            resolve_tag_sha("v1.5.5", "notes-api", &repo_tags, &releases).as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(
            resolve_tag_sha("pr-620", "engine", &repo_tags, &releases),
            None
        );
        assert_eq!(
            resolve_tag_sha("tree-bedcc267", "engine", &repo_tags, &releases),
            None
        );
        assert_eq!(
            resolve_tag_sha("v9.9.9", "engine", &repo_tags, &releases),
            None
        );
    }

    fn other_repo(name: &str, fork: bool, archived: bool) -> OrgRepo {
        OrgRepo {
            name: name.to_string(),
            description: None,
            language: None,
            archived,
            fork,
            private: true,
            license: None,
            updated_at: None,
            pushed_at: None,
            kind: RepoKind::Other,
        }
    }

    #[test]
    fn cold_start_other_becomes_library_once_probed() {
        let known_services: HashSet<String> = ["notes-api".to_string()].into();
        let names = HashMap::new();
        let mut versions: HashSet<String> = HashSet::new();
        assert_eq!(
            classify_repo(
                "notes-sdk",
                false,
                false,
                &known_services,
                &versions,
                &names
            ),
            RepoKind::Other
        );

        // Fresh cache: notes-sdk is a probe candidate, forks and archived repos are not.
        let repos = vec![
            other_repo("notes-sdk", false, false),
            other_repo("some-fork", true, false),
            other_repo("old-thing", false, true),
        ];
        let now = Utc::now();
        let picked = cold_probe_candidates(&repos, &HashSet::new(), |_| None, now);
        assert_eq!(picked, vec!["notes-sdk".to_string()]);

        // Probed an hour ago: skipped until a day has passed.
        let recent = now - chrono::Duration::hours(1);
        let picked = cold_probe_candidates(&repos, &HashSet::new(), |_| Some(recent), now);
        assert!(picked.is_empty());
        let stale = now - chrono::Duration::hours(25);
        let picked = cold_probe_candidates(&repos, &HashSet::new(), |_| Some(stale), now);
        assert_eq!(picked.len(), 1);

        // The probe found a tag: the next org poll classifies it Library.
        versions.insert("notes-sdk".to_string());
        assert_eq!(
            classify_repo(
                "notes-sdk",
                false,
                false,
                &known_services,
                &versions,
                &names
            ),
            RepoKind::Library
        );
        // A fork never becomes a Library, release or not.
        assert_eq!(
            classify_repo("notes-sdk", false, true, &known_services, &versions, &names),
            RepoKind::Other
        );
        assert_eq!(
            classify_repo(
                "notes-api",
                false,
                false,
                &known_services,
                &versions,
                &names
            ),
            RepoKind::Service
        );
    }

    #[test]
    fn merge_releases_keeps_other_repos() {
        let mk = |repo: &str, tag: &str| GitHubRelease {
            repo: repo.to_string(),
            tag: tag.to_string(),
            name: None,
            body: None,
            draft: false,
            prerelease: false,
            author: None,
            published_at: None,
            created_at: Utc::now(),
            html_url: String::new(),
            id: 1,
            target_commitish: String::new(),
        };
        let mut releases = vec![mk("notes-api", "v1"), mk("engine", "v1")];
        let mut tags: HashMap<String, Vec<(String, String)>> = HashMap::new();
        tags.insert(
            "notes-api".to_string(),
            vec![("v1".to_string(), "a".to_string())],
        );
        let mut new_tags = HashMap::new();
        new_tags.insert(
            "engine".to_string(),
            vec![("v2".to_string(), "b".to_string())],
        );
        merge_releases(
            &mut releases,
            &mut tags,
            &["engine".to_string()],
            vec![mk("engine", "v2")],
            new_tags,
        );
        let engine: Vec<_> = releases.iter().filter(|r| r.repo == "engine").collect();
        assert_eq!(engine.len(), 1);
        assert_eq!(engine[0].tag, "v2");
        assert!(releases.iter().any(|r| r.repo == "notes-api"));
        assert_eq!(tags["notes-api"][0].0, "v1");
        assert_eq!(tags["engine"][0].0, "v2");
    }

    #[test]
    fn search_item_becomes_release_pr() {
        let item = json!({
            "number": 42,
            "title": "Release engine v0.17.2",
            "state": "open",
            "draft": false,
            "html_url": "https://github.com/o/app-gitops/pull/42",
            "user": {"login": "bot"},
            "labels": [{"name": "release"}],
            "created_at": "2026-09-28T10:00:00Z",
            "updated_at": "2026-09-28T10:05:00Z",
            "pull_request": {"merged_at": null}
        });
        let pr = parse_search_pr("app-gitops", &item).unwrap();
        assert!(pr.is_open());
        assert_eq!(pr.number, 42);
        assert_eq!(pr.title, "Release engine v0.17.2");
        assert_eq!(pr.labels, vec!["release".to_string()]);
        assert!(parse_search_pr("app-gitops", &json!({"number": 1})).is_none());
    }
}
