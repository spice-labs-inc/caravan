//! Banner shown above every view while any vault environment is sealed, or
//! while an unseal run's result waits to be dismissed.

use crate::actions::unseal_keys::KeySource;
use crate::actions::vault::{self as vault_actions, KeyInput, UnsealTrigger, VaultUnsealRun};
use crate::config::Config;
use crate::state::AppState;
use crate::ui::icons;
use crate::ui::sidebar::View;
use crate::ui::theme::{Colors, ThemeMode};
use crate::ui::widgets;
use eframe::egui;

/// egui temp-data key the banner sets so the Vault page opens that env's
/// manual key entry on arrival.
pub const OPEN_MANUAL_ID: &str = "vault_open_manual";

/// Per-env snapshot the banner renders from; collected in one state read.
struct EnvRow {
    env: String,
    sealed: usize,
    total: usize,
    sealed_since: Option<chrono::DateTime<chrono::Utc>>,
    key_source: KeySource,
    run: Option<VaultUnsealRun>,
}

/// Returns a navigation request when the banner is clicked through.
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    config: &Option<Config>,
    mode: ThemeMode,
    runtime: &tokio::runtime::Runtime,
) -> Option<View> {
    let mut rows: Vec<EnvRow> = state.read(|s| {
        let mut rows: Vec<EnvRow> = s
            .vault_states
            .iter()
            .map(|(env, v)| EnvRow {
                env: env.clone(),
                sealed: v.sealed_pod_count(),
                total: v.pods.len(),
                sealed_since: v.sealed_since,
                key_source: v.key_source,
                run: s.vault_ops.runs.get(env).cloned(),
            })
            .filter(|r| r.sealed > 0 || r.run.is_some())
            .collect();
        for (env, run) in &s.vault_ops.runs {
            if !rows.iter().any(|r| &r.env == env) {
                rows.push(EnvRow {
                    env: env.clone(),
                    sealed: 0,
                    total: 0,
                    sealed_since: None,
                    key_source: KeySource::Manual,
                    run: Some(run.clone()),
                });
            }
        }
        rows
    });
    if rows.is_empty() {
        return None;
    }
    let ladder = crate::models::environments::current();
    rows.sort_by_key(|r| ladder.rank(&r.env));

    let mut nav = None;
    for row in &rows {
        if let Some(v) = show_env_band(ui, row, state, config, mode, runtime) {
            nav = Some(v);
        }
    }
    if rows
        .iter()
        .any(|r| r.run.as_ref().is_some_and(|x| x.is_running()))
    {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }
    nav
}

fn show_env_band(
    ui: &mut egui::Ui,
    row: &EnvRow,
    state: &AppState,
    config: &Option<Config>,
    mode: ThemeMode,
    runtime: &tokio::runtime::Runtime,
) -> Option<View> {
    let mut nav = None;
    let resolved = row.sealed == 0 && row.run.as_ref().is_some_and(|r| r.succeeded());
    let band = if resolved { Colors::GREEN } else { Colors::RED };

    egui::Frame::default()
        .fill(band.linear_multiply(0.18))
        .stroke(egui::Stroke::new(1.0_f32, band))
        .inner_margin(egui::Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (icon, title) = if resolved {
                    (icons::LOCK_OPEN, format!("{} vault unsealed", row.env))
                } else {
                    (icons::LOCK, format!("{} vault sealed", row.env))
                };
                ui.label(icons::icon(icon, 16.0, band));
                ui.label(
                    egui::RichText::new(title)
                        .size(14.0)
                        .strong()
                        .color(Colors::text_primary(mode)),
                );
                if row.sealed > 0 {
                    let since = row
                        .sealed_since
                        .map(|t| format!("sealed {}", widgets::time_ago(&t)))
                        .unwrap_or_else(|| "sealed just now".to_string());
                    ui.label(
                        egui::RichText::new(format!(
                            "{} of {} pods \u{b7} {} \u{b7} keys: {}",
                            row.sealed,
                            row.total,
                            since,
                            row.key_source.label()
                        ))
                        .size(12.0)
                        .color(Colors::text_secondary(mode)),
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let running = row.run.as_ref().is_some_and(|r| r.is_running());
                    let finished = row.run.as_ref().is_some_and(|r| !r.is_running());
                    if finished && dismiss_button(ui, mode) {
                        state.dismiss_vault_run(&row.env);
                    }
                    if row.sealed > 0 && !running {
                        let label = format!("{} Unseal {}", icons::KEY, row.env);
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(label)
                                        .size(12.0)
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                )
                                .fill(band)
                                .corner_radius(4.0),
                            )
                            .clicked()
                        {
                            if row.key_source.is_automatic() {
                                start_unseal(state, config, &row.env, runtime);
                            } else {
                                ui.ctx().data_mut(|d| {
                                    d.insert_temp(egui::Id::new(OPEN_MANUAL_ID), row.env.clone())
                                });
                                nav = Some(View::Vault);
                            }
                        }
                    }
                });
            });

            if let Some(run) = &row.run {
                ui.add_space(6.0);
                show_run(ui, run, mode);
            }
        });
    nav
}

