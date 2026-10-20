//! The Releases page: what is on its way to prod, what could go, and what
//! shipped.

use crate::actions::release::{self, Bump};
use crate::cache::{Cache, RepoKind};
use crate::config::Config;
use crate::models::image::ImageRef;
use crate::models::release::PullRequest;
use crate::pollers::gh_http::GhClient;
use crate::state::{AppState, EnvironmentState};
use crate::ui::app_table;
use crate::ui::charts;
use crate::ui::filters::GlobalFilters;
use crate::ui::icons;
use crate::ui::sidebar::View;
use crate::ui::theme::{self, Colors, ThemeMode};
use crate::ui::toast::Toaster;
use crate::ui::widgets;
use chrono::{DateTime, Duration, Utc};
use eframe::egui;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

/// How far back the Shipped section looks.
pub const SHIPPED_DAYS: i64 = 30;
/// Shipped rows shown before the "show all" toggle.
pub const SHIPPED_CAP: usize = 40;

/// Open app-gitops release PRs, longest open first.
pub fn in_flight(state: &AppState) -> Vec<PullRequest> {
    let mut prs = state.get_open_gitops_prs();
    prs.sort_by_key(|pr| pr.created_at);
    prs
}

/// One project component of a repo that is ahead on staging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentPin {
    pub component: String,
    pub staging: String,
    pub prod: String,
}

/// A repo whose staging deployment runs commits prod does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyRow {
    pub repo: String,
    pub staging: String,
    pub prod: String,
    pub ahead: i64,
    pub components: Vec<ComponentPin>,
    /// The staging commit a release would be tagged at: the newest deployed
    /// commit of the repo (the component furthest ahead of prod)
    pub target_sha: Option<String>,
}

/// The commit a component pin was built from, when it can be known.
fn pin_commit(state: &AppState, repo: &str, pin: &ImageRef) -> Option<String> {
    let tag = pin.tag.as_deref()?;
    let bare = tag.split('@').next().unwrap_or(tag);
    if let Some(sha) = bare.strip_prefix("sha-") {
        return Some(sha.to_string());
    }
    if let Some(n) = crate::models::image::pr_number(bare) {
        return state.pr_build(repo, n).map(|b| b.compare_sha().to_string());
    }
    if bare.len() >= 8 && bare.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(bare.to_string());
    }
    state.read(|s| {
        s.repo_tags.get(repo).and_then(|tags| {
            tags.iter()
                .find(|(t, _)| {
                    t == bare || t.trim_start_matches('v') == bare.trim_start_matches('v')
                })
                .map(|(_, sha)| sha.clone())
        })
    })
}

