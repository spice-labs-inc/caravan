//! Create a GitHub release for what staging runs: the next version after the
//! repo's latest release, tagged at the deployed commit, with notes GitHub
//! generates (previewed and editable before anything is created).

use anyhow::{Context, Result};

use crate::pollers::gh_http::GhClient;
use crate::state::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

impl Bump {
    pub const ALL: [Bump; 3] = [Bump::Patch, Bump::Minor, Bump::Major];

    pub fn label(self) -> &'static str {
        match self {
            Bump::Patch => "patch",
            Bump::Minor => "minor",
            Bump::Major => "major",
        }
    }
}

/// The version after `latest` for the given bump, keeping the repo's `v`
/// prefix convention and dropping any prerelease or build suffix. Without a
/// latest release the first version is v0.1.0.
pub fn next_version(latest: Option<&str>, bump: Bump) -> String {
    let Some(latest) = latest.map(str::trim).filter(|s| !s.is_empty()) else {
        return "v0.1.0".to_string();
    };
    let prefixed = latest.starts_with('v');
    let core = latest.trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts: Vec<u64> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(3, 0);
    match bump {
        Bump::Major => {
            parts[0] += 1;
            parts[1] = 0;
            parts[2] = 0;
        }
        Bump::Minor => {
            parts[1] += 1;
            parts[2] = 0;
        }
        Bump::Patch => parts[2] += 1,
    }
    format!(
        "{}{}.{}.{}",
        if prefixed { "v" } else { "" },
        parts[0],
        parts[1],
        parts[2]
    )
}

fn semver_key(tag: &str) -> Option<(u64, u64, u64)> {
    let core = tag.trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = it.next().flatten()?;
    let minor = it.next().flatten()?;
    let patch = it.next().flatten()?;
    Some((major, minor, patch))
}

/// The repo's latest published, non-draft, non-prerelease release tag; when
/// no release is known, the highest semver git tag.
pub fn latest_release_tag(state: &AppState, repo: &str) -> Option<String> {
    let mut releases: Vec<_> = state
        .get_releases_for_repo(repo)
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease && semver_key(&r.tag).is_some())
        .collect();
    releases.sort_by_key(|a| std::cmp::Reverse(a.published_at));
    if let Some(r) = releases.first() {
        return Some(r.tag.clone());
    }
    state.read(|s| {
        s.repo_tags.get(repo).and_then(|tags| {
            tags.iter()
                .map(|(t, _)| t.clone())
                .filter(|t| semver_key(t).is_some())
                .max_by_key(|t| semver_key(t))
        })
    })
}

/// Notes GitHub would generate for a release of `tag` at `target`, without
/// creating anything.
pub async fn generate_notes(
    client: &GhClient,
    org: &str,
    repo: &str,
    tag: &str,
    target: &str,
) -> Result<String> {
    let body = serde_json::json!({ "tag_name": tag, "target_commitish": target });
    let resp = client
        .post(
            &format!("/repos/{org}/{repo}/releases/generate-notes"),
            &body,
        )
        .await
        .context("Generating release notes")?;
    Ok(resp
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or_default()
        .to_string())
}

/// Publish the release (tag created at `target` when it does not exist).
/// Returns the release page URL.
pub async fn create_release(
    client: &GhClient,
    org: &str,
    repo: &str,
    tag: &str,
    target: &str,
    notes: &str,
) -> Result<String> {
    let body = serde_json::json!({
        "tag_name": tag,
        "target_commitish": target,
        "name": tag,
        "body": notes,
        "draft": false,
        "prerelease": false,
        "generate_release_notes": false,
    });
    let resp = client
        .post(&format!("/repos/{org}/{repo}/releases"), &body)
        .await
        .context("Creating the release")?;
    Ok(resp
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or_default()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bumps_keep_the_prefix_convention() {
        assert_eq!(next_version(Some("v1.5.7"), Bump::Patch), "v1.5.8");
        assert_eq!(next_version(Some("v1.5.7"), Bump::Minor), "v1.6.0");
        assert_eq!(next_version(Some("v1.5.7"), Bump::Major), "v2.0.0");
        assert_eq!(next_version(Some("1.0.10"), Bump::Patch), "1.0.11");
        assert_eq!(next_version(Some("v0.17.3-rc1"), Bump::Patch), "v0.17.4");
        assert_eq!(next_version(None, Bump::Minor), "v0.1.0");
    }

    #[test]
    fn semver_tags_only() {
        assert_eq!(semver_key("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(semver_key("version-3.39.4"), None);
        assert_eq!(semver_key("libsql-server-v0.1.0"), None);
    }
}
