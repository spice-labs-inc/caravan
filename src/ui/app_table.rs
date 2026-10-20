//! The status matrix shared by the Status home and the Infrastructure page:
//! row collection, per-environment cells with release markers, and the
//! projects section (operator pins with their rollout across the fleet).
//!
//! Everything that decides what a cell says lives in plain functions on
//! cloned data so it can be tested without egui.

use crate::cache::RepoKind;
use crate::models::deployment::{DeploymentStatus, HelmReleaseInfo, PodInfo};
use crate::models::image::{digests_equal, ImageRef};
use crate::models::release::PullRequest;
use crate::state::{AppState, DeclaredVersion, EnvironmentState};
use crate::ui::icons;
use crate::ui::sidebar::View;
use crate::ui::theme::{self, Colors, ThemeMode};
use crate::ui::widgets;
use chrono::{DateTime, Utc};
use eframe::egui;
use std::collections::BTreeSet;

/// egui temp-data key: the environment the Projects view should preselect,
/// set by the fleet line and read once by the Projects view.
pub const PENDING_PROJECTS_ENV: &str = "pending_projects_env";

/// Service names shown as app rows: app-gitops service directories plus
/// helm releases whose name matches a service repo, minus hidden names and
/// minus the operator components (those live under the projects section).
///
/// Everything is resolved in a single state read. Never call back into
/// `state` from inside the closure: a nested read deadlocks once a poller's
/// write is queued (std RwLock is writer-preferring on Linux).
pub fn collect_services(state: &AppState) -> Vec<String> {
    state.read(|s| {
        let is_service = |name: &str| {
            s.org_repos
                .iter()
                .any(|r| r.kind == RepoKind::Service && r.name == name)
        };
        let mut names: BTreeSet<String> = s.gitops_services.iter().cloned().collect();
        for env in s.environments.values() {
            for hr in &env.helm_releases {
                if is_service(&hr.name) {
                    names.insert(hr.name.clone());
                }
            }
        }
        let is_component = |name: &str| {
            s.operator_pins
                .values()
                .any(|pins| pins.components.contains_key(name))
        };
        // A base directory with no env overlay and no release (apps/base/public)
        // is not a deployed app.
        let deployed = |name: &str| {
            s.declared_versions.contains_key(name)
                || s.environments
                    .values()
                    .any(|e| e.helm_releases.iter().any(|h| h.name == name))
        };
        names.retain(|n| !s.hidden_repos.contains(n) && !is_component(n) && deployed(n));
        names.into_iter().collect()
    })
}

/// Helm releases that are not services and not fleet instances.
/// Single state read, see [`collect_services`] for why.
pub fn collect_infrastructure(state: &AppState) -> Vec<String> {
    state.read(|s| {
        let is_service = |name: &str| {
            s.gitops_services.iter().any(|g| g == name)
                || (!s.org_repos.is_empty()
                    && s.org_repos
                        .iter()
                        .any(|r| r.kind == RepoKind::Service && r.name == name))
        };
        let mut names = BTreeSet::new();
        for env in s.environments.values() {
            for hr in &env.helm_releases {
                if !is_service(&hr.name)
                    && !hr.name.starts_with("project-")
                    && !s.hidden_repos.contains(&hr.name)
                {
                    names.insert(hr.name.clone());
                }
            }
        }
        names.into_iter().collect()
    })
}

/// Split a container image reference into (name, tag).
/// e.g. "ghcr.io/notes-co/broker:pr-500@sha256:..." -> ("broker", "pr-500")
pub fn parse_image(image: &str) -> (String, String) {
    let r = ImageRef::parse(image);
    (r.name, r.tag.unwrap_or_else(|| "latest".to_string()))
}

/// Snapshot each active environment once, instead of cloning state per cell.
pub fn env_snapshots(
    state: &AppState,
    active_envs: &[&str],
) -> Vec<(String, Option<EnvironmentState>)> {
    active_envs
        .iter()
        .map(|&e| (e.to_string(), state.get_environment(e)))
        .collect()
}

/// State that is the same for every cell of a frame.
pub struct CellContext {
    pub gitops_prs: Vec<PullRequest>,
}

pub fn cell_context(state: &AppState) -> CellContext {
    CellContext {
        gitops_prs: state.get_open_gitops_prs(),
    }
}

/// The environment a cell is measured against for "behind".
pub fn upstream_env(env: &str) -> Option<String> {
    crate::models::environments::current()
        .upstream_of(env)
        .map(str::to_string)
}

/// The open gitops PR that releases a service to the reference environment,
/// if any. Titles look like "{release_pr_prefix}{service} v1.5.6" (config
/// `github.release_pr_prefix`).
pub fn release_pr_for<'a>(prs: &'a [PullRequest], service: &str) -> Option<&'a PullRequest> {
    let prefix = format!("{}{} v", crate::config::release_pr_prefix(), service);
    prs.iter()
        .find(|pr| pr.is_open() && pr.title.trim().starts_with(&prefix))
}

// ── Service cells ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// A newer version is on its way: text such as "→ v1.2.3 releasing"
    Releasing(String),
    /// This env runs older code than the upstream env
    Behind { n: i64, upstream: String },
    /// Commits ahead of prod
    Ahead(i64),
    /// The running tag is a pull request build; `open` is None until the
    /// PR has been looked up
    PrBuild { open: Option<bool> },
}

/// Everything one service cell shows, computed once per frame.
#[derive(Debug, Clone)]
pub struct CellInfo {
    pub hr: Option<HelmReleaseInfo>,
    pub declared: Option<DeclaredVersion>,
    pub version: String,
    pub release: Option<String>,
    pub markers: Vec<Marker>,
    /// (ready, total) pods in the release's namespace
    pub pods: Option<(usize, usize)>,
    pub ahead_of_prod: Option<i64>,
}

/// Split "tag@sha256:..." into (tag, digest).
fn split_digest(tag: &str) -> (&str, Option<&str>) {
    match tag.split_once('@') {
        Some((t, d)) => (t, Some(d)),
        None => (tag, None),
    }
}

/// The version app-gitops declares when it differs from what the
/// HelmRelease runs. Only compared when both sides carry the same kind of
/// value, so a missing tag never reads as a release in flight.
/// The tag app-gitops declares for a service: the one tag its nested image
/// pins share when it has any (a leftover top-level image.tag next to them
/// is ignored), else the top-level image.tag.
pub fn declared_effective_tag(d: &DeclaredVersion) -> Option<String> {
    let nested: Vec<&crate::state::DeclaredImage> = d
        .images
        .iter()
        .filter(|i| i.path != "values.image")
        .collect();
    if nested.is_empty() {
        return d.image_tag.clone();
    }
    // Different images never share a digest; the tag is the version
    let first = nested[0];
    let same = nested.iter().all(|i| i.bare_tag() == first.bare_tag());
    same.then(|| first.tag.clone())
}