/// Services and project components whose staging commit is ahead of prod,
/// one row per repo, most commits ahead first.
pub fn ready_to_release(state: &AppState) -> Vec<ReadyRow> {
    let ladder = crate::models::environments::current();
    let (Some(reference), Some(source)) = (ladder.reference_id(), ladder.release_source()) else {
        return Vec::new();
    };
    let staging = state.get_environment(source);
    let prod = state.get_environment(reference);
    let hr_version = |env: &Option<EnvironmentState>, name: &str, repo: &str| {
        env.as_ref()
            .and_then(|e| e.helm_releases.iter().find(|h| h.name == name))
            .map(|h| widgets::version_label(state, repo, h))
            .unwrap_or_else(|| "?".to_string())
    };

    let mut rows: BTreeMap<String, ReadyRow> = BTreeMap::new();
    for name in app_table::collect_services(state) {
        let Some(rel) = state.get_relation(&name, reference, source) else {
            continue;
        };
        if rel.ahead <= 0 {
            continue;
        }
        let repo = crate::fleet::repo_for_service(&name);
        let sha = staging
            .as_ref()
            .and_then(|e| e.helm_releases.iter().find(|h| h.name == name))
            .and_then(|h| h.full_commit_sha());
        let row = rows.entry(repo.clone()).or_insert_with(|| ReadyRow {
            staging: hr_version(&staging, &name, &repo),
            prod: hr_version(&prod, &name, &repo),
            repo,
            ahead: 0,
            components: vec![],
            target_sha: None,
        });
        if rel.ahead >= row.ahead {
            row.ahead = rel.ahead;
            if sha.is_some() {
                row.target_sha = sha;
            }
        }
    }

    for (component, envs) in state.component_pins() {
        let Some(rel) = state.get_relation(&component, reference, source) else {
            continue;
        };
        if rel.ahead <= 0 {
            continue;
        }
        let repo = crate::fleet::repo_for_image(&component);
        let pin_of = |env: &str| {
            envs.get(env)
                .map(|p| pin_label(state, &repo, p))
                .unwrap_or_else(|| "?".to_string())
        };
        let pin = ComponentPin {
            component: component.clone(),
            staging: pin_of(source),
            prod: pin_of(reference),
        };
        let sha = envs.get(source).and_then(|p| pin_commit(state, &repo, p));
        let row = rows.entry(repo.clone()).or_insert_with(|| ReadyRow {
            repo: repo.clone(),
            staging: pin.staging.clone(),
            prod: pin.prod.clone(),
            ahead: 0,
            components: vec![],
            target_sha: None,
        });
        // The component named after its repo is the repo's own version
        if component == repo {
            row.staging = pin.staging.clone();
            row.prod = pin.prod.clone();
        }
        // Components share one repo; the one furthest ahead runs the newest
        // commit, which is where the release tag belongs
        if rel.ahead >= row.ahead {
            row.ahead = rel.ahead;
            if sha.is_some() {
                row.target_sha = sha;
            }
        }
        row.components.push(pin);
    }

    let mut out: Vec<ReadyRow> = rows.into_values().collect();
    for row in &mut out {
        let order = app_table::component_order(row.components.iter().map(|c| c.component.clone()));
        row.components
            .sort_by_key(|c| order.iter().position(|n| *n == c.component));
    }
    out.sort_by(|a, b| b.ahead.cmp(&a.ahead).then_with(|| a.repo.cmp(&b.repo)));
    out
}

/// A release published inside the window and the environments running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShippedRow {
    pub repo: String,
    pub tag: String,
    pub published_at: DateTime<Utc>,
    pub author: Option<String>,
    pub html_url: String,
    pub prerelease: bool,
    pub envs: Vec<String>,
}

