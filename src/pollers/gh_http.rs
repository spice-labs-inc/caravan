use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::cache::Cache;
use crate::state::{AppState, RateLimitInfo};

const API_BASE: &str = "https://api.github.com";

/// GitHub REST client that sends conditional requests (If-None-Match) using
/// ETags persisted in the sqlite cache. A 304 response is served from the
/// cached body and does not count against the API rate limit, so routes whose
/// data hasn't changed are effectively free to re-poll.
pub struct GhClient {
    http: reqwest::Client,
    token: String,
    cache: Option<Arc<Cache>>,
    state: AppState,
    fetched: std::sync::atomic::AtomicU64,
    not_modified: std::sync::atomic::AtomicU64,
    /// Per-tier (fetched, not_modified) since the last take, keyed by the
    /// route classification in `tier_of`.
    by_tier: Mutex<BTreeMap<&'static str, (u64, u64)>>,
}

impl GhClient {
    pub fn new(token: String, cache: Option<Arc<Cache>>, state: AppState) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("caravan")
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self {
            http,
            token,
            cache,
            state,
            fetched: std::sync::atomic::AtomicU64::new(0),
            not_modified: std::sync::atomic::AtomicU64::new(0),
            by_tier: Mutex::new(BTreeMap::new()),
        })
    }

    /// (fetched, served-from-cache) request counts since the last call.
    pub fn take_request_counts(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.fetched.swap(0, Ordering::Relaxed),
            self.not_modified.swap(0, Ordering::Relaxed),
        )
    }

    /// Per-tier (fetched, not_modified) counts since the last call.
    pub fn take_tier_counts(&self) -> BTreeMap<&'static str, (u64, u64)> {
        std::mem::take(&mut *self.by_tier.lock().unwrap())
    }

    fn count(&self, route: &str, fetched: bool) {
        use std::sync::atomic::Ordering;
        if fetched {
            self.fetched.fetch_add(1, Ordering::Relaxed);
        } else {
            self.not_modified.fetch_add(1, Ordering::Relaxed);
        }
        let mut by_tier = self.by_tier.lock().unwrap();
        let entry = by_tier.entry(tier_of(route)).or_insert((0, 0));
        if fetched {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }

    /// GET an API route (starting with `/`), returning parsed JSON.
    /// Uses a stored ETag when available; on 304 the cached body is returned.
    pub async fn get(&self, route: &str) -> Result<serde_json::Value> {
        let cached = self
            .cache
            .as_ref()
            .and_then(|c| c.get_http_cache(route).ok().flatten());

        let mut req = self
            .http
            .get(format!("{API_BASE}{route}"))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some((etag, _)) = &cached {
            req = req.header("If-None-Match", etag.clone());
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("GET {route} failed"))?;
        record_rate_headers(&self.state, resp.headers());
        let status = resp.status();

        if status == reqwest::StatusCode::NOT_MODIFIED {
            self.count(route, false);
            if let Some((_, body)) = cached {
                debug!(route, "304, served from http cache");
                if let Some(c) = &self.cache {
                    let _ = c.touch_http_cache(route);
                }
                return serde_json::from_str(&body)
                    .with_context(|| format!("Cached body for {route} unparseable"));
            }
            anyhow::bail!("304 for {route} without a cached body");
        }
        if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            let remaining = header_str(resp.headers(), "x-ratelimit-remaining");
            anyhow::bail!(
                "GitHub rate limit / 403 on {route} (remaining {})",
                remaining.as_deref().unwrap_or("?")
            );
        }
        // Every non-304 response is billed, error responses included.
        self.count(route, true);
        if !status.is_success() {
            anyhow::bail!("GET {route} returned {status}");
        }

        let etag = header_str(resp.headers(), "etag");
        let body = resp
            .text()
            .await
            .with_context(|| format!("Failed to read body for {route}"))?;
        if let (Some(c), Some(etag)) = (&self.cache, etag) {
            if let Err(e) = c.put_http_cache(route, &etag, &body) {
                warn!(err = %e, route, "Failed to store http cache entry");
            }
        }
        serde_json::from_str(&body).with_context(|| format!("Response for {route} unparseable"))
    }

    /// POST a GraphQL query. GraphQL has its own rate budget; the response
    /// headers are recorded like any other so the sidebar figure stays honest.
    pub async fn graphql(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{API_BASE}/graphql"))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .context("GraphQL request failed")?;
        record_rate_headers(&self.state, resp.headers());
        self.count("/graphql", true);
        resp.json()
            .await
            .context("Failed to parse GraphQL response")
    }

    /// POST a JSON body to a REST route. Counted against the rate budget;
    /// a non-2xx response surfaces GitHub's `message` in the error.
    pub async fn post(&self, route: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{API_BASE}{route}"))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {route} failed"))?;
        record_rate_headers(&self.state, resp.headers());
        self.count(route, true);
        let status = resp.status();
        let value: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            let message = value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("no message");
            let details = value
                .get("errors")
                .map(|e| format!(" {}", e))
                .unwrap_or_default();
            anyhow::bail!("GitHub {}: {}{}", status.as_u16(), message, details);
        }
        Ok(value)
    }

    /// Run a search query (already qualifier-formatted, unencoded), paginating
    /// up to `max_pages` pages of 100 items.
    pub async fn search_issues(
        &self,
        query: &str,
        max_pages: u32,
    ) -> Result<Vec<serde_json::Value>> {
        let q = urlencoding::encode(query);
        let mut items = Vec::new();
        for page in 1..=max_pages {
            let route = format!("/search/issues?q={q}&per_page=100&page={page}");
            let resp = self.get(&route).await?;
            let batch = resp
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let n = batch.len();
            items.extend(batch);
            if n < 100 {
                break;
            }
        }
        Ok(items)
    }

    /// Snapshot all rate-limit buckets. This endpoint does not count against
    /// the rate limit.
    pub async fn refresh_rate_limit(&self) -> Result<()> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{API_BASE}/rate_limit"))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .context("GET /rate_limit failed")?
            .json()
            .await
            .context("rate_limit response unparseable")?;
        for resource in ["core", "search"] {
            if let Some(bucket) = resp.pointer(&format!("/resources/{resource}")) {
                let info = RateLimitInfo {
                    remaining: bucket
                        .get("remaining")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    limit: bucket.get("limit").and_then(|v| v.as_u64()).unwrap_or(0),
                    reset: bucket
                        .get("reset")
                        .and_then(|v| v.as_i64())
                        .and_then(|t| Utc.timestamp_opt(t, 0).single()),
                };
                self.state.set_rate_limit(resource, info);
            }
        }
        Ok(())
    }
}