pub fn declared_target(declared: Option<&DeclaredVersion>, hr: &HelmReleaseInfo) -> Option<String> {
    let d = declared?;
    let declared_tag = declared_effective_tag(d);
    if let (Some(dt), Some(rt)) = (declared_tag.as_deref(), hr.image_tag.as_deref()) {
        let (dtag, ddig) = split_digest(dt);
        let (rtag, rdig) = split_digest(rt);
        let same = match (ddig, rdig) {
            (Some(a), Some(b)) => digests_equal(a, b),
            _ => dtag == rtag,
        };
        return (!same).then(|| dtag.to_string());
    }
    if let (Some(dc), Some(rc)) = (d.chart_version.as_deref(), hr.chart_version.as_deref()) {
        if hr.image_tag.is_none() && dc != rc {
            return Some(dc.to_string());
        }
    }
    None
}

/// Order markers by importance and keep the two that matter most.
pub fn trim_markers(mut markers: Vec<Marker>) -> Vec<Marker> {
    let rank = |m: &Marker| match m {
        Marker::Releasing(_) => 0,
        Marker::Behind { .. } => 1,
        Marker::Ahead(_) => 2,
        Marker::PrBuild { .. } => 3,
    };
    markers.sort_by_key(rank);
    markers.truncate(2);
    markers
}