/// Non-draft releases of the service repos and the multi-image repo from the last `days`
/// days, newest first.
pub fn shipped(state: &AppState, days: i64) -> Vec<ShippedRow> {
    let cutoff = Utc::now() - Duration::days(days);
    let mut repos: BTreeSet<String> = state
        .get_repo_names_by_kind(RepoKind::Service)
        .into_iter()
        .collect();
    if let Some(multi) = crate::fleet::multi_image_repo() {
        repos.insert(multi.to_string());
    }
    let running = running_versions(state);
    let mut rows: Vec<ShippedRow> = state
        .get_releases()
        .into_iter()
        .filter(|r| !r.draft && repos.contains(&r.repo))
        .filter_map(|r| {
            let published_at = r.published_at.unwrap_or(r.created_at);
            if published_at < cutoff {
                return None;
            }
            Some(ShippedRow {
                envs: envs_running(&running, &r.repo, &r.tag),
                repo: r.repo,
                tag: r.tag,
                published_at,
                author: r.author,
                html_url: r.html_url,
                prerelease: r.prerelease,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.published_at
            .cmp(&a.published_at)
            .then_with(|| a.repo.cmp(&b.repo))
    });
    rows
}

/// The version name each environment runs, per repo: the release resolved
/// for every service HelmRelease built from the repo, and the best digest
/// name of every component pin built from it.
pub fn running_versions(state: &AppState) -> BTreeMap<String, Vec<(String, String)>> {
    let mut out: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for env in env_order(&state.environment_names()) {
        let Some(e) = state.get_environment(&env) else {
            continue;
        };
        for hr in e.helm_releases.iter().filter(|h| !h.is_fleet_instance()) {
            let repo = &crate::fleet::repo_for_service(&hr.name);
            let sha = hr.full_commit_sha().unwrap_or_default();
            if let Some(tag) =
                state.find_release_for_service(repo, &sha, hr.chart_version.as_deref())
            {
                out.entry(repo.to_string())
                    .or_default()
                    .push((env.clone(), tag));
            }
        }
    }
    for (_component, envs) in state.component_pins() {
        for (env, pin) in envs {
            // The repo is named after the image, not the operator key (rogues
            // is pqc-rogues-gallery); a sha- tag resolves through its releases
            let repo = &crate::fleet::repo_for_image(&pin.name);
            let name = pin
                .digest
                .as_deref()
                .and_then(|d| state.best_name_for_digest(d))
                .or_else(|| {
                    pin.tag
                        .as_deref()
                        .map(|t| widgets::resolved_version(state, repo, t, None))
                });
            if let Some(name) = name {
                out.entry(repo.to_string()).or_default().push((env, name));
            }
        }
    }
    out
}

/// The environments whose running version of `repo` is `tag`, in env order.
pub fn envs_running(
    running: &BTreeMap<String, Vec<(String, String)>>,
    repo: &str,
    tag: &str,
) -> Vec<String> {
    let mut envs: Vec<String> = running
        .get(repo)
        .map(|versions| {
            versions
                .iter()
                .filter(|(_, name)| same_version(tag, name))
                .map(|(env, _)| env.clone())
                .collect()
        })
        .unwrap_or_default();
    envs.sort();
    envs.dedup();
    env_order(&envs)
}

/// Two names denote the same version when they differ only by a `v` prefix.
fn same_version(a: &str, b: &str) -> bool {
    a.strip_prefix('v').unwrap_or(a) == b.strip_prefix('v').unwrap_or(b)
}

fn env_rank(env: &str) -> usize {
    let ladder = crate::models::environments::current();
    let rank = ladder.rank(env);
    if rank >= ladder.len() {
        ladder.len() + 1
    } else {
        ladder.len() - rank
    }
}

/// Environments in pipeline order: the bottom rung first, the reference last,
/// then the rest.
fn env_order(envs: &[String]) -> Vec<String> {
    let mut out = envs.to_vec();
    out.sort_by(|a, b| env_rank(a).cmp(&env_rank(b)).then_with(|| a.cmp(b)));
    out
}

/// Version text for a component pin: its best digest name, else its tag,
/// resolved against the repo's releases.
fn pin_label(state: &AppState, repo: &str, pin: &ImageRef) -> String {
    let name = pin
        .digest
        .as_deref()
        .and_then(|d| state.best_name_for_digest(d))
        .or_else(|| pin.tag.clone());
    match name {
        Some(n) => widgets::resolved_version(state, repo, &n, None),
        None => pin
            .digest
            .as_deref()
            .map(widgets::digest_short)
            .unwrap_or_else(|| "?".to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    _filters: &GlobalFilters,
    mode: ThemeMode,
    config: &Option<Config>,
    cache: &Option<Arc<Cache>>,
    runtime: &tokio::runtime::Runtime,
    toaster: &mut Toaster,
) -> Option<View> {
    let in_flight = in_flight(state);
    let ready = ready_to_release(state);
    let shipped = shipped(state, SHIPPED_DAYS);
    let release_ctx = ReleaseCtx {
        state,
        config: config.as_ref(),
        cache,
        runtime,
    };

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Releases")
                .size(26.0)
                .color(Colors::text_primary(mode))
                .strong(),
        );
        if !in_flight.is_empty() {
            theme::status_pill(
                ui,
                &format!("{} in flight", in_flight.len()),
                Colors::ORANGE,
            );
        }
        if !ready.is_empty() {
            theme::status_pill(ui, &format!("{} ready", ready.len()), Colors::BLUE);
        }
    });
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("What is on its way to prod, what could go, and what shipped")
            .color(Colors::text_secondary(mode)),
    );

    theme::section_header(
        ui,
        mode,
        &format!("In flight to prod ({})", in_flight.len()),
    );
    if in_flight.is_empty() {
        theme::card(ui, mode, |ui| empty_note(ui, mode, "No release PR open"));
    }
    for pr in &in_flight {
        in_flight_card(ui, mode, pr);
    }

    theme::section_header(ui, mode, &format!("Ready to release ({})", ready.len()));
    theme::card(ui, mode, |ui| {
        if ready.is_empty() {
            empty_note(ui, mode, "Prod matches staging everywhere");
        } else {
            ready_grid(ui, mode, &ready, &release_ctx);
        }
    });
    show_release_modal(ui.ctx(), mode, &release_ctx, toaster);

    theme::section_header(
        ui,
        mode,
        &format!(
            "Shipped ({} in the last {} days)",
            shipped.len(),
            SHIPPED_DAYS
        ),
    );
    theme::card(ui, mode, |ui| {
        if shipped.is_empty() {
            empty_note(
                ui,
                mode,
                &format!("Nothing released in the last {} days", SHIPPED_DAYS),
            );
        } else {
            shipped_grid(ui, mode, &shipped);
        }
    });

    ui.add_space(8.0);
    let service_names = app_table::collect_services(state);
    widgets::collapsible_strip(
        ui,
        mode,
        "release_activity",
        "Release activity",
        None,
        false,
        |ui| {
            charts::deploy_heatmap(ui, state, &service_names, mode);
        },
    );

    None
}

