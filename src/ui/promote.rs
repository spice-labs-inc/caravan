//! Promote modal (source to target environment from config): change-set preview with per-service
//! checkboxes, then commit and push from caravan's own scratch clone.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::actions::promote::{
    commit_message, run_promote, ChangeSet, PromoteError, ServiceChange,
};
use crate::config::Config;
use crate::gitops::{layout_from_config, GitopsLayout, MirrorTree};
use crate::gitops_mirror::Mirror;
use crate::state::AppState;
use crate::ui::icons;
use crate::ui::theme::{Colors, ThemeMode};
use crate::ui::toast::Toaster;

/// egui temp-data key holding the shared modal state.
pub const MODAL_ID: &str = "promote_modal";
const WINDOW_ID: &str = "promote_modal_window";

#[derive(Debug, Default)]
pub enum Phase {
    #[default]
    Idle,
    Computing,
    Ready(ChangeSet),
    /// A promote is in flight; the string is the current step
    Running(String),
    Error(String),
}

#[derive(Debug, Default)]
pub struct PromoteModal {
    pub open: bool,
    pub phase: Phase,
    pub selected: HashSet<String>,
    pub sync_after_push: bool,
    /// Set by the worker after a successful push when the sync box was on
    pub sync_requested: bool,
    /// Messages for the toaster, drained by the UI thread
    pub toasts: Vec<String>,
}

pub type Shared = Arc<Mutex<PromoteModal>>;

pub fn shared(ctx: &egui::Context) -> Shared {
    ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<Shared>(egui::Id::new(MODAL_ID))
            .clone()
    })
}

/// Open the modal and start computing the change set in the background.
pub fn open(
    ctx: &egui::Context,
    state: &AppState,
    config: &Config,
    runtime: &tokio::runtime::Runtime,
) {
    let shared = shared(ctx);
    {
        let mut m = shared.lock().unwrap();
        m.open = true;
        m.sync_after_push = config.promote.sync_after_push;
        m.selected.clear();
    }
    spawn_compute(runtime, shared, state, config);
}

/// Services and operator components whose `to` build is ahead of `from`.
fn target_ahead(state: &AppState, from: &str, to: &str) -> HashMap<String, i64> {
    let mut names = state.get_gitops_services();
    let pins = state.get_operator_pins();
    for env_pins in pins.values() {
        names.extend(env_pins.components.keys().cloned());
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            let rel = state.get_relation(&name, from, to)?;
            (rel.ahead > 0).then_some((name, rel.ahead))
        })
        .collect()
}

fn spawn_compute(
    runtime: &tokio::runtime::Runtime,
    shared: Shared,
    state: &AppState,
    config: &Config,
) {
    shared.lock().unwrap().phase = Phase::Computing;
    let remote = config.github.app_gitops_remote.clone();
    let active = state.run_mode().is_active();
    let Some((from, to)) = config.promote.pair(&config.environments) else {
        shared.lock().unwrap().phase =
            Phase::Error("no promotion pair configured: set [promote] target".to_string());
        return;
    };
    let ahead = target_ahead(state, &from, &to);
    let layout = layout_from_config(&config.gitops);
    let branch = config.gitops.branch.clone();
    runtime.spawn_blocking(move || {
        let result = compute(&*layout, &branch, &remote, active, &from, &to, &ahead);
        let mut m = shared.lock().unwrap();
        match result {
            Ok(cs) => {
                m.selected = cs.services.iter().map(|s| s.name.clone()).collect();
                m.phase = Phase::Ready(cs);
            }
            Err(e) => m.phase = Phase::Error(format!("{e:#}")),
        }
    });
}

fn compute(
    layout: &dyn GitopsLayout,
    branch: &str,
    remote: &str,
    active: bool,
    from: &str,
    to: &str,
    ahead: &HashMap<String, i64>,
) -> anyhow::Result<ChangeSet> {
    if !layout.supports_promotion() {
        anyhow::bail!(
            "the {} gitops layout does not support promotion",
            layout.name()
        );
    }
    let data_dir = crate::config::data_dir();
    let Some(mirror) = Mirror::open_existing(&data_dir, remote) else {
        anyhow::bail!("no gitops mirror yet; wait for the first gitops poll");
    };
    if active {
        mirror.fetch()?;
    }
    let head = mirror.head_sha(branch)?;
    let tree = MirrorTree {
        mirror: &mirror,
        rev: branch,
    };
    let services = layout.promotable_services(&tree, from)?;
    layout.promotion(&tree, &head, from, to, &services, ahead)
}

