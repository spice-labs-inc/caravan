//! The Status home: problems strip and the service matrix with the projects
//! section.

use crate::state::{AppState, Problem, Severity};
use crate::ui::app_table::{self, MatrixSummary};
use crate::ui::filters::GlobalFilters;
use crate::ui::icons;
use crate::ui::sidebar::View;
use crate::ui::theme::{self, Colors, ThemeMode};
use crate::ui::widgets;
use eframe::egui;

pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    filters: &GlobalFilters,
    mode: ThemeMode,
) -> Option<View> {
    let mut nav_request: Option<View> = None;

    let active_envs = filters.active_envs();
    let service_names = app_table::collect_services(state);
    let data = app_table::compute_matrix(state, &service_names, &active_envs);
    let summary = app_table::summarize(&data);
    let problems = filtered_problems(state, filters);
    let drift = applied_revision_notes(state, &data.envs);

    problems_strip(ui, mode, &problems, summary, &drift, filters);
    ui.add_space(8.0);

    if service_names.is_empty() && data.projects.is_none() {
        theme::card(ui, mode, |ui| {
            ui.label(
                egui::RichText::new("Waiting for first poll…")
                    .color(Colors::text_muted(mode))
                    .italics(),
            );
        });
    } else {
        theme::card(ui, mode, |ui| {
            nav_request = app_table::service_matrix(ui, &data, mode, "app_matrix");
        });
        if data.projects.is_some() {
            ui.add_space(12.0);
            theme::section_header(ui, mode, "Projects");
            theme::card(ui, mode, |ui| {
                if let Some(view) = app_table::projects_table(ui, &data, mode, "projects_table") {
                    nav_request = Some(view);
                }
            });
        }
    }
    nav_request
}

/// Problems in the visible environments inside the selected time period.
fn filtered_problems(state: &AppState, filters: &GlobalFilters) -> Vec<Problem> {
    let cutoff = filters.time_period.cutoff();
    state
        .get_problems()
        .into_iter()
        .filter(|p| filters.visible_envs.contains(&p.environment))
        .filter(|p| match (cutoff, p.timestamp) {
            (Some(cutoff), Some(ts)) => ts >= cutoff,
            _ => true,
        })
        .collect()
}

/// One note per environment whose applied Flux revision is not the mirror's
/// main HEAD.
fn applied_revision_notes(
    state: &AppState,
    envs: &[(String, Option<crate::state::EnvironmentState>)],
) -> Vec<String> {
    let Some(head) = state.get_gitops_head() else {
        return vec![];
    };
    envs.iter()
        .filter_map(|(env_name, env)| {
            let applied = env.as_ref()?.kustomization.as_ref()?.applied_sha.as_ref()?;
            if applied == &head.sha {
                return None;
            }
            Some(match state.commits_behind_main(env_name) {
                Some(n) if n > 0 => format!(
                    "{} applied revision is {} commit{} behind main",
                    env_name,
                    n,
                    if n == 1 { "" } else { "s" }
                ),
                _ => format!("{} applied revision differs from main", env_name),
            })
        })
        .collect()
}