fn empty_note(ui: &mut egui::Ui, mode: ThemeMode, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(12.0)
            .color(Colors::text_muted(mode)),
    );
}

fn link_label(ui: &mut egui::Ui, text: egui::RichText, url: &str, hover: &str) {
    let resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
    if resp.clicked() {
        let _ = open::that(url);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(hover);
}

fn in_flight_card(ui: &mut egui::Ui, mode: ThemeMode, pr: &PullRequest) {
    let age = Utc::now() - pr.created_at;
    let age_color = if age.num_hours() >= 24 {
        Colors::RED
    } else if age.num_hours() >= 1 {
        Colors::YELLOW
    } else {
        Colors::text_muted(mode)
    };
    theme::accent_card(ui, mode, Colors::ORANGE, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            match pr.release_service_name() {
                Some(service) => {
                    theme::status_pill(ui, service, Colors::BLUE);
                }
                None => {
                    ui.label(
                        egui::RichText::new(&pr.title)
                            .strong()
                            .color(Colors::text_primary(mode)),
                    );
                }
            }
            if let Some(version) = pr.release_version() {
                theme::status_pill(ui, version, Colors::PURPLE);
            }
            if let Some(ref author) = pr.author {
                ui.label(
                    egui::RichText::new(format!("by {}", author))
                        .size(12.0)
                        .color(Colors::text_muted(mode)),
                );
            }
            ui.label(
                egui::RichText::new(format!("opened {}", widgets::time_ago_short(age)))
                    .size(12.0)
                    .color(age_color),
            );
            link_label(
                ui,
                egui::RichText::new(format!("#{}", pr.number))
                    .monospace()
                    .size(12.0)
                    .color(Colors::text_muted(mode)),
                &pr.html_url,
                &pr.title,
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                link_label(
                    ui,
                    icons::icon(icons::ARROW_UP_RIGHT_FROM_SQUARE, 11.0, Colors::ACCENT),
                    &pr.html_url,
                    "Open the PR in the browser",
                );
                link_label(
                    ui,
                    egui::RichText::new("Merge on GitHub")
                        .size(12.0)
                        .strong()
                        .color(Colors::ACCENT),
                    &pr.html_url,
                    "Open the PR in the browser",
                );
            });
        });
    });
}