fn spawn_promote(
    runtime: &tokio::runtime::Runtime,
    shared: Shared,
    state: &AppState,
    config: &Config,
    cs: ChangeSet,
    selected: HashSet<String>,
) {
    let message = commit_message(&cs, &selected);
    let remote = config.github.app_gitops_remote.clone();
    let branch = config.gitops.branch.clone();
    let active = state.run_mode().is_active();
    let (from, to) = (cs.from.clone(), cs.to.clone());
    let ahead = target_ahead(state, &from, &to);
    let layout = layout_from_config(&config.gitops);
    shared.lock().unwrap().phase = Phase::Running("cloning".to_string());
    runtime.spawn_blocking(move || {
        let data_dir = crate::config::data_dir();
        let Some(mirror) = Mirror::open_existing(&data_dir, &remote) else {
            shared.lock().unwrap().phase = Phase::Error("gitops mirror is gone".to_string());
            return;
        };
        let set_phase = |p: &str| shared.lock().unwrap().phase = Phase::Running(p.to_string());
        match run_promote(&mirror, &branch, &cs, &selected, &message, &set_phase) {
            Ok(outcome) => {
                set_phase("syncing");
                if active {
                    if let Err(e) = mirror.fetch() {
                        tracing::warn!(err = %e, "Mirror fetch after promote failed");
                    }
                }
                let mut m = shared.lock().unwrap();
                m.toasts.push(format!(
                    "Promoted {} to {} @ {}",
                    outcome.changed_files.len(),
                    cs.to,
                    short(&outcome.sha)
                ));
                m.sync_requested = m.sync_after_push;
                m.open = false;
                m.phase = Phase::Idle;
            }
            Err(e) => {
                let rejected = matches!(
                    e.downcast_ref::<PromoteError>(),
                    Some(PromoteError::PushRejected(_))
                );
                if rejected {
                    shared
                        .lock()
                        .unwrap()
                        .toasts
                        .push("main moved, recomputing".to_string());
                    shared.lock().unwrap().phase = Phase::Computing;
                    let result = compute(&*layout, &branch, &remote, active, &from, &to, &ahead);
                    let mut m = shared.lock().unwrap();
                    match result {
                        Ok(cs) => {
                            m.selected = cs.services.iter().map(|s| s.name.clone()).collect();
                            m.phase = Phase::Ready(cs);
                        }
                        Err(e) => m.phase = Phase::Error(format!("{e:#}")),
                    }
                } else {
                    shared.lock().unwrap().phase = Phase::Error(format!("{e:#}"));
                }
            }
        }
    });
}

fn short(s: &str) -> String {
    s.chars().take(8).collect()
}