pub fn cell_info(
    state: &AppState,
    ctx: &CellContext,
    env: &EnvironmentState,
    service: &str,
    env_name: &str,
) -> Option<CellInfo> {
    let hr = env
        .helm_releases
        .iter()
        .find(|h| h.name == service)
        .cloned();
    let declared = state.get_declared_version(service, env_name);
    if hr.is_none() && declared.is_none() {
        return None;
    }
    let repo = &crate::fleet::repo_for_service(service);

    let (version, release) = match (&hr, &declared) {
        (Some(hr), _) => {
            let raw = widgets::hr_version_raw(hr);
            let release = state.find_release_for_service(
                repo,
                widgets::plain_sha(&raw).unwrap_or(""),
                hr.chart_version.as_deref(),
            );
            (widgets::version_text(&raw, release.as_deref()), release)
        }
        (None, Some(d)) => {
            let raw = d
                .image_tag
                .as_deref()
                .map(|t| split_digest(t).0.to_string())
                .or_else(|| d.chart_version.clone())
                .unwrap_or_else(|| "?".to_string());
            let release = state.find_release_for_service(
                repo,
                widgets::plain_sha(&raw).unwrap_or(""),
                d.chart_version.as_deref(),
            );
            (widgets::version_text(&raw, release.as_deref()), release)
        }
        (None, None) => unreachable!(),
    };

    let mut markers = Vec::new();
    if let Some(hr) = &hr {
        if let Some(target) = declared_target(declared.as_ref(), hr) {
            let text = widgets::resolved_version(state, repo, &target, None);
            markers.push(Marker::Releasing(format!(
                "{} {} releasing",
                icons::ARROW_RIGHT,
                text
            )));
        } else if hr.reconciling {
            markers.push(Marker::Releasing("reconciling".to_string()));
        }
    }
    let ladder = crate::models::environments::current();
    if ladder.is_reference(env_name) && !markers.iter().any(|m| matches!(m, Marker::Releasing(_))) {
        if let Some(pr) = release_pr_for(&ctx.gitops_prs, service) {
            markers.push(Marker::Releasing(format!(
                "{} PR #{} open",
                icons::ARROW_RIGHT,
                pr.number
            )));
        }
    }
    if let Some(up) = upstream_env(env_name) {
        if let Some(rel) = state.get_relation(service, &up, env_name) {
            if rel.behind > 0 {
                markers.push(Marker::Behind {
                    n: rel.behind,
                    upstream: up.to_string(),
                });
            }
        }
    }
    let mut ahead_of_prod = None;
    let reference = ladder.reference_id().unwrap_or_default().to_string();
    if !ladder.is_reference(env_name) {
        let ahead = state
            .get_relation(service, &reference, env_name)
            .map(|r| r.ahead)
            .or_else(|| state.get_ahead_of_prod(service, env_name));
        if let Some(a) = ahead {
            ahead_of_prod = Some(a);
            // The comparison only refreshes on the GitHub poll cycle; if the
            // running SHAs already match the badge is stale from a deploy
            // that just finished.
            if a > 0 && !state.envs_in_sync(service, env_name) {
                markers.push(Marker::Ahead(a));
            }
        }
    }
    if let Some(n) = hr
        .as_ref()
        .and_then(|h| h.image_tag.as_deref())
        .and_then(crate::models::image::pr_number)
    {
        let open = state.pr_build(repo, n).map(|b| b.open);
        markers.push(Marker::PrBuild { open });
    }

    let pods = hr.as_ref().map(|hr| {
        let ns = if hr.is_fleet_instance() {
            &hr.name
        } else {
            &hr.namespace
        };
        let in_ns: Vec<&PodInfo> = env.pods.iter().filter(|p| p.namespace == *ns).collect();
        (in_ns.iter().filter(|p| p.ready).count(), in_ns.len())
    });

    Some(CellInfo {
        hr,
        declared,
        version,
        release,
        markers: trim_markers(markers),
        pods,
        ahead_of_prod,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowGlyph {
    Releasing,
    Behind,
    Ahead,
    Ok,
    NoData,
}

impl RowGlyph {
    pub fn color(self, mode: ThemeMode) -> egui::Color32 {
        match self {
            RowGlyph::Releasing => Colors::BLUE,
            RowGlyph::Behind => Colors::RED,
            RowGlyph::Ahead => Colors::ORANGE,
            RowGlyph::Ok => Colors::GREEN,
            RowGlyph::NoData => Colors::text_muted(mode),
        }
    }

    /// Sort group: releasing and behind first, then ahead, then the rest.
    pub fn rank(self) -> u8 {
        match self {
            RowGlyph::Releasing | RowGlyph::Behind => 0,
            RowGlyph::Ahead => 1,
            RowGlyph::Ok | RowGlyph::NoData => 2,
        }
    }
}

pub fn glyph_of_markers<'a>(markers: impl Iterator<Item = &'a Marker>, any_data: bool) -> RowGlyph {
    let mut releasing = false;
    let mut behind = false;
    let mut ahead = false;
    for m in markers {
        match m {
            Marker::Releasing(_) => releasing = true,
            Marker::Behind { .. } => behind = true,
            Marker::Ahead(_) => ahead = true,
            Marker::PrBuild { .. } => {}
        }
    }
    if releasing {
        RowGlyph::Releasing
    } else if behind {
        RowGlyph::Behind
    } else if ahead {
        RowGlyph::Ahead
    } else if any_data {
        RowGlyph::Ok
    } else {
        RowGlyph::NoData
    }
}

#[derive(Debug, Clone)]
pub struct ServiceRow {
    pub name: String,
    pub glyph: RowGlyph,
    /// One entry per active environment
    pub cells: Vec<Option<CellInfo>>,
}

/// Rows with releasing or behind first, then ahead, then the rest,
/// alphabetical within each group.
pub fn sort_rows(rows: &mut [ServiceRow]) {
    rows.sort_by(|a, b| {
        a.glyph
            .rank()
            .cmp(&b.glyph.rank())
            .then_with(|| a.name.cmp(&b.name))
    });
}

// ── Projects: pins and rollout ─────────────────────────────────────────────

/// The `spec.values` key that carries a component's image in a project
/// HelmRelease.
/// The configured operator, when the gitops layout tracks one.
fn operator() -> Option<crate::gitops::overlay::OperatorConfig> {
    crate::config::installed().and_then(|c| c.gitops.overlay.operator.clone())
}

fn image_key_suffix(op: Option<&crate::gitops::overlay::OperatorConfig>) -> String {
    op.map(|o| o.image_key_suffix.clone())
        .unwrap_or_else(|| "_image".to_string())
}

pub fn values_key(component: &str) -> String {
    let op = operator();
    if let Some(op) = &op {
        if op.primary_component.as_deref() == Some(component) {
            if let Some(key) = &op.primary_image_key {
                return key.clone();
            }
        }
    }
    format!("{component}{}", image_key_suffix(op.as_ref()))
}

/// The component name a `spec.values` image key belongs to.
pub fn component_of_key(key: &str) -> Option<String> {
    let op = operator();
    if let Some(op) = &op {
        if Some(key) == op.primary_image_key.as_deref() {
            return op.primary_component.clone();
        }
    }
    key.strip_suffix(image_key_suffix(op.as_ref()).as_str())
        .map(|s| s.to_string())
}

/// One project instance measured against a component pin.
#[derive(Debug, Clone)]
pub struct InstanceCheck {
    pub instance: String,
    pub on_pin: bool,
    pub reconciling: bool,
    /// What the instance runs, as a tag or short digest
    pub running: Option<String>,
    /// Why it is not on the pin (empty when it is)
    pub reason: String,
    /// Reason or status of a problematic pod in the instance namespace
    pub pod_reason: Option<String>,
    pub since: Option<DateTime<Utc>>,
}

/// Whether a project HelmRelease is on a component pin: its values carry the
/// pin, it is Ready with its generation observed, and (when awake) the pod
/// container running that image reports the pin's digest.
pub fn check_instance(
    component: &str,
    pin: &ImageRef,
    hr: &HelmReleaseInfo,
    pods: &[PodInfo],
) -> InstanceCheck {
    let ns_pods: Vec<&PodInfo> = pods.iter().filter(|p| p.namespace == hr.name).collect();
    let pod_reason = ns_pods
        .iter()
        .find(|p| p.is_problematic())
        .map(|p| p.reason.clone().unwrap_or_else(|| p.status.clone()));
    let mut check = InstanceCheck {
        instance: hr.name.clone(),
        on_pin: false,
        reconciling: hr.reconciling,
        running: None,
        reason: String::new(),
        pod_reason,
        since: hr.last_transition_time,
    };

    // The container actually running this image, when the instance has pods
    let running_container = ns_pods
        .iter()
        .flat_map(|p| p.containers.iter())
        .find(|c| !c.init && container_image_name(c) == pin.name);
    if let Some(c) = running_container {
        let r = ImageRef::parse(&c.image);
        check.running = r
            .tag
            .clone()
            .or_else(|| r.digest.as_deref().map(widgets::digest_short));
    }
    // Images mounted as volumes (rogues) have no container status; the
    // pod spec reference is what runs.
    let mounted_volume = if running_container.is_none() {
        ns_pods
            .iter()
            .flat_map(|p| p.volume_images.iter())
            .map(|v| ImageRef::parse(v))
            .find(|r| r.name == pin.name)
    } else {
        None
    };
    if let Some(r) = &mounted_volume {
        check.running = r
            .tag
            .clone()
            .or_else(|| r.digest.as_deref().map(widgets::digest_short));
    }

    let key = values_key(component);
    let value = hr.values_images.get(&key).or_else(|| {
        (Some(component) == crate::fleet::multi_image_repo())
            .then(|| hr.values_images.get(&format!("{component}_image")))
            .flatten()
    });
    let Some(value) = value else {
        check.reason = format!("no {} in values", key);
        return check;
    };
    let values_ref = ImageRef::parse(value);
    let values_name = values_ref
        .tag
        .clone()
        .or_else(|| values_ref.digest.as_deref().map(widgets::digest_short));
    if check.running.is_none() {
        check.running = values_name.clone();
    }
    let values_match = match (&pin.digest, &values_ref.digest) {
        (Some(p), Some(r)) => digests_equal(p, r),
        _ => pin.tag.is_some() && pin.tag == values_ref.tag,
    };
    if !values_match {
        check.reason = format!(
            "values carry {}",
            values_name.unwrap_or_else(|| "nothing".to_string())
        );
        return check;
    }
    if hr.ready != Some(true) {
        check.reason = "not ready".to_string();
        return check;
    }
    if let (Some(g), Some(o)) = (hr.generation, hr.observed_generation) {
        if g != o {
            check.reason = format!("generation {} observed {}", g, o);
            return check;
        }
    }
    let awake = hr.replica_count.map_or(!ns_pods.is_empty(), |r| r > 0);
    if awake {
        if let (Some(pin_digest), Some(c)) = (&pin.digest, running_container) {
            if let Some(d) = ImageRef::parse(&c.image_id).digest {
                if !digests_equal(&d, pin_digest) {
                    check.reason = format!("pod runs {}", widgets::digest_short(&d));
                    return check;
                }
            }
        }
        if let Some(r) = &mounted_volume {
            let same = match (&pin.digest, &r.digest) {
                (Some(p), Some(d)) => digests_equal(p, d),
                _ => pin.tag == r.tag,
            };
            if !same {
                check.reason = format!(
                    "pod mounts {}",
                    check.running.clone().unwrap_or_else(|| "?".to_string())
                );
                return check;
            }
        }
    }
    check.on_pin = true;
    check
}

/// How far a pin has rolled out across the instances of one environment.
#[derive(Debug, Clone, Default)]
pub struct Rollout {
    pub rolled: usize,
    pub rolling: usize,
    pub stuck: usize,
    pub total: usize,
    pub rolling_since: Option<DateTime<Utc>>,
    /// The instances not on the pin
    pub off: Vec<InstanceCheck>,
}

impl Rollout {
    pub fn complete(&self) -> bool {
        self.total > 0 && self.rolled == self.total
    }
}

pub fn rollout_of(checks: Vec<InstanceCheck>) -> Rollout {
    let mut r = Rollout {
        total: checks.len(),
        ..Default::default()
    };
    for c in checks {
        if c.on_pin {
            r.rolled += 1;
        } else if c.reconciling {
            r.rolling += 1;
            r.rolling_since = match (r.rolling_since, c.since) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            r.off.push(c);
        } else {
            r.stuck += 1;
            r.off.push(c);
        }
    }
    r
}

/// Fleet counts for one environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetEnv {
    pub total: usize,
    pub awake: usize,
    pub healthy: usize,
    pub reconciling: usize,
    pub failed: usize,
}

pub fn fleet_of(env: &EnvironmentState) -> FleetEnv {
    let mut f = FleetEnv::default();
    for hr in env.fleet_instances() {
        f.total += 1;
        if hr.replica_count.is_some_and(|r| r > 0) {
            f.awake += 1;
        }
        if hr.status == DeploymentStatus::Healthy {
            f.healthy += 1;
        }
        if hr.reconciling {
            f.reconciling += 1;
        }
        if hr.status == DeploymentStatus::Failed {
            f.failed += 1;
        }
    }
    f
}

/// Image names running in project pod containers.
pub fn running_image_names(env: &EnvironmentState) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for pod in env
        .pods
        .iter()
        .filter(|p| p.namespace.starts_with("project-"))
    {
        if pod.containers.is_empty() {
            for image in &pod.images {
                names.insert(ImageRef::parse(image).name);
            }
        } else {
            for c in &pod.containers {
                names.insert(container_image_name(c));
            }
        }
        for v in &pod.volume_images {
            names.insert(ImageRef::parse(v).name);
        }
    }
    names
}