/// Keep the live rate-limit numbers current from any GitHub response's
/// headers. Callers outside `GhClient` (the update checker) use it so every
/// request the app makes shows in the sidebar figure.
pub fn record_rate_headers(state: &AppState, headers: &reqwest::header::HeaderMap) {
    let Some(resource) = header_str(headers, "x-ratelimit-resource") else {
        return;
    };
    let parse = |name: &str| header_str(headers, name).and_then(|v| v.parse::<u64>().ok());
    let (Some(remaining), Some(limit)) =
        (parse("x-ratelimit-remaining"), parse("x-ratelimit-limit"))
    else {
        return;
    };
    let reset: Option<DateTime<Utc>> =
        parse("x-ratelimit-reset").and_then(|t| Utc.timestamp_opt(t as i64, 0).single());
    state.set_rate_limit(
        &resource,
        RateLimitInfo {
            remaining,
            limit,
            reset,
        },
    );
}

/// Which poll tier a route belongs to, for the per-cycle request breakdown.
pub fn tier_of(route: &str) -> &'static str {
    if route.starts_with("/graphql") {
        "projects_v2"
    } else if route.starts_with("/orgs/") {
        "org_repos"
    } else if route.starts_with("/search/issues") {
        if route.contains("is%3Aissue") || route.contains("is:issue") {
            "issues"
        } else if route.contains("app-gitops") {
            "gitops_prs"
        } else {
            "pull_requests"
        }
    } else if route.contains("/actions/runs") {
        "workflows"
    } else if route.contains("/pulls/") {
        "pull_requests"
    } else if route.contains("/releases") || route.contains("/tags") {
        "releases"
    } else if route.contains("/compare/") {
        "relations"
    } else {
        "other"
    }
}

fn header_str(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_map_to_tiers() {
        assert_eq!(tier_of("/orgs/notes-co/repos?per_page=100"), "org_repos");
        assert_eq!(
            tier_of("/repos/notes-co/notes-api/actions/runs?per_page=20"),
            "workflows"
        );
        assert_eq!(tier_of("/repos/o/r/pulls/12"), "pull_requests");
        assert_eq!(
            tier_of("/repos/o/r/pulls/12/reviews?per_page=100"),
            "pull_requests"
        );
        assert_eq!(tier_of("/repos/o/r/releases?per_page=30"), "releases");
        assert_eq!(tier_of("/repos/o/r/tags?per_page=100"), "releases");
        assert_eq!(tier_of("/repos/o/r/compare/abc...def"), "relations");
        assert_eq!(tier_of("/graphql"), "projects_v2");
        let q = urlencoding::encode("org:o is:issue is:open");
        assert_eq!(tier_of(&format!("/search/issues?q={q}")), "issues");
        let q = urlencoding::encode("repo:o/app-gitops is:pr is:open");
        assert_eq!(tier_of(&format!("/search/issues?q={q}")), "gitops_prs");
        let q = urlencoding::encode("org:o is:pr is:open");
        assert_eq!(tier_of(&format!("/search/issues?q={q}")), "pull_requests");
    }
}
