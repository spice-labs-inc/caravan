//! Container image references as they appear in app-gitops pins, HelmRelease
//! values and pod container statuses.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// A parsed container image reference such as
/// `ghcr.io/notes-co/broker:pr-620@sha256:7814...`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    /// The reference exactly as written
    pub reference: String,
    /// Everything before the tag and digest (registry, org and name)
    pub repository: String,
    /// Last path segment of the repository (e.g. `libsql-server`)
    pub name: String,
    pub tag: Option<String>,
    /// Digest including its algorithm prefix (e.g. `sha256:f07e...`)
    pub digest: Option<String>,
}

impl ImageRef {
    pub fn parse(reference: &str) -> ImageRef {
        let reference = reference.trim();
        let (rest, digest) = match reference.split_once('@') {
            Some((r, d)) => (r, Some(d.to_string())),
            None => (reference, None),
        };
        let (repository, tag) = match rest.rsplit_once(':') {
            Some((r, t)) if !t.contains('/') && !t.is_empty() => (r, Some(t.to_string())),
            _ => (rest, None),
        };
        let name = repository.rsplit('/').next().unwrap_or(repository);
        ImageRef {
            reference: reference.to_string(),
            repository: repository.to_string(),
            name: name.to_string(),
            tag,
            digest,
        }
    }

    /// Whether this reference carries the given digest, with or without the
    /// `sha256:` prefix on either side.
    pub fn has_digest(&self, digest: &str) -> bool {
        self.digest
            .as_deref()
            .is_some_and(|d| digests_equal(d, digest))
    }
}

/// Compare two digests ignoring an algorithm prefix on either side.
pub fn digests_equal(a: &str, b: &str) -> bool {
    let strip = |s: &str| s.rsplit(':').next().unwrap_or(s).to_ascii_lowercase();
    !a.is_empty() && strip(a) == strip(b)
}

/// What kind of tag a component image carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TagKind {
    /// `v1.2.3` or `1.2.3`
    Version,
    /// `sha-<hex>` or a bare hex string of at least 7 characters
    Sha,
    /// `pr-N`
    Pr,
    /// `tree-<hex>`
    Tree,
    Other,
}

impl TagKind {
    /// Rank for choosing a display name among tags of the same digest:
    /// higher is better.
    pub fn rank(self) -> u8 {
        match self {
            TagKind::Version => 4,
            TagKind::Sha => 3,
            TagKind::Pr => 2,
            TagKind::Tree => 1,
            TagKind::Other => 0,
        }
    }
}

/// The pull request number of a `pr-N` tag (a trailing `@digest` is ignored).
pub fn pr_number(tag: &str) -> Option<u64> {
    let tag = tag.split('@').next().unwrap_or(tag);
    tag.strip_prefix("pr-")?.parse().ok()
}

pub fn tag_kind(tag: &str) -> TagKind {
    let is_hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
    if is_version(tag) {
        return TagKind::Version;
    }
    if let Some(rest) = tag.strip_prefix("sha-") {
        if is_hex(rest) {
            return TagKind::Sha;
        }
    }
    if let Some(rest) = tag.strip_prefix("pr-") {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return TagKind::Pr;
        }
    }
    if let Some(rest) = tag.strip_prefix("tree-") {
        if is_hex(rest) {
            return TagKind::Tree;
        }
    }
    if tag.len() >= 7 && is_hex(tag) {
        return TagKind::Sha;
    }
    TagKind::Other
}

fn is_version(tag: &str) -> bool {
    let core = tag.strip_prefix('v').unwrap_or(tag);
    let core = core.split_once('-').map(|(c, _)| c).unwrap_or(core);
    let core = core.split_once('+').map(|(c, _)| c).unwrap_or(core);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_reference_with_tag_and_digest() {
        let r = ImageRef::parse(
            "ghcr.io/notes-co/libsql-server:a64d504@sha256:f07e8119b33a266da781329f427b020649104de660c2a181dab12bed28d04412",
        );
        assert_eq!(r.repository, "ghcr.io/notes-co/libsql-server");
        assert_eq!(r.name, "libsql-server");
        assert_eq!(r.tag.as_deref(), Some("a64d504"));
        assert_eq!(
            r.digest.as_deref(),
            Some("sha256:f07e8119b33a266da781329f427b020649104de660c2a181dab12bed28d04412")
        );
        assert!(r.has_digest("f07e8119b33a266da781329f427b020649104de660c2a181dab12bed28d04412"));
        assert_eq!(tag_kind("a64d504"), TagKind::Sha);
    }

    #[test]
    fn parses_pr_tag_with_digest() {
        let r = ImageRef::parse("ghcr.io/notes-co/broker:pr-620@sha256:7814242c");
        assert_eq!(r.name, "broker");
        assert_eq!(r.tag.as_deref(), Some("pr-620"));
        assert_eq!(r.digest.as_deref(), Some("sha256:7814242c"));
        assert_eq!(tag_kind("pr-620"), TagKind::Pr);
    }

    #[test]
    fn parses_name_and_tag() {
        let r = ImageRef::parse("engine:v0.17.1");
        assert_eq!(r.repository, "engine");
        assert_eq!(r.name, "engine");
        assert_eq!(r.tag.as_deref(), Some("v0.17.1"));
        assert_eq!(r.digest, None);
    }

    #[test]
    fn parses_name_and_digest() {
        let r = ImageRef::parse("engine@sha256:abc");
        assert_eq!(r.name, "engine");
        assert_eq!(r.tag, None);
        assert_eq!(r.digest.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn parses_bare_name() {
        let r = ImageRef::parse("ghcr.io/notes-co/engine");
        assert_eq!(r.name, "engine");
        assert_eq!(r.repository, "ghcr.io/notes-co/engine");
        assert_eq!(r.tag, None);
        assert_eq!(r.digest, None);
    }

    #[test]
    fn registry_port_is_not_a_tag() {
        let r = ImageRef::parse("localhost:5000/org/name");
        assert_eq!(r.tag, None);
        assert_eq!(r.name, "name");
    }

    #[test]
    fn tag_kinds() {
        assert_eq!(tag_kind("v1.2.3"), TagKind::Version);
        assert_eq!(tag_kind("1.2.3"), TagKind::Version);
        assert_eq!(tag_kind("v0.17.1-rc1"), TagKind::Version);
        assert_eq!(
            tag_kind("sha-e3f01e86a467c48fed87c4de092ec00343b28c1f"),
            TagKind::Sha
        );
        assert_eq!(tag_kind("e3f01e8"), TagKind::Sha);
        assert_eq!(tag_kind("pr-612"), TagKind::Pr);
        assert_eq!(
            tag_kind("tree-3438098b2d850f296438bfe9d15b485e47c4077d"),
            TagKind::Tree
        );
        assert_eq!(tag_kind("latest"), TagKind::Other);
        assert_eq!(tag_kind("abc"), TagKind::Other);
        assert_eq!(tag_kind("pr-"), TagKind::Other);
        assert!(TagKind::Version.rank() > TagKind::Sha.rank());
        assert!(TagKind::Sha.rank() > TagKind::Pr.rank());
        assert!(TagKind::Pr.rank() > TagKind::Tree.rank());
    }
}