/// Image repo name of a container, from the status image ID when the status
/// image is a bare digest.
pub fn container_image_name(c: &crate::models::deployment::ContainerStatusInfo) -> String {
    let from_image = ImageRef::parse(&c.image);
    if c.image.starts_with("sha256:") || from_image.name.is_empty() {
        ImageRef::parse(&c.image_id).name
    } else {
        from_image.name
    }
}

/// The pin pill: the best name its digest is known by, then its own tag when
/// that differs, e.g. "v0.17.1 · pr-612".
pub fn pin_pill_text(best: Option<&str>, own: &str) -> String {
    best.map(str::to_string).unwrap_or_else(|| own.to_string())
}

/// Component order: the fleet's multi-image repo first, then alphabetically.
pub fn component_order(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let set: BTreeSet<String> = names.into_iter().collect();
    let lead = crate::fleet::multi_image_repo();
    let mut out = Vec::new();
    if let Some(lead) = lead.filter(|l| set.contains(*l)) {
        out.push(lead.to_string());
    }
    out.extend(set.into_iter().filter(|n| Some(n.as_str()) != lead));
    out
}

#[derive(Debug, Clone)]
pub struct ComponentCell {
    pub pin: ImageRef,
    pub pill: String,
    pub rollout: Rollout,
    pub markers: Vec<Marker>,
}

#[derive(Debug, Clone)]
pub struct ComponentRow {
    pub name: String,
    pub cells: Vec<Option<ComponentCell>>,
}

#[derive(Debug, Clone)]
pub struct ProjectsData {
    pub components: Vec<ComponentRow>,
    /// Pinned components with no running container anywhere: name and the
    /// per-env pin text
    /// Chart version per env
    pub chart: Vec<Option<String>>,
    pub fleet: Vec<Option<FleetEnv>>,
}

/// The pin pill text for a component pin: digest-named when a better name
/// exists in any env, else the pin's own tag.
pub fn pin_text(state: &AppState, _component: &str, pin: &ImageRef) -> String {
    let repo = &crate::fleet::repo_for_image(&pin.name);
    let own = match (&pin.tag, &pin.digest) {
        (Some(t), _) => widgets::resolved_version(state, repo, t, None),
        (None, Some(d)) => widgets::digest_short(d),
        (None, None) => "?".to_string(),
    };
    let best = pin
        .digest
        .as_deref()
        .and_then(|d| state.best_name_for_digest(d))
        .filter(|b| pin.tag.as_deref() != Some(b.as_str()))
        .map(|b| widgets::resolved_version(state, repo, &b, None));
    pin_pill_text(best.as_deref(), &own)
}

fn component_markers(
    state: &AppState,
    ctx: &CellContext,
    component: &str,
    pin: &ImageRef,
    env_name: &str,
) -> Vec<Marker> {
    let mut markers = Vec::new();
    let ladder = crate::models::environments::current();
    let reference = ladder.reference_id().unwrap_or_default().to_string();
    if ladder.is_reference(env_name) {
        let repo = &crate::fleet::repo_for_image(&pin.name);
        let release_name = match crate::fleet::multi_image_repo() {
            Some(multi) if repo == multi => multi,
            _ => component,
        };
        if let Some(pr) = release_pr_for(&ctx.gitops_prs, release_name) {
            markers.push(Marker::Releasing(format!(
                "{} PR #{} open",
                icons::ARROW_RIGHT,
                pr.number
            )));
        }
    }
    // The same image digest in two envs is the same build whatever the tags
    // say, so no commit distance applies.
    let pins = state.component_pins();
    let same_image_as = |other_env: &str| {
        pins.get(component)
            .and_then(|by_env| by_env.get(other_env))
            .and_then(|o| o.digest.as_deref())
            .zip(pin.digest.as_deref())
            .is_some_and(|(o, d)| digests_equal(o, d))
    };
    if let Some(up) = upstream_env(env_name) {
        if !same_image_as(&up) {
            if let Some(rel) = state.get_relation(component, &up, env_name) {
                if rel.behind > 0 {
                    markers.push(Marker::Behind {
                        n: rel.behind,
                        upstream: up.to_string(),
                    });
                }
            }
        }
    }
    if !ladder.is_reference(env_name) && !same_image_as(&reference) {
        if let Some(rel) = state.get_relation(component, &reference, env_name) {
            if rel.ahead > 0 {
                markers.push(Marker::Ahead(rel.ahead));
            }
        }
    }
    // A PR build whose image was since released is shown by its release
    // name, so the PR origin no longer matters.
    let released = pin
        .digest
        .as_deref()
        .and_then(|d| state.best_name_for_digest(d))
        .is_some_and(|b| {
            crate::models::image::tag_kind(&b) == crate::models::image::TagKind::Version
        });
    if let Some(n) = pin.tag.as_deref().and_then(crate::models::image::pr_number) {
        if !released {
            let repo = &crate::fleet::repo_for_image(&pin.name);
            let open = state.pr_build(repo, n).map(|b| b.open);
            markers.push(Marker::PrBuild { open });
        }
    }
    trim_markers(markers)
}