fn ready_grid(ui: &mut egui::Ui, mode: ThemeMode, rows: &[ReadyRow], rc: &ReleaseCtx) {
    let modal = shared_release(ui.ctx());
    let busy = modal.lock().map(|m| m.open).unwrap_or(false);
    let active = rc.state.run_mode().is_active() && rc.config.is_some();
    let ladder = crate::models::environments::current();
    let source = ladder.release_source().unwrap_or_default().to_string();
    let reference = ladder.reference_id().unwrap_or_default().to_string();
    let source_head = ladder.label(&source).to_uppercase();
    let reference_head = ladder.label(&reference).to_uppercase();
    egui::Grid::new(egui::Id::new("releases_ready"))
        .num_columns(5)
        .spacing([16.0, 8.0])
        .min_col_width(60.0)
        .show(ui, |ui| {
            for col in [
                "SERVICE",
                source_head.as_str(),
                reference_head.as_str(),
                "",
                "RELEASE",
            ] {
                ui.label(
                    egui::RichText::new(col)
                        .size(10.0)
                        .strong()
                        .color(Colors::text_muted(mode)),
                );
            }
            ui.end_row();
            for row in rows {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(&row.repo)
                            .size(13.0)
                            .strong()
                            .color(Colors::text_primary(mode)),
                    );
                    if !row.components.is_empty() {
                        let line = row
                            .components
                            .iter()
                            .map(|c| format!("{} {}", c.component, c.staging))
                            .collect::<Vec<_>>()
                            .join(" · ");
                        ui.label(
                            egui::RichText::new(line)
                                .size(11.0)
                                .color(Colors::text_muted(mode)),
                        );
                    }
                });
                theme::status_pill(ui, &row.staging, theme::env_color(&source, mode));
                theme::status_pill(ui, &row.prod, theme::env_color(&reference, mode));
                theme::status_pill(
                    ui,
                    &format!(
                        "{}{} commit{}",
                        icons::ARROW_UP,
                        row.ahead,
                        if row.ahead == 1 { "" } else { "s" }
                    ),
                    Colors::ORANGE,
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let latest = release::latest_release_tag(rc.state, &row.repo);
                    let enabled = active && !busy && row.target_sha.is_some();
                    for bump in Bump::ALL {
                        let version = release::next_version(latest.as_deref(), bump);
                        let label = format!("{} {}", bump.label(), version);
                        let btn = ui.add_enabled(
                            enabled,
                            egui::Button::new(
                                egui::RichText::new(label)
                                    .size(11.0)
                                    .color(Colors::text_primary(mode)),
                            )
                            .fill(Colors::bg_elevated(mode)),
                        );
                        let btn = if !active {
                            btn.on_disabled_hover_text("Only the active caravan can release")
                        } else if row.target_sha.is_none() {
                            btn.on_disabled_hover_text(
                                "No commit to tag: staging runs an unresolved build",
                            )
                        } else {
                            btn.on_hover_text(format!(
                                "Preview generated notes, then release {} {} at {}",
                                row.repo,
                                version,
                                row.target_sha
                                    .as_deref()
                                    .map(|s| &s[..s.len().min(8)])
                                    .unwrap_or("?")
                            ))
                        };
                        if btn.clicked() {
                            if let Some(sha) = &row.target_sha {
                                start_release(ui.ctx(), rc, &row.repo, &version, sha);
                            }
                        }
                    }
                });
                ui.end_row();
            }
        });
}

/// What the release buttons need to reach GitHub.
pub struct ReleaseCtx<'a> {
    pub state: &'a AppState,
    pub config: Option<&'a Config>,
    pub cache: &'a Option<Arc<Cache>>,
    pub runtime: &'a tokio::runtime::Runtime,
}

#[derive(Default)]
pub enum ReleasePhase {
    #[default]
    Idle,
    Fetching,
    Editing,
    Creating,
    Error(String),
}

/// The release preview modal: generated notes, editable, then one confirm.
#[derive(Default)]
pub struct ReleaseModal {
    pub open: bool,
    pub repo: String,
    pub tag: String,
    pub target: String,
    pub notes: String,
    pub phase: ReleasePhase,
    pub toasts: Vec<String>,
    pub refresh: bool,
}

pub type SharedRelease = Arc<Mutex<ReleaseModal>>;
pub const RELEASE_MODAL_ID: &str = "release_modal";

pub fn shared_release(ctx: &egui::Context) -> SharedRelease {
    ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<SharedRelease>(egui::Id::new(RELEASE_MODAL_ID))
            .clone()
    })
}

fn gh_client(rc: &ReleaseCtx) -> anyhow::Result<GhClient> {
    let config = rc
        .config
        .ok_or_else(|| anyhow::anyhow!("no configuration loaded"))?;
    let token = config
        .resolve_github_token()?
        .ok_or_else(|| anyhow::anyhow!("no GitHub token configured"))?;
    GhClient::new(token, rc.cache.clone(), rc.state.clone())
}

/// Open the modal and fetch the generated notes in the background.
fn start_release(ctx: &egui::Context, rc: &ReleaseCtx, repo: &str, tag: &str, target: &str) {
    let modal = shared_release(ctx);
    {
        let Ok(mut m) = modal.lock() else { return };
        if m.open {
            return;
        }
        *m = ReleaseModal {
            open: true,
            repo: repo.to_string(),
            tag: tag.to_string(),
            target: target.to_string(),
            phase: ReleasePhase::Fetching,
            ..Default::default()
        };
    }
    let org = rc.config.map(|c| c.github.org.clone()).unwrap_or_default();
    let client = gh_client(rc);
    let (repo, tag, target) = (repo.to_string(), tag.to_string(), target.to_string());
    let ctx = ctx.clone();
    rc.runtime.spawn(async move {
        let result = match client {
            Ok(client) => release::generate_notes(&client, &org, &repo, &tag, &target).await,
            Err(e) => Err(e),
        };
        if let Ok(mut m) = modal.lock() {
            match result {
                Ok(notes) => {
                    m.notes = notes;
                    m.phase = ReleasePhase::Editing;
                }
                Err(e) => m.phase = ReleasePhase::Error(format!("{:#}", e)),
            }
        }
        ctx.request_repaint();
    });
}