/// Shorten hashes inside a value for display: a 40+ hex run or a sha256
/// digest becomes its first 8 characters.
fn short_value(v: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    let bytes = v.as_bytes();
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i - start >= 32 {
            out.push_str(&v[start..start + 8]);
        } else if i > start {
            out.push_str(&v[start..i]);
        } else {
            let c = v[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// Render the modal (a no-op unless open), drain worker toasts, and kick off
/// the post-push target sync when the worker asked for it.
pub fn show(
    ctx: &egui::Context,
    state: &AppState,
    config: &Config,
    mode: ThemeMode,
    runtime: &tokio::runtime::Runtime,
    toaster: &mut Toaster,
) {
    let shared = shared(ctx);
    let (open, toasts, sync_requested, busy) = {
        let mut m = shared.lock().unwrap();
        let toasts = std::mem::take(&mut m.toasts);
        let sync = std::mem::take(&mut m.sync_requested);
        let busy = matches!(m.phase, Phase::Computing | Phase::Running(_));
        (m.open, toasts, sync, busy)
    };
    for t in toasts {
        toaster.push("", t);
    }
    if sync_requested {
        if let Some((_, to)) = config.promote.pair(&config.environments) {
            if let Some(env) = config.environments.get(&to) {
                crate::actions::flux::spawn_sync(runtime, to.clone(), env.clone(), state.clone());
            }
        }
    }
    if !open {
        return;
    }
    if busy {
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
    }

    let mut action = Action::None;
    let modal = egui::Modal::new(egui::Id::new(WINDOW_ID))
        .frame(
            egui::Frame::window(&ctx.style())
                .fill(Colors::bg_surface(mode))
                .inner_margin(20.0),
        )
        .show(ctx, |ui| {
            ui.set_width(760.0_f32);
            ui.set_max_height(ctx.screen_rect().height() * 0.85_f32);
            ui.label(
                egui::RichText::new(promote_title(config))
                    .size(18.0_f32)
                    .strong()
                    .color(Colors::text_primary(mode)),
            );
            let mut m = shared.lock().unwrap();
            let PromoteModal {
                phase,
                selected,
                sync_after_push,
                ..
            } = &mut *m;
            match phase {
                Phase::Idle | Phase::Computing => {
                    ui.add_space(8.0_f32);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new("fetching app-gitops main and comparing pins")
                                .color(Colors::text_secondary(mode)),
                        );
                    });
                    ui.add_space(8.0_f32);
                    if ui.button("Cancel").clicked() {
                        action = Action::Close;
                    }
                }
                Phase::Running(phase) => {
                    ui.add_space(8.0_f32);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new(format!("{phase}..."))
                                .color(Colors::text_secondary(mode)),
                        );
                    });
                }
                Phase::Error(err) => {
                    ui.add_space(8.0_f32);
                    ui.label(egui::RichText::new(err.as_str()).color(Colors::RED));
                    ui.add_space(8.0_f32);
                    ui.horizontal(|ui| {
                        if ui.button("Retry").clicked() {
                            action = Action::Recompute;
                        }
                        if ui.button("Close").clicked() {
                            action = Action::Close;
                        }
                    });
                }
                Phase::Ready(cs) => {
                    show_ready(ui, mode, cs, selected, sync_after_push, &mut action);
                }
            }
        });
    if modal.should_close() && !busy {
        action = Action::Close;
    }

    match action {
        Action::None => {}
        Action::Close => shared.lock().unwrap().open = false,
        Action::Recompute => spawn_compute(runtime, shared.clone(), state, config),
        Action::Promote => {
            let (cs, selected) = {
                let m = shared.lock().unwrap();
                match &m.phase {
                    Phase::Ready(cs) => (cs.clone(), m.selected.clone()),
                    _ => return,
                }
            };
            spawn_promote(runtime, shared.clone(), state, config, cs, selected);
        }
    }
}

enum Action {
    None,
    Close,
    Recompute,
    Promote,
}

fn show_ready(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    cs: &ChangeSet,
    selected: &mut HashSet<String>,
    sync_after_push: &mut bool,
    action: &mut Action,
) {
    ui.label(
        egui::RichText::new(format!(
            "fresh clone of gitops @ {} · {} services change, {} already match. {}-only settings are kept.",
            short(&cs.base_sha),
            cs.services.len(),
            cs.unchanged.len(),
            crate::ui::theme::env_label(&cs.to)
        ))
        .size(12.0_f32)
        .color(Colors::text_secondary(mode)),
    );
    ui.add_space(10.0_f32);

    egui::ScrollArea::vertical()
        .max_height(ui.available_height() - 90.0_f32)
        .show(ui, |ui| {
            for svc in &cs.services {
                service_row(ui, mode, svc, cs, selected);
                ui.add_space(6.0_f32);
            }
            if !cs.unchanged.is_empty() {
                ui.add_space(4.0_f32);
                ui.label(
                    egui::RichText::new(format!(
                        "already matching {}: {}",
                        crate::ui::theme::env_label(&cs.from),
                        cs.unchanged.join(", ")
                    ))
                    .size(12.0_f32)
                    .color(Colors::text_muted(mode)),
                );
            }
        });

    ui.add_space(10.0_f32);
    ui.separator();
    ui.add_space(6.0_f32);
    let n = selected.len();
    ui.label(
        egui::RichText::new(commit_message(cs, selected))
            .size(12.0_f32)
            .monospace()
            .color(Colors::text_secondary(mode)),
    );
    ui.add_space(6.0_f32);
    ui.horizontal(|ui| {
        ui.checkbox(
            sync_after_push,
            format!("sync {} after push", crate::ui::theme::env_label(&cs.to)),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = match n {
                1 => "Promote 1 service and push".to_string(),
                _ => format!("Promote {n} services and push"),
            };
            let button = egui::Button::new(
                egui::RichText::new(label)
                    .color(egui::Color32::WHITE)
                    .strong(),
            )
            .fill(Colors::ORANGE);
            if ui.add_enabled(n > 0, button).clicked() {
                *action = Action::Promote;
            }
            if ui.button("Cancel").clicked() {
                *action = Action::Close;
            }
        });
    });
}