pub fn compute_projects(
    state: &AppState,
    ctx: &CellContext,
    envs: &[(String, Option<EnvironmentState>)],
) -> Option<ProjectsData> {
    let pins = state.component_pins();
    let operator_pins = state.get_operator_pins();
    let any_instances = envs
        .iter()
        .any(|(_, e)| e.as_ref().is_some_and(|e| !e.fleet_instances().is_empty()));
    if pins.is_empty() && !any_instances {
        return None;
    }

    let running: Vec<BTreeSet<String>> = envs
        .iter()
        .map(|(_, e)| e.as_ref().map(running_image_names).unwrap_or_default())
        .collect();

    let mut components = Vec::new();
    for name in component_order(pins.keys().cloned()) {
        let per_env = &pins[&name];
        let runs_somewhere = envs.iter().zip(&running).any(|((env_name, _), names)| {
            per_env
                .get(env_name)
                .is_some_and(|p| names.contains(&p.name))
        });
        // Pins nothing runs (build-only images) are not part of the fleet
        if !runs_somewhere {
            continue;
        }
        let cells = envs
            .iter()
            .map(|(env_name, env_state)| {
                let pin = per_env.get(env_name)?;
                let checks = env_state
                    .as_ref()
                    .map(|env| {
                        env.fleet_instances()
                            .into_iter()
                            .map(|hr| check_instance(&name, pin, hr, &env.pods))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Some(ComponentCell {
                    pin: pin.clone(),
                    pill: pin_text(state, &name, pin),
                    rollout: rollout_of(checks),
                    markers: component_markers(state, ctx, &name, pin, env_name),
                })
            })
            .collect();
        components.push(ComponentRow { name, cells });
    }

    let chart = envs
        .iter()
        .map(|(env_name, _)| {
            operator_pins
                .get(env_name)
                .and_then(|m| m.chart_version.as_deref())
                .map(widgets::shorten_hashes)
        })
        .collect();
    let fleet = envs.iter().map(|(_, e)| e.as_ref().map(fleet_of)).collect();

    Some(ProjectsData {
        components,
        chart,
        fleet,
    })
}

// ── The matrix ─────────────────────────────────────────────────────────────

pub struct MatrixData {
    pub envs: Vec<(String, Option<EnvironmentState>)>,
    pub rows: Vec<ServiceRow>,
    pub projects: Option<ProjectsData>,
}

/// Counts for the problems strip, over service rows and component rows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MatrixSummary {
    pub releasing: usize,
    pub behind: usize,
    pub ahead: usize,
}

pub fn compute_matrix(state: &AppState, services: &[String], active_envs: &[&str]) -> MatrixData {
    let envs = env_snapshots(state, active_envs);
    let ctx = cell_context(state);
    let mut rows: Vec<ServiceRow> = services
        .iter()
        .map(|name| {
            let cells: Vec<Option<CellInfo>> = envs
                .iter()
                .map(|(env_name, env_state)| {
                    env_state
                        .as_ref()
                        .and_then(|env| cell_info(state, &ctx, env, name, env_name))
                })
                .collect();
            let glyph = glyph_of_markers(
                cells.iter().flatten().flat_map(|c| c.markers.iter()),
                cells.iter().any(|c| c.is_some()),
            );
            ServiceRow {
                name: name.clone(),
                glyph,
                cells,
            }
        })
        .collect();
    sort_rows(&mut rows);
    let projects = compute_projects(state, &ctx, &envs);
    MatrixData {
        envs,
        rows,
        projects,
    }
}

pub fn summarize(data: &MatrixData) -> MatrixSummary {
    let mut s = MatrixSummary::default();
    let count = |s: &mut MatrixSummary, glyph: RowGlyph| match glyph {
        RowGlyph::Releasing => s.releasing += 1,
        RowGlyph::Behind => s.behind += 1,
        RowGlyph::Ahead => s.ahead += 1,
        _ => {}
    };
    for row in &data.rows {
        count(&mut s, row.glyph);
    }
    if let Some(p) = &data.projects {
        for row in &p.components {
            let glyph = glyph_of_markers(
                row.cells.iter().flatten().flat_map(|c| c.markers.iter()),
                true,
            );
            count(&mut s, glyph);
        }
    }
    s
}

/// The full matrix: header, one row per service, the projects section.
/// Returns a navigation request if something was clicked.
pub fn service_matrix(
    ui: &mut egui::Ui,
    data: &MatrixData,
    mode: ThemeMode,
    grid_id: &'static str,
) -> Option<View> {
    let mut nav_request: Option<View> = None;

    egui::Grid::new(grid_id)
        .num_columns(2 + data.envs.len())
        .spacing([16.0, 6.0])
        .min_col_width(100.0)
        .striped(false)
        .show(ui, |ui| {
            ui.label("");
            column_header(ui, mode, "APP");
            for (env_name, _) in &data.envs {
                ui.label(
                    egui::RichText::new(theme::env_label(env_name))
                        .size(11.0)
                        .strong()
                        .color(theme::env_color(env_name, mode)),
                );
            }
            ui.end_row();

            for row in &data.rows {
                glyph_dot(ui, row.glyph.color(mode));
                let name_resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(row.name.as_str())
                            .size(13.0)
                            .strong()
                            .color(Colors::text_primary(mode)),
                    )
                    .sense(egui::Sense::click()),
                );
                if name_resp.clicked() {
                    nav_request = Some(View::AppDetail(row.name.clone()));
                }
                if name_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                for (i, (env_name, _)) in data.envs.iter().enumerate() {
                    render_cell(ui, row.cells[i].as_ref(), env_name, mode);
                }
                ui.end_row();
            }
        });

    nav_request
}

fn column_header(ui: &mut egui::Ui, mode: ThemeMode, label: &str) {
    ui.label(
        egui::RichText::new(label)
            .size(11.0)
            .strong()
            .color(Colors::text_muted(mode)),
    );
}

fn dash(ui: &mut egui::Ui, mode: ThemeMode) {
    ui.label(
        egui::RichText::new(icons::MINUS.to_string())
            .size(10.0)
            .color(Colors::text_muted(mode)),
    );
}

/// The small colored dot in the glyph column.
fn glyph_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 16.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn marker_pill(ui: &mut egui::Ui, marker: &Marker) {
    match marker {
        Marker::Releasing(text) => {
            theme::status_pill(ui, text, Colors::BLUE);
        }
        Marker::Behind { n, upstream } => {
            theme::status_pill(
                ui,
                &format!("{}{} behind {}", icons::ARROW_DOWN, n, upstream),
                Colors::RED,
            )
            .on_hover_text(format!(
                "{} commit{} behind {}",
                n,
                if *n == 1 { "" } else { "s" },
                theme::env_label(upstream)
            ));
        }
        Marker::Ahead(n) => {
            theme::status_pill(ui, &format!("{}{}", icons::ARROW_UP, n), Colors::ORANGE)
                .on_hover_text(format!(
                    "{} commit{} ahead of Prod",
                    n,
                    if *n == 1 { "" } else { "s" }
                ));
        }
        Marker::PrBuild { open } => {
            let (text, hover) = match open {
                Some(true) => (
                    "open PR",
                    "Built from a pull request that is still open; the arrow counts its commits over prod",
                ),
                Some(false) => (
                    "merged PR",
                    "Built from a pull request branch that has since merged",
                ),
                None => ("PR build", "Built from a pull request branch"),
            };
            theme::status_pill(ui, text, Colors::GRAY).on_hover_text(hover);
        }
    }
}

/// One environment cell for a service on the Infrastructure page, computed
/// on the spot.
pub fn service_cell(
    ui: &mut egui::Ui,
    state: &AppState,
    ctx: &CellContext,
    env_state: &Option<EnvironmentState>,
    service_name: &str,
    env_name: &str,
    mode: ThemeMode,
) {
    let info = env_state
        .as_ref()
        .and_then(|env| cell_info(state, ctx, env, service_name, env_name));
    render_cell(ui, info.as_ref(), env_name, mode);
}