fn confirm_release(ctx: &egui::Context, rc: &ReleaseCtx, modal: &SharedRelease) {
    let (repo, tag, target, notes) = {
        let Ok(mut m) = modal.lock() else { return };
        if !matches!(m.phase, ReleasePhase::Editing) {
            return;
        }
        m.phase = ReleasePhase::Creating;
        (
            m.repo.clone(),
            m.tag.clone(),
            m.target.clone(),
            m.notes.clone(),
        )
    };
    let org = rc.config.map(|c| c.github.org.clone()).unwrap_or_default();
    let client = gh_client(rc);
    let modal = modal.clone();
    let ctx = ctx.clone();
    rc.runtime.spawn(async move {
        let result = match client {
            Ok(client) => {
                release::create_release(&client, &org, &repo, &tag, &target, &notes).await
            }
            Err(e) => Err(e),
        };
        if let Ok(mut m) = modal.lock() {
            match result {
                Ok(url) => {
                    tracing::info!(repo = %repo, tag = %tag, url = %url, "Release created");
                    m.toasts.push(format!("Released {} {} {}", repo, tag, url));
                    m.refresh = true;
                    m.open = false;
                    m.phase = ReleasePhase::Idle;
                }
                Err(e) => m.phase = ReleasePhase::Error(format!("{:#}", e)),
            }
        }
        ctx.request_repaint();
    });
}

fn show_release_modal(
    ctx: &egui::Context,
    mode: ThemeMode,
    rc: &ReleaseCtx,
    toaster: &mut Toaster,
) {
    let modal = shared_release(ctx);
    let (open, toasts, refresh) = {
        let Ok(mut m) = modal.lock() else { return };
        (
            m.open,
            std::mem::take(&mut m.toasts),
            std::mem::take(&mut m.refresh),
        )
    };
    for t in toasts {
        toaster.push("", t);
    }
    if refresh {
        rc.state.request_refresh();
    }
    if !open {
        return;
    }
    let (repo, tag, target, phase_label, error) = {
        let Ok(m) = modal.lock() else { return };
        let (label, error) = match &m.phase {
            ReleasePhase::Idle => ("", None),
            ReleasePhase::Fetching => ("generating notes…", None),
            ReleasePhase::Editing => ("", None),
            ReleasePhase::Creating => ("creating the release…", None),
            ReleasePhase::Error(e) => ("", Some(e.clone())),
        };
        (
            m.repo.clone(),
            m.tag.clone(),
            m.target.clone(),
            label,
            error,
        )
    };
    let short = &target[..target.len().min(8)];
    egui::Window::new(format!("Release {} {}", repo, tag))
        .id(egui::Id::new("release_modal_window"))
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new(format!(
                    "Tag {} at {} (what staging runs). The notes below are GitHub's generated notes; edit them before creating.",
                    tag, short
                ))
                .size(12.0)
                .color(Colors::text_secondary(mode)),
            );
            ui.add_space(8.0);
            if !phase_label.is_empty() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new(phase_label)
                            .size(12.0)
                            .color(Colors::BLUE),
                    );
                });
                ctx.request_repaint_after(std::time::Duration::from_millis(300));
            }
            if let Some(e) = &error {
                ui.label(egui::RichText::new(e).size(12.0).color(Colors::RED));
            }
            let editing = {
                modal
                    .lock()
                    .map(|m| matches!(m.phase, ReleasePhase::Editing | ReleasePhase::Error(_)))
                    .unwrap_or(false)
            };
            if editing {
                if let Ok(mut m) = modal.lock() {
                    egui::ScrollArea::vertical()
                        .max_height(420.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut m.notes)
                                    .desired_rows(18)
                                    .desired_width(f32::INFINITY)
                                    .font(egui::TextStyle::Monospace),
                            );
                        });
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    if let Ok(mut m) = modal.lock() {
                        m.open = false;
                        m.phase = ReleasePhase::Idle;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_create = editing;
                    let btn = ui.add_enabled(
                        can_create,
                        egui::Button::new(
                            egui::RichText::new(format!("Create release {}", tag))
                                .strong()
                                .color(Colors::text_primary(mode)),
                        )
                        .fill(Colors::GREEN.linear_multiply(0.35)),
                    );
                    if btn.clicked() {
                        if let Ok(mut m) = modal.lock() {
                            if matches!(m.phase, ReleasePhase::Error(_)) {
                                m.phase = ReleasePhase::Editing;
                            }
                        }
                        confirm_release(ctx, rc, &modal);
                    }
                });
            });
        });
}