fn dismiss_button(ui: &mut egui::Ui, mode: ThemeMode) -> bool {
    ui.add(
        egui::Button::new(
            egui::RichText::new("Dismiss")
                .size(11.0)
                .color(Colors::text_secondary(mode)),
        )
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::new(1.0_f32, Colors::border(mode)))
        .corner_radius(4.0),
    )
    .clicked()
}

/// Per-pod progress and the outcome line of a run. Shared with the Vault page.
pub fn show_run(ui: &mut egui::Ui, run: &VaultUnsealRun, mode: ThemeMode) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 14.0;
        let (icon, color) = if run.is_running() {
            (icons::SPINNER, Colors::ACCENT)
        } else if run.succeeded() {
            (icons::CIRCLE_CHECK, Colors::GREEN)
        } else {
            (icons::TRIANGLE_EXCLAMATION, Colors::ORANGE)
        };
        ui.label(icons::icon(icon, 12.0, color));
        ui.label(
            egui::RichText::new(format!(
                "{} ({}, keys: {})",
                run.summary(),
                run.trigger.label(),
                run.source.label()
            ))
            .size(12.0)
            .color(color),
        );
        ui.label(
            egui::RichText::new(format!("started {}", widgets::time_ago(&run.started)))
                .size(11.0)
                .color(Colors::text_muted(mode)),
        );
    });
    if run.pods.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 18.0;
        for pod in &run.pods {
            let (state_text, color) = match (&pod.error, pod.sealed) {
                (Some(e), _) => (format!("failed: {}", e), Colors::RED),
                (None, Some(false)) => ("unsealed".to_string(), Colors::GREEN),
                (None, Some(true)) => ("sealed".to_string(), Colors::ORANGE),
                (None, None) if run.is_running() => {
                    ("waiting".to_string(), Colors::text_muted(mode))
                }
                (None, None) => ("no answer".to_string(), Colors::text_muted(mode)),
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(
                    egui::RichText::new(&pod.pod)
                        .size(11.0)
                        .monospace()
                        .color(Colors::text_primary(mode)),
                );
                ui.label(
                    egui::RichText::new(format!("{}/{}", pod.submitted, pod.threshold))
                        .size(11.0)
                        .color(Colors::text_secondary(mode)),
                );
                ui.label(egui::RichText::new(state_text).size(11.0).color(color));
            });
        }
    });
}

/// Start an unseal from the configured key source (command or keyring).
pub fn start_unseal(
    state: &AppState,
    config: &Option<Config>,
    env: &str,
    runtime: &tokio::runtime::Runtime,
) {
    let Some(cfg) = config.as_ref().and_then(|c| c.vault.get(env)).cloned() else {
        state.push_vault_event(format!("{}: no vault config", env));
        return;
    };
    let snapshot = state.read(|s| {
        s.vault_states.get(env).map(|v| {
            (
                v.kubeconfig_path.clone(),
                v.sealed_targets(),
                vault_actions::select_threshold(None, v.http_threshold()),
            )
        })
    });
    let Some((Some(kubeconfig), targets, fallback)) = snapshot else {
        state.push_vault_event(format!("{}: no vault kubeconfig configured", env));
        return;
    };
    if targets.is_empty() {
        return;
    }
    let remember_override = state.vault_remember_override(env);
    let notifications = config.as_ref().map(|c| c.notifications.clone());
    vault_actions::spawn_unseal(
        runtime.handle(),
        state.clone(),
        env.to_string(),
        kubeconfig,
        targets,
        fallback,
        KeyInput::Resolve {
            cfg,
            remember_override,
        },
        UnsealTrigger::Banner,
        false,
        notifications,
    );
}