fn render_cell(ui: &mut egui::Ui, info: Option<&CellInfo>, env_name: &str, mode: ThemeMode) {
    // Everything in this cell must be in one horizontal group so the grid
    // treats it as a single column.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let Some(info) = info else {
            dash(ui, mode);
            return;
        };
        let resp = match &info.hr {
            Some(hr) => {
                let color = widgets::status_color(&hr.status, mode);
                let label = format!("{} {}", widgets::status_icon_char(&hr.status), info.version);
                theme::status_pill(ui, &label, color)
            }
            None => {
                // Shipped as an OCI image pin, no HelmRelease to report on
                let label = format!("{} {}", icons::CIRCLE_CHECK, info.version);
                theme::status_pill(ui, &label, Colors::GREEN)
            }
        };
        for marker in &info.markers {
            marker_pill(ui, marker);
        }
        resp.on_hover_ui(|ui| cell_tooltip(ui, info, env_name, mode));
    });
}

fn tooltip_row(ui: &mut egui::Ui, mode: ThemeMode, key: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(key)
                .size(11.0)
                .color(Colors::text_muted(mode)),
        );
        add(ui);
    });
}

fn cell_tooltip(ui: &mut egui::Ui, info: &CellInfo, env_name: &str, mode: ThemeMode) {
    ui.set_min_width(220.0);
    let Some(hr) = &info.hr else {
        ui.label(
            egui::RichText::new("Deployed as an OCI image (pinned in app-gitops)")
                .size(11.0)
                .color(Colors::text_muted(mode)),
        );
        if let Some(d) = &info.declared {
            if let Some(tag) = &d.image_tag {
                ui.label(
                    egui::RichText::new(format!("Image tag: {}", widgets::version_text(tag, None)))
                        .size(11.0)
                        .monospace()
                        .color(Colors::text_secondary(mode)),
                );
            }
            if let Some(cv) = &d.chart_version {
                ui.label(
                    egui::RichText::new(format!("Chart: {}", widgets::shorten_hashes(cv)))
                        .size(11.0)
                        .monospace()
                        .color(Colors::text_secondary(mode)),
                );
            }
        }
        return;
    };
    ui.label(
        egui::RichText::new(&hr.name)
            .size(14.0)
            .strong()
            .color(Colors::text_primary(mode)),
    );
    ui.label(
        egui::RichText::new(&hr.namespace)
            .size(11.0)
            .color(Colors::text_muted(mode)),
    );
    ui.add_space(6.0);
    let status_color = widgets::status_color(&hr.status, mode);
    tooltip_row(ui, mode, "Status", |ui| {
        theme::status_pill(ui, &format!("{}", hr.status), status_color);
    });
    if let Some(ver) = &hr.chart_version {
        tooltip_row(ui, mode, "Chart", |ui| {
            ui.label(
                egui::RichText::new(widgets::shorten_hashes(ver))
                    .size(11.0)
                    .color(Colors::text_secondary(mode)),
            );
        });
    }
    if let Some(tag) = &hr.image_tag {
        tooltip_row(ui, mode, "Image", |ui| {
            ui.label(
                egui::RichText::new(widgets::version_text(tag, None))
                    .size(11.0)
                    .color(Colors::text_secondary(mode))
                    .monospace(),
            );
        });
    }
    // Charts with several images: one line per nested image pin
    for (path, value) in hr
        .values_images
        .iter()
        .filter(|(k, _)| k.as_str() != "image" && k.ends_with(".image"))
    {
        let label = path.trim_end_matches(".image").to_string();
        tooltip_row(ui, mode, &label, |ui| {
            ui.label(
                egui::RichText::new(widgets::version_text(value, None))
                    .size(11.0)
                    .color(Colors::text_secondary(mode))
                    .monospace(),
            );
        });
    }
    if let Some(tag) = &info.release {
        tooltip_row(ui, mode, "Release", |ui| {
            ui.label(egui::RichText::new(tag).size(11.0).color(Colors::GREEN));
        });
    }
    if let Some(d) = &info.declared {
        if let Some(target) = declared_target(Some(d), hr) {
            tooltip_row(ui, mode, "Declared", |ui| {
                ui.label(
                    egui::RichText::new(widgets::version_text(&target, None))
                        .size(11.0)
                        .color(Colors::BLUE)
                        .monospace(),
                );
            });
        }
    }
    if !crate::models::environments::current().is_reference(env_name) {
        if let Some(ahead) = info.ahead_of_prod {
            if ahead > 0 {
                tooltip_row(ui, mode, "vs Prod", |ui| {
                    ui.label(
                        egui::RichText::new(format!("{} commits ahead", ahead))
                            .size(11.0)
                            .color(Colors::ORANGE),
                    );
                });
            }
        }
    }
    if let Some(msg) = &hr.status_message {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(msg)
                .size(11.0)
                .color(Colors::text_muted(mode)),
        );
    }
    if let Some((ready, total)) = info.pods {
        if total > 0 {
            ui.add_space(4.0);
            let pod_color = if ready == total {
                Colors::GREEN
            } else {
                Colors::YELLOW
            };
            tooltip_row(ui, mode, "Pods", |ui| {
                theme::status_pill(ui, &format!("{}/{}", ready, total), pod_color);
            });
        }
    }
}

// ── Projects section rendering ─────────────────────────────────────────────

/// The projects table: the fleet as the first row, then one row per
/// operator-pinned component with its per-environment pin and rollout.
pub fn projects_table(
    ui: &mut egui::Ui,
    data: &MatrixData,
    mode: ThemeMode,
    grid_id: &'static str,
) -> Option<View> {
    #[allow(unused_mut)]
    let mut nav_request: Option<View> = None;
    let Some(projects) = &data.projects else {
        return None;
    };
    let envs = &data.envs;

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            egui::RichText::new("Instances")
                .size(11.0)
                .strong()
                .color(Colors::text_muted(mode)),
        )
        .on_hover_text(
            "Instances the operator manages per environment: count, awake (not paused), health",
        );
        for ((env_name, _), fleet) in envs.iter().zip(&projects.fleet) {
            let Some(f) = fleet.as_ref().filter(|f| f.total > 0) else {
                continue;
            };
            let health = if f.healthy == f.total {
                "all healthy".to_string()
            } else {
                format!("{} healthy", f.healthy)
            };
            let label = format!(
                "{} {} · {} awake · {}",
                theme::env_label(env_name),
                f.total,
                f.awake,
                health
            );
            let resp = theme::status_pill(ui, &label, theme::env_color(env_name, mode))
                .interact(egui::Sense::click());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                ui.ctx().data_mut(|d| {
                    d.insert_temp(egui::Id::new(PENDING_PROJECTS_ENV), env_name.clone())
                });
            }
            if f.reconciling > 0 {
                theme::status_pill(ui, &format!("{} reconciling", f.reconciling), Colors::BLUE);
            }
            if f.failed > 0 {
                theme::status_pill(ui, &format!("{} failed", f.failed), Colors::RED);
            }
            ui.add_space(6.0);
        }
    });
    ui.add_space(8.0);

    egui::Grid::new(grid_id)
        .num_columns(2 + envs.len())
        .spacing([16.0, 6.0])
        .min_col_width(100.0)
        .striped(false)
        .show(ui, |ui| {
            ui.label("");
            column_header(ui, mode, "COMPONENT");
            for (env_name, _) in envs {
                ui.label(
                    egui::RichText::new(theme::env_label(env_name))
                        .size(11.0)
                        .strong()
                        .color(theme::env_color(env_name, mode)),
                );
            }
            ui.end_row();

            for row in &projects.components {
                let glyph = component_glyph(row);
                glyph_dot(ui, glyph.color(mode));
                ui.label(
                    egui::RichText::new(&row.name)
                        .size(13.0)
                        .strong()
                        .color(Colors::text_primary(mode)),
                );
                for cell in &row.cells {
                    component_cell(ui, cell.as_ref(), mode);
                }
                ui.end_row();
            }

            glyph_dot(ui, RowGlyph::NoData.color(mode));
            ui.label(
                egui::RichText::new("chart")
                    .size(13.0)
                    .color(Colors::text_secondary(mode)),
            );
            for version in &projects.chart {
                match version {
                    Some(v) => {
                        theme::status_pill(ui, v, Colors::text_secondary(mode));
                    }
                    None => dash(ui, mode),
                }
            }
            ui.end_row();
        });

    nav_request
}