fn problems_strip(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    problems: &[Problem],
    summary: MatrixSummary,
    drift: &[String],
    filters: &GlobalFilters,
) {
    let critical = problems
        .iter()
        .filter(|p| p.severity == Severity::Critical)
        .count();
    let warnings = problems
        .iter()
        .filter(|p| p.severity == Severity::Warning)
        .count();
    let info = problems
        .iter()
        .filter(|p| p.severity == Severity::Info)
        .count();

    let expanded_id = egui::Id::new("problems_strip_expanded");
    let mut expanded: bool = ui
        .ctx()
        .data_mut(|d| d.get_temp(expanded_id).unwrap_or(false));

    let mut line = vec![
        format!(
            "{} problem{}",
            problems.len(),
            if problems.len() == 1 { "" } else { "s" }
        ),
        format!("{} releasing", summary.releasing),
        format!("{} behind", summary.behind),
        format!("{} ahead of prod", summary.ahead),
    ];
    line.extend(drift.iter().cloned());

    let accent = if critical > 0 {
        Colors::RED
    } else if warnings > 0 {
        Colors::ORANGE
    } else {
        Colors::GREEN
    };
    theme::accent_card(ui, mode, accent, |ui| {
        let header = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let caret = if expanded {
                    icons::CHEVRON_DOWN
                } else {
                    icons::CHEVRON_RIGHT
                };
                ui.label(icons::icon(caret, 10.0, Colors::text_muted(mode)));
                if problems.is_empty() {
                    ui.label(icons::icon(icons::CIRCLE_CHECK, 13.0, Colors::GREEN));
                }
                if critical > 0 {
                    theme::status_pill(ui, &format!("{} critical", critical), Colors::RED);
                }
                if warnings > 0 {
                    theme::status_pill(ui, &format!("{} warning", warnings), Colors::ORANGE);
                }
                if info > 0 {
                    theme::status_pill(ui, &format!("{} info", info), Colors::GRAY);
                }
                ui.label(
                    egui::RichText::new(line.join(" · "))
                        .size(12.0)
                        .color(Colors::text_secondary(mode)),
                );
                ui.label(
                    egui::RichText::new(format!("last {}", filters.time_period.label()))
                        .size(11.0)
                        .color(Colors::text_muted(mode)),
                );
            })
            .response
            .interact(egui::Sense::click());
        if header.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if header.clicked() {
            expanded = !expanded;
            ui.ctx().data_mut(|d| d.insert_temp(expanded_id, expanded));
        }
        if expanded {
            ui.add_space(8.0);
            problems_list(ui, mode, problems);
        }
    });
}

fn problems_list(ui: &mut egui::Ui, mode: ThemeMode, problems: &[Problem]) {
    if problems.is_empty() {
        ui.label(
            egui::RichText::new("All clear across the visible environments")
                .size(12.0)
                .color(Colors::text_muted(mode)),
        );
        return;
    }
    for (severity, title, color) in [
        (Severity::Critical, "Critical", Colors::RED),
        (Severity::Warning, "Warning", Colors::ORANGE),
        (Severity::Info, "Info", Colors::GRAY),
    ] {
        let group: Vec<&Problem> = problems.iter().filter(|p| p.severity == severity).collect();
        if group.is_empty() {
            continue;
        }
        theme::section_header(ui, mode, title);
        for problem in group {
            problem_card(ui, mode, problem, color);
        }
    }
}

fn problem_card(ui: &mut egui::Ui, mode: ThemeMode, problem: &Problem, accent: egui::Color32) {
    theme::accent_card(ui, mode, accent, |ui| {
        ui.horizontal(|ui| {
            let (icon_ch, icon_color) = match problem.severity {
                Severity::Critical => (icons::CIRCLE_XMARK, Colors::RED),
                Severity::Warning => (icons::TRIANGLE_EXCLAMATION, Colors::YELLOW),
                Severity::Info => (icons::CIRCLE_INFO, Colors::BLUE),
            };
            ui.label(icons::icon(icon_ch, 14.0, icon_color));
            ui.add_space(4.0);
            let ec = theme::env_color(&problem.environment, mode);
            theme::status_pill(ui, &theme::env_label(&problem.environment), ec);
            ui.add_space(4.0);
            if let Some(ref svc) = problem.service {
                ui.label(
                    egui::RichText::new(svc)
                        .color(Colors::text_primary(mode))
                        .strong()
                        .size(13.0),
                );
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(&problem.message)
                    .color(Colors::text_secondary(mode))
                    .size(12.0),
            );
            if let Some(ts) = problem.timestamp {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(widgets::time_ago(&ts))
                            .size(11.0)
                            .color(Colors::text_muted(mode)),
                    );
                });
            }
        });
    });
}
