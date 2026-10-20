use crate::models::deployment::DeploymentStatus;
use crate::state::AppState;
use crate::ui::app_table;
use crate::ui::charts;
use crate::ui::filters::GlobalFilters;
use crate::ui::sidebar::View;
use crate::ui::theme::{self, Colors, ThemeMode};
use eframe::egui;

pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    filters: &GlobalFilters,
    mode: ThemeMode,
) -> Option<View> {
    ui.label(
        egui::RichText::new("Infrastructure")
            .size(26.0)
            .color(Colors::text_primary(mode))
            .strong(),
    );
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Platform services managed via infra-gitops")
            .color(Colors::text_secondary(mode)),
    );
    ui.add_space(16.0);

    let infra_services = app_table::collect_infrastructure(state);
    let active_envs = filters.active_envs();
    let cell_ctx = app_table::cell_context(state);
    let envs = app_table::env_snapshots(state, &active_envs);

    // ── Release Activity Heatmap ───────────────────────────────────────
    theme::section_header(ui, mode, "Release Activity");
    theme::card(ui, mode, |ui| {
        charts::deploy_heatmap(ui, state, &infra_services, mode);
    });
    ui.add_space(12.0);

    // ── Status summary ─────────────────────────────────────────────────
    let all_healthy = state.read(|s| {
        s.environments.values().all(|env| {
            infra_services.iter().all(|name| {
                env.helm_releases
                    .iter()
                    .find(|hr| hr.name == *name)
                    .is_none_or(|hr| hr.status == DeploymentStatus::Healthy)
            })
        })
    });

    let header = if all_healthy {
        format!(
            "Status — {} services, all healthy \u{f058}",
            infra_services.len()
        )
    } else {
        format!("Status — {} services", infra_services.len())
    };
    theme::section_header(ui, mode, &header);

    if infra_services.is_empty() {
        theme::card(ui, mode, |ui| {
            ui.label(
                egui::RichText::new("Waiting for first poll…")
                    .color(Colors::text_muted(mode))
                    .italics(),
            );
        });
        return None;
    }

    // ── Infra Matrix ───────────────────────────────────────────────────
    theme::card(ui, mode, |ui| {
        egui::Grid::new("infra_full_matrix")
            .num_columns(1 + active_envs.len())
            .spacing([16.0, 4.0])
            .min_col_width(100.0)
            .striped(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("SERVICE")
                        .size(11.0)
                        .strong()
                        .color(Colors::text_muted(mode)),
                );
                for &env_name in &active_envs {
                    ui.label(
                        egui::RichText::new(theme::env_label(env_name))
                            .size(11.0)
                            .strong()
                            .color(theme::env_color(env_name, mode)),
                    );
                }
                ui.end_row();

                for service_name in &infra_services {
                    ui.label(
                        egui::RichText::new(service_name.as_str())
                            .size(13.0)
                            .strong()
                            .color(Colors::text_primary(mode)),
                    );

                    for (env_name, env_state) in &envs {
                        app_table::service_cell(
                            ui,
                            state,
                            &cell_ctx,
                            env_state,
                            service_name,
                            env_name,
                            mode,
                        );
                    }
                    ui.end_row();
                }
            });
    });

    None
}