fn service_row(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    svc: &ServiceChange,
    cs: &ChangeSet,
    selected: &mut HashSet<String>,
) {
    egui::Frame::NONE
        .fill(Colors::bg_elevated(mode))
        .corner_radius(6.0_f32)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let mut on = selected.contains(&svc.name);
                if ui.checkbox(&mut on, "").changed() {
                    if on {
                        selected.insert(svc.name.clone());
                    } else {
                        selected.remove(&svc.name);
                    }
                }
                ui.label(
                    egui::RichText::new(&svc.name)
                        .size(14.0_f32)
                        .strong()
                        .color(Colors::text_primary(mode)),
                );
                if !svc.components.is_empty() {
                    ui.label(
                        egui::RichText::new(svc.components.join(", "))
                            .size(12.0_f32)
                            .color(Colors::text_muted(mode)),
                    );
                }
                if let Some(w) = &svc.warning {
                    warning_pill(ui, w);
                }
            });
            ui.add_space(4.0_f32);
            egui::Grid::new(("promote_edits", &svc.name))
                .num_columns(4)
                .spacing([16.0_f32, 2.0_f32])
                .show(ui, |ui| {
                    for e in &svc.edits {
                        ui.label(
                            egui::RichText::new(&e.path)
                                .size(12.0_f32)
                                .monospace()
                                .color(Colors::text_secondary(mode)),
                        );
                        ui.label(
                            egui::RichText::new(short_value(&e.old))
                                .size(12.0_f32)
                                .monospace()
                                .strikethrough()
                                .color(Colors::text_muted(mode)),
                        )
                        .on_hover_text(&e.old);
                        ui.label(
                            egui::RichText::new(short_value(&e.new))
                                .size(12.0_f32)
                                .monospace()
                                .color(Colors::GREEN),
                        )
                        .on_hover_text(&e.new);
                        ui.label(
                            egui::RichText::new(if e.same_image {
                                "same image, tag only"
                            } else {
                                ""
                            })
                            .size(11.0_f32)
                            .color(Colors::text_muted(mode)),
                        );
                        ui.end_row();
                    }
                });
            if cs.operator.as_deref() == Some(svc.name.as_str()) {
                let kept = if cs.operator_target_only.is_empty() {
                    "none".to_string()
                } else {
                    cs.operator_target_only.join(", ")
                };
                let to = crate::ui::theme::env_label(&cs.to);
                let text = if svc.edits.is_empty() {
                    format!("no pin differs · {to}-only keys kept: {kept}")
                } else {
                    format!("{to}-only keys kept: {kept}")
                };
                ui.label(
                    egui::RichText::new(text)
                        .size(11.0_f32)
                        .color(Colors::text_muted(mode)),
                );
            }
        });
}

fn warning_pill(ui: &mut egui::Ui, text: &str) {
    let color = Colors::ORANGE;
    egui::Frame::NONE
        .fill(color.linear_multiply(0.15_f32))
        .corner_radius(10.0_f32)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0_f32;
            ui.label(icons::icon(icons::ARROW_UP, 10.0_f32, color));
            ui.label(egui::RichText::new(text).size(11.0_f32).color(color));
        });
}

/// "Source → Target" from the configured promotion pair.
fn promote_title(config: &Config) -> String {
    match config.promote.pair(&config.environments) {
        Some((from, to)) => format!(
            "{} → {}",
            crate::ui::theme::env_label(&from),
            crate::ui::theme::env_label(&to)
        ),
        None => "Promote".to_string(),
    }
}