fn shipped_grid(ui: &mut egui::Ui, mode: ThemeMode, rows: &[ShippedRow]) {
    let show_all_id = egui::Id::new("releases_shipped_show_all");
    let mut show_all: bool = ui
        .ctx()
        .data_mut(|d| d.get_temp(show_all_id).unwrap_or(false));
    let visible = if show_all {
        rows
    } else {
        &rows[..rows.len().min(SHIPPED_CAP)]
    };
    egui::Grid::new(egui::Id::new("releases_shipped"))
        .num_columns(5)
        .spacing([16.0, 6.0])
        .min_col_width(60.0)
        .striped(true)
        .show(ui, |ui| {
            for col in ["WHEN", "REPO", "RELEASE", "BY", "RUNNING IN"] {
                ui.label(
                    egui::RichText::new(col)
                        .size(10.0)
                        .strong()
                        .color(Colors::text_muted(mode)),
                );
            }
            ui.end_row();
            for row in visible {
                let local = row.published_at.with_timezone(&chrono::Local);
                ui.label(
                    egui::RichText::new(local.format("%b %-d").to_string())
                        .size(12.0)
                        .color(Colors::text_secondary(mode)),
                )
                .on_hover_text(format!(
                    "{} ({})",
                    local.format("%Y-%m-%d %H:%M %Z"),
                    widgets::time_ago(&row.published_at)
                ));
                ui.label(
                    egui::RichText::new(&row.repo)
                        .size(12.0)
                        .strong()
                        .color(Colors::text_primary(mode)),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    link_label(
                        ui,
                        egui::RichText::new(&row.tag)
                            .monospace()
                            .size(12.0)
                            .color(Colors::ACCENT),
                        &row.html_url,
                        "Open the release in the browser",
                    );
                    if row.prerelease {
                        theme::status_pill(ui, "pre-release", Colors::GRAY);
                    }
                });
                ui.label(
                    egui::RichText::new(row.author.as_deref().unwrap_or(""))
                        .size(12.0)
                        .color(Colors::text_muted(mode)),
                );
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for env in &row.envs {
                        theme::status_pill(ui, &theme::env_label(env), theme::env_color(env, mode));
                    }
                });
                ui.end_row();
            }
        });
    if rows.len() > SHIPPED_CAP {
        ui.add_space(6.0);
        let text = if show_all {
            format!("show the latest {}", SHIPPED_CAP)
        } else {
            format!("show all {}", rows.len())
        };
        let resp = ui.add(
            egui::Label::new(egui::RichText::new(text).size(12.0).color(Colors::ACCENT))
                .sense(egui::Sense::click()),
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            show_all = !show_all;
            ui.ctx().data_mut(|d| d.insert_temp(show_all_id, show_all));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::OrgRepo;
    use crate::models::deployment::HelmReleaseInfo;
    use crate::models::release::GitHubRelease;
    use crate::state::OperatorPins;
    use std::collections::HashMap;

    const PROD_SHA: &str = "2b5ba6be2808cadf5e0414f6fa817c76fd79b952";
    const STAGING_SHA: &str = "e3f01e86a467c48fed87c4de092ec00343b28c1f";
    const ENGINE_DIGEST: &str =
        "sha256:f07e8119b33a266da781329f427b020649104de660c2a181dab12bed28d0441a";
    const BROKER_DIGEST: &str =
        "sha256:7814242c7814242c7814242c7814242c7814242c7814242c7814242c7814242c";

    fn repo(name: &str, kind: RepoKind) -> OrgRepo {
        OrgRepo {
            name: name.to_string(),
            description: None,
            language: None,
            archived: false,
            fork: false,
            private: true,
            license: None,
            updated_at: None,
            pushed_at: None,
            kind,
        }
    }

    fn hr(name: &str, sha: &str) -> HelmReleaseInfo {
        HelmReleaseInfo {
            name: name.to_string(),
            namespace: name.to_string(),
            image_tag: Some(format!("sha-{}", sha)),
            chart_version: Some(format!("0.1.0-{}", sha)),
            ..Default::default()
        }
    }

    fn release(repo: &str, tag: &str, sha: &str, days_ago: i64, draft: bool) -> GitHubRelease {
        let at = Utc::now() - Duration::days(days_ago);
        GitHubRelease {
            repo: repo.to_string(),
            tag: tag.to_string(),
            name: None,
            body: None,
            draft,
            prerelease: false,
            author: Some("dani".to_string()),
            published_at: Some(at),
            created_at: at,
            html_url: format!("https://github.com/x/{}/releases/{}", repo, tag),
            id: days_ago as u64,
            target_commitish: sha.to_string(),
        }
    }

    fn pins(engine: &str, broker: &str) -> OperatorPins {
        let mut p = OperatorPins::default();
        p.components.insert(
            "engine".to_string(),
            ImageRef::parse(&format!("ghcr.io/notes-co/engine:{}", engine)),
        );
        p.components.insert(
            "broker".to_string(),
            ImageRef::parse(&format!("ghcr.io/notes-co/broker:{}", broker)),
        );
        p
    }

    fn fixture() -> AppState {
        let state = AppState::new();
        state.update_org_repos(vec![
            repo("notes-api", RepoKind::Service),
            repo("operator", RepoKind::Service),
            repo("engine", RepoKind::Service),
            repo("goatrodeo", RepoKind::Library),
        ]);
        state.update_environment(
            "staging",
            vec![hr("notes-api", STAGING_SHA), hr("operator", PROD_SHA)],
            vec![],
            None,
        );
        state.update_environment(
            "prod",
            vec![hr("notes-api", PROD_SHA), hr("operator", PROD_SHA)],
            vec![],
            None,
        );
        let mut operator = HashMap::new();
        operator.insert(
            "staging".to_string(),
            pins(
                &format!("pr-9@{}", ENGINE_DIGEST),
                &format!("v0.17.3@{}", BROKER_DIGEST),
            ),
        );
        operator.insert(
            "prod".to_string(),
            pins(
                &format!("v0.17.3@{}", ENGINE_DIGEST),
                &format!("v0.17.3@{}", BROKER_DIGEST),
            ),
        );
        state.update_gitops(
            vec!["notes-api".to_string(), "operator".to_string()],
            HashMap::new(),
            operator,
            None,
        );
        state.update_github(
            vec![],
            vec![],
            vec![
                release("notes-api", "v1.5.5", PROD_SHA, 2, false),
                release("notes-api", "v1.5.4", "0000000000", 40, false),
                release("notes-api", "v1.5.6", "3333333333", 1, true),
                release("engine", "v0.17.3", "1111111111", 3, false),
                release("goatrodeo", "v2.0.0", "2222222222", 1, false),
            ],
            vec![],
        );
        state
    }

    #[test]
    fn ready_is_empty_without_relations() {
        assert!(ready_to_release(&fixture()).is_empty());
    }

    #[test]
    fn shipped_matches_envs_through_releases_and_pins() {
        let rows = shipped(&fixture(), SHIPPED_DAYS);
        let tags: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.repo.as_str(), r.tag.as_str()))
            .collect();
        assert_eq!(tags, [("notes-api", "v1.5.5"), ("engine", "v0.17.3")]);
        assert_eq!(rows[0].envs, ["prod"]);
        assert_eq!(rows[1].envs, ["staging", "prod"]);
        assert_eq!(rows[0].author.as_deref(), Some("dani"));
    }

    #[test]
    fn env_pills_follow_pipeline_order_and_ignore_v_prefix() {
        let mut running = BTreeMap::new();
        running.insert(
            "notes-api".to_string(),
            vec![
                ("prod".to_string(), "v1.5.5".to_string()),
                ("dev".to_string(), "1.5.5".to_string()),
                ("staging".to_string(), "v1.5.6".to_string()),
            ],
        );
        assert_eq!(
            envs_running(&running, "notes-api", "v1.5.5"),
            ["dev", "prod"]
        );
        assert!(envs_running(&running, "operator", "v1.5.5").is_empty());
    }
}