/// Row dot for a component: blue while any env rolls, red when any env has
/// an instance stuck off the pin, green when every env is fully rolled.
fn component_glyph(row: &ComponentRow) -> RowGlyph {
    let cells: Vec<&ComponentCell> = row.cells.iter().flatten().collect();
    if cells.is_empty() {
        return RowGlyph::NoData;
    }
    let marker_glyph = glyph_of_markers(cells.iter().flat_map(|c| c.markers.iter()), true);
    if marker_glyph != RowGlyph::Ok {
        return marker_glyph;
    }
    if cells.iter().any(|c| c.rollout.rolling > 0) {
        RowGlyph::Releasing
    } else if cells.iter().any(|c| c.rollout.stuck > 0) {
        RowGlyph::Behind
    } else {
        RowGlyph::Ok
    }
}

fn component_cell(ui: &mut egui::Ui, cell: Option<&ComponentCell>, mode: ThemeMode) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let Some(cell) = cell else {
            dash(ui, mode);
            return;
        };
        let color = if cell.rollout.complete() {
            Colors::GREEN
        } else if cell.rollout.rolling > 0 {
            Colors::BLUE
        } else if cell.rollout.stuck > 0 {
            Colors::RED
        } else {
            Colors::text_secondary(mode)
        };
        theme::status_pill(ui, &cell.pill, color).on_hover_ui(|ui| {
            ui.label(
                egui::RichText::new(&cell.pin.reference)
                    .size(11.0)
                    .monospace()
                    .color(Colors::text_secondary(mode)),
            );
        });
        rollout_bar(ui, &cell.rollout, mode);
        for marker in &cell.markers {
            marker_pill(ui, marker);
        }
    });
}

/// An 84x6 bar: green for instances on the pin, blue for ones reconciling,
/// red for ones stuck off it, plus the fraction and the rolling time.
fn rollout_bar(ui: &mut egui::Ui, r: &Rollout, mode: ThemeMode) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(84.0, 6.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, Colors::bg_elevated(mode));
    if r.total > 0 {
        let unit = rect.width() / r.total as f32;
        let mut x = rect.left();
        for (count, color) in [
            (r.rolled, Colors::GREEN),
            (r.rolling, Colors::BLUE),
            (r.stuck, Colors::RED),
        ] {
            if count == 0 {
                continue;
            }
            let w = unit * count as f32;
            let seg =
                egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(w, rect.height()));
            // Round only the bar's outer ends; inner joins stay square
            let left = if x <= rect.left() + 0.5 { 3 } else { 0 };
            let right = if x + w >= rect.right() - 0.5 { 3 } else { 0 };
            let corners = egui::CornerRadius {
                nw: left,
                sw: left,
                ne: right,
                se: right,
            };
            painter.rect_filled(seg, corners, color);
            x += w;
        }
    }
    let text_color = if r.complete() {
        Colors::text_muted(mode)
    } else if r.rolling > 0 {
        Colors::BLUE
    } else if r.stuck > 0 {
        Colors::RED
    } else {
        Colors::text_muted(mode)
    };
    ui.label(
        egui::RichText::new(format!("{}/{}", r.rolled, r.total))
            .size(10.0)
            .color(text_color),
    );
    if r.rolling > 0 {
        if let Some(since) = r.rolling_since {
            ui.label(
                egui::RichText::new(format!(
                    "rolling {}",
                    widgets::elapsed_short(Utc::now() - since)
                ))
                .size(10.0)
                .color(Colors::BLUE),
            );
        }
    }
    if !r.off.is_empty() {
        resp.on_hover_ui(|ui| {
            ui.label(
                egui::RichText::new("Not on the pin")
                    .size(11.0)
                    .strong()
                    .color(Colors::text_primary(mode)),
            );
            for c in &r.off {
                let color = if c.reconciling {
                    Colors::BLUE
                } else {
                    Colors::RED
                };
                ui.label(
                    egui::RichText::new(format!(
                        "{}  {}{}",
                        c.instance.strip_prefix("project-").unwrap_or(&c.instance),
                        c.reason,
                        c.pod_reason
                            .as_ref()
                            .map(|r| format!(" ({})", r))
                            .unwrap_or_default()
                    ))
                    .size(11.0)
                    .monospace()
                    .color(color),
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::deployment::ContainerStatusInfo;
    use crate::state::OperatorPins;
    use std::collections::BTreeMap;

    const PIN_DIGEST: &str =
        "sha256:f07e8119b33a266da781329f427b020649104de660c2a181dab12bed28d0441a";
    const OLD_DIGEST: &str =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    fn pin() -> ImageRef {
        ImageRef::parse(&format!("ghcr.io/notes-co/broker:pr-612@{}", PIN_DIGEST))
    }

    fn hr(name: &str, values: &str) -> HelmReleaseInfo {
        let mut values_images = BTreeMap::new();
        values_images.insert("broker_image".to_string(), values.to_string());
        HelmReleaseInfo {
            name: name.to_string(),
            namespace: "operator".to_string(),
            status: DeploymentStatus::Healthy,
            status_message: None,
            chart_version: None,
            image_tag: None,
            chart_name: Some("engine".to_string()),
            last_attempted_revision: None,
            last_applied_revision: None,
            last_transition_time: Some(Utc::now() - chrono::Duration::minutes(4)),
            suspended: false,
            app_name_label: None,
            kustomize_name: None,
            ready: Some(true),
            reconciling: false,
            generation: Some(3),
            observed_generation: Some(3),
            replica_count: Some(1),
            values_images,
        }
    }

    fn pod(ns: &str, image_digest: &str) -> PodInfo {
        PodInfo {
            name: format!("{}-0", ns),
            namespace: ns.to_string(),
            phase: "Running".to_string(),
            status: "Running".to_string(),
            restarts: 0,
            created: None,
            started_at: None,
            ready: true,
            ready_count: "1/1".to_string(),
            images: vec!["ghcr.io/notes-co/broker:pr-612".to_string()],
            node_name: None,
            reason: None,
            cpu_request_millicores: None,
            cpu_limit_millicores: None,
            memory_request_bytes: None,
            memory_limit_bytes: None,
            volume_images: vec![],
            containers: vec![ContainerStatusInfo {
                name: "broker".to_string(),
                image: "ghcr.io/notes-co/broker:pr-612".to_string(),
                image_id: format!("ghcr.io/notes-co/broker@{}", image_digest),
                init: false,
                ready: true,
            }],
        }
    }

    fn on_pin_values() -> String {
        format!("ghcr.io/notes-co/broker:pr-612@{}", PIN_DIGEST)
    }

    #[test]
    fn instance_on_pin() {
        let h = hr("project-a", &on_pin_values());
        let c = check_instance("broker", &pin(), &h, &[pod("project-a", PIN_DIGEST)]);
        assert!(c.on_pin, "{}", c.reason);
        assert_eq!(c.running.as_deref(), Some("pr-612"));
    }

    #[test]
    fn instance_reconciling_counts_as_rolling() {
        let mut h = hr("project-b", "ghcr.io/notes-co/broker:pr-600@sha256:abcd");
        h.reconciling = true;
        h.ready = Some(false);
        let c = check_instance("broker", &pin(), &h, &[]);
        assert!(!c.on_pin);
        assert!(c.reconciling);
        assert_eq!(c.reason, "values carry pr-600");
    }

    #[test]
    fn instance_not_ready_or_unobserved_is_off_pin() {
        let mut h = hr("project-c", &on_pin_values());
        h.ready = Some(false);
        assert_eq!(
            check_instance("broker", &pin(), &h, &[]).reason,
            "not ready"
        );
        let mut h = hr("project-c", &on_pin_values());
        h.observed_generation = Some(2);
        assert_eq!(
            check_instance("broker", &pin(), &h, &[]).reason,
            "generation 3 observed 2"
        );
    }

    #[test]
    fn awake_instance_needs_matching_pod_digest() {
        let h = hr("project-d", &on_pin_values());
        let stale = check_instance("broker", &pin(), &h, &[pod("project-d", OLD_DIGEST)]);
        assert!(!stale.on_pin);
        assert_eq!(stale.reason, "pod runs 00000000");
        let fresh = check_instance("broker", &pin(), &h, &[pod("project-d", PIN_DIGEST)]);
        assert!(fresh.on_pin);
        // Asleep: no pods, the values alone decide
        let mut asleep = hr("project-e", &on_pin_values());
        asleep.replica_count = Some(0);
        let c = check_instance("broker", &pin(), &asleep, &[pod("project-e", OLD_DIGEST)]);
        assert!(c.on_pin);
    }

    #[test]
    fn rollout_fraction() {
        let on = hr("project-1", &on_pin_values());
        let mut rolling = hr("project-2", "ghcr.io/notes-co/broker:pr-600@sha256:abcd");
        rolling.reconciling = true;
        let mut stuck = hr("project-3", &on_pin_values());
        stuck.ready = Some(false);
        let checks: Vec<InstanceCheck> = [&on, &rolling, &stuck]
            .into_iter()
            .map(|h| check_instance("broker", &pin(), h, &[]))
            .collect();
        let r = rollout_of(checks);
        assert_eq!((r.rolled, r.rolling, r.stuck, r.total), (1, 1, 1, 3));
        assert!(!r.complete());
        assert!(r.rolling_since.is_some());
        assert_eq!(r.off.len(), 2);
        let all_on = rollout_of(vec![check_instance("broker", &pin(), &on, &[])]);
        assert!(all_on.complete());
    }

    #[test]
    fn rows_sort_releasing_and_behind_first() {
        let row = |name: &str, glyph: RowGlyph| ServiceRow {
            name: name.to_string(),
            glyph,
            cells: vec![],
        };
        let mut rows = vec![
            row("notes-worker", RowGlyph::Ok),
            row("notes-ui", RowGlyph::Ahead),
            row("notes-api", RowGlyph::NoData),
            row("engine", RowGlyph::Behind),
            row("notes-web", RowGlyph::Ok),
            row("alerts-mailer", RowGlyph::Releasing),
        ];
        sort_rows(&mut rows);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "alerts-mailer",
                "engine",
                "notes-ui",
                "notes-api",
                "notes-web",
                "notes-worker"
            ]
        );
    }

    #[test]
    fn markers_are_trimmed_to_two_by_importance() {
        let m = trim_markers(vec![
            Marker::PrBuild { open: Some(true) },
            Marker::Ahead(3),
            Marker::Behind {
                n: 2,
                upstream: "staging".to_string(),
            },
        ]);
        assert_eq!(
            m,
            vec![
                Marker::Behind {
                    n: 2,
                    upstream: "staging".to_string()
                },
                Marker::Ahead(3)
            ]
        );
    }

    #[test]
    fn pr_pin_is_named_by_its_digest() {
        let state = AppState::new();
        let mut prod = OperatorPins::default();
        prod.components.insert(
            "engine".to_string(),
            ImageRef::parse(&format!("ghcr.io/notes-co/engine:v0.17.1@{}", PIN_DIGEST)),
        );
        let mut dev = OperatorPins::default();
        let jy_pin = ImageRef::parse(&format!("ghcr.io/notes-co/engine:pr-612@{}", PIN_DIGEST));
        dev.components.insert("engine".to_string(), jy_pin.clone());
        state.write(|s| {
            s.operator_pins.insert("prod".to_string(), prod);
            s.operator_pins.insert("dev".to_string(), dev);
        });
        assert_eq!(pin_text(&state, "engine", &jy_pin), "v0.17.1");
        assert_eq!(pin_pill_text(Some("v0.17.1"), "v0.17.1"), "v0.17.1");
        assert_eq!(pin_pill_text(None, "PR #612"), "PR #612");
        assert_eq!(pin_pill_text(Some("v0.17.1"), "PR #612"), "v0.17.1");
    }

    #[test]
    fn component_order_and_keys() {
        assert_eq!(
            component_order(
                ["sqld", "broker", "mailer", "engine"]
                    .into_iter()
                    .map(String::from)
            ),
            ["broker", "engine", "mailer", "sqld"]
        );
        // Without an operator in config every component uses "<name>_image".
        assert_eq!(values_key("engine"), "engine_image");
        assert_eq!(values_key("sqld"), "sqld_image");
        assert_eq!(component_of_key("target_image").as_deref(), Some("target"));
        assert_eq!(component_of_key("broker_image").as_deref(), Some("broker"));
        assert_eq!(component_of_key("replicaCount"), None);
    }

    #[test]
    fn declared_target_only_compares_like_with_like() {
        let mut h = hr("notes-api", "");
        h.image_tag = Some("sha-aaaa".to_string());
        let d = DeclaredVersion {
            image_tag: Some("sha-bbbb".to_string()),
            ..Default::default()
        };
        assert_eq!(declared_target(Some(&d), &h).as_deref(), Some("sha-bbbb"));
        let same = DeclaredVersion {
            image_tag: Some("sha-aaaa@sha256:1".to_string()),
            ..Default::default()
        };
        assert_eq!(declared_target(Some(&same), &h), None);
        h.image_tag = None;
        assert_eq!(declared_target(Some(&d), &h), None);
    }
}
