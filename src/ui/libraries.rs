use crate::cache::{Cache, OrgRepo, RepoKind};
use crate::state::AppState;
use crate::ui::charts;
use crate::ui::filters::GlobalFilters;
use crate::ui::sidebar::View;
use crate::ui::theme::{self, Colors, ThemeMode};
use crate::ui::toast::Toaster;
use chrono::Utc;
use eframe::egui;
use std::sync::Arc;

/// Libraries view — table of library repos with latest release info.
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    _filters: &GlobalFilters,
    mode: ThemeMode,
    cache: &Option<Arc<Cache>>,
    toaster: &mut Toaster,
) -> Option<View> {
    ui.label(
        egui::RichText::new("Libraries & CLI Releases")
            .size(26.0)
            .color(Colors::text_primary(mode))
            .strong(),
    );
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Non-K8s artifact releases across all library repositories")
            .color(Colors::text_secondary(mode)),
    );
    ui.add_space(16.0);

    state.note_view_shown("libraries");
    let all_releases = state.get_releases();
    let all_runs = state.get_workflow_runs();
    let library_repos = state.get_repos_by_kind(RepoKind::Library);

    // get_repos_by_kind already excludes hidden repos
    let mut repo_names: Vec<String> = if library_repos.is_empty() {
        // Fallback: just show repos that have releases
        let mut names: Vec<String> = all_releases.iter().map(|r| r.repo.clone()).collect();
        names.sort();
        names.dedup();
        names
    } else {
        library_repos.iter().map(|r| r.name.clone()).collect()
    };
    repo_names.sort();

    // Collect hidden repos for the "manage" section
    let hidden_repos = state.read(|s| s.hidden_repos.clone());
    let mut hidden_library_repos: Vec<OrgRepo> = state.read(|s| {
        s.org_repos
            .iter()
            .filter(|r| r.kind == RepoKind::Library && s.hidden_repos.contains(&r.name))
            .cloned()
            .collect()
    });
    hidden_library_repos.sort_by(|a, b| a.name.cmp(&b.name));

    // Summary line
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{} libraries", repo_names.len()))
                .size(12.0)
                .color(Colors::text_muted(mode)),
        );
        if !hidden_repos.is_empty() {
            ui.label(
                egui::RichText::new(format!("· {} hidden", hidden_repos.len()))
                    .size(12.0)
                    .color(Colors::text_muted(mode)),
            );
        }
    });
    crate::ui::pull_requests::lazy_poll_caption(ui, state, mode, "releases poll every 15 min");
    ui.add_space(8.0);

    // ── Release Activity Heatmap ──────────────────────────────────────────
    theme::section_header(ui, mode, "Release Activity");
    theme::card(ui, mode, |ui| {
        charts::library_release_heatmap(ui, state, mode);
    });
    ui.add_space(12.0);

    if repo_names.is_empty() {
        theme::card(ui, mode, |ui| {
            ui.label(
                egui::RichText::new("Waiting for first poll…")
                    .color(Colors::text_muted(mode))
                    .italics(),
            );
        });
        return None;
    }

    // ── Library table ────────────────────────────────────────────────────────
    let mut hide_action: Option<String> = None;

    theme::card(ui, mode, |ui| {
        egui::Grid::new("libraries_table")
            .num_columns(8)
            .spacing([16.0, 8.0])
            .min_col_width(60.0)
            .striped(true)
            .show(ui, |ui| {
                // Header row
                let hdr = |ui: &mut egui::Ui, text: &str| {
                    ui.label(
                        egui::RichText::new(text.to_uppercase())
                            .color(Colors::text_muted(mode))
                            .strong()
                            .size(11.0),
                    );
                };
                hdr(ui, "Repository");
                hdr(ui, "Visibility");
                hdr(ui, "License");
                hdr(ui, "Type");
                hdr(ui, "Latest Release");
                hdr(ui, "Publish Status");
                hdr(ui, "Released");
                ui.label(egui::RichText::new("").size(11.0));
                ui.end_row();

                // Data rows
                for repo_name in &repo_names {
                    let lib_info = library_repos.iter().find(|r| &r.name == repo_name);
                    let lang = lib_info
                        .and_then(|r| r.language.as_deref())
                        .unwrap_or("unknown");

                    let latest_release = all_releases
                        .iter()
                        .filter(|r| &r.repo == repo_name && !r.draft)
                        .max_by_key(|r| r.created_at);

                    let latest_publish = all_runs
                        .iter()
                        .filter(|r| {
                            &r.repo == repo_name
                                && r.workflow_name.to_lowercase().contains("publish")
                        })
                        .max_by_key(|r| r.updated_at);

                    // Repo name
                    ui.label(
                        egui::RichText::new(repo_name.as_str())
                            .color(Colors::text_primary(mode))
                            .strong(),
                    );

                    // Visibility
                    if let Some(info) = lib_info {
                        if info.private {
                            theme::status_pill(ui, "private", Colors::GRAY);
                        } else {
                            theme::status_pill(ui, "public", Colors::GREEN);
                        }
                    } else {
                        ui.label(egui::RichText::new("—").color(Colors::text_muted(mode)));
                    }

                    // License
                    if let Some(lic) = lib_info.and_then(|i| i.license.as_ref()) {
                        theme::status_pill(ui, lic, Colors::BLUE);
                    } else {
                        ui.label(egui::RichText::new("—").color(Colors::text_muted(mode)));
                    }

                    // Language/type pill
                    let type_color = match lang.to_lowercase().as_str() {
                        "scala" | "java" | "kotlin" => Colors::ORANGE,
                        "rust" => Colors::RED,
                        "typescript" | "javascript" => Colors::BLUE,
                        "go" => Colors::GREEN,
                        _ => Colors::GRAY,
                    };
                    theme::status_pill(ui, lang, type_color);

                    // Latest release tag
                    if let Some(rel) = latest_release {
                        ui.label(
                            egui::RichText::new(&rel.tag)
                                .color(Colors::ACCENT)
                                .strong()
                                .monospace(),
                        );
                    } else {
                        ui.label(egui::RichText::new("—").color(Colors::text_muted(mode)));
                    }

                    // Publish status
                    if let Some(run) = latest_publish {
                        let (label, color) = if run.is_in_progress() {
                            ("publishing", Colors::BLUE)
                        } else if run.is_success() {
                            ("published", Colors::GREEN)
                        } else if run.is_failed() {
                            ("failed", Colors::RED)
                        } else {
                            ("unknown", Colors::GRAY)
                        };
                        theme::status_pill(ui, label, color);
                    } else {
                        ui.label(egui::RichText::new("—").color(Colors::text_muted(mode)));
                    }

                    // Last release date
                    if let Some(rel) = latest_release {
                        let date = rel.published_at.unwrap_or(rel.created_at);
                        let age = Utc::now() - date;
                        ui.label(
                            egui::RichText::new(format_duration(age))
                                .color(Colors::text_secondary(mode))
                                .size(12.0),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("never")
                                .color(Colors::text_muted(mode))
                                .size(12.0),
                        );
                    }

                    // Hide button
                    let hide_resp = ui.add(
                        egui::Button::new(
                            egui::RichText::new("Hide")
                                .size(11.0)
                                .color(Colors::text_muted(mode)),
                        )
                        .fill(Colors::bg_elevated(mode))
                        .corner_radius(8.0),
                    );
                    if hide_resp.clicked() {
                        hide_action = Some(repo_name.clone());
                    }
                    hide_resp.on_hover_text("Hide this repository from the list");

                    ui.end_row();
                }
            });
    });

    // Process hide action outside the borrow
    if let Some(repo_name) = hide_action {
        let is_hidden = state.toggle_hidden(&repo_name);
        if let Some(ref c) = cache {
            if is_hidden {
                let _ = c.save_hidden_repo(&repo_name);
            } else {
                let _ = c.remove_hidden_repo(&repo_name);
            }
        }
        if is_hidden {
            toaster.push(
                "",
                format!("Hidden {repo_name} — find it under \"Hidden repositories\""),
            );
        }
    }

    // ── Hidden repos section ───────────────────────────────────────────────
    if !hidden_library_repos.is_empty() {
        ui.add_space(16.0);

        let id = ui.make_persistent_id("hidden_repos_collapse");
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Hidden repositories ({})",
                        hidden_library_repos.len()
                    ))
                    .size(13.0)
                    .color(Colors::text_muted(mode)),
                );
            })
            .body(|ui| {
                let mut unhide_action: Option<String> = None;
                theme::card(ui, mode, |ui| {
                    egui::Grid::new("hidden_repos_grid")
                        .num_columns(3)
                        .spacing([16.0, 6.0])
                        .min_col_width(60.0)
                        .striped(true)
                        .show(ui, |ui| {
                            for repo in &hidden_library_repos {
                                ui.label(
                                    egui::RichText::new(&repo.name)
                                        .size(12.0)
                                        .color(Colors::text_muted(mode)),
                                );
                                if let Some(ref lang) = repo.language {
                                    ui.label(
                                        egui::RichText::new(lang.as_str())
                                            .size(11.0)
                                            .color(Colors::text_muted(mode)),
                                    );
                                } else {
                                    ui.label(
                                        egui::RichText::new("—").color(Colors::text_muted(mode)),
                                    );
                                }
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new("Show")
                                                .size(11.0)
                                                .color(Colors::ACCENT),
                                        )
                                        .fill(egui::Color32::TRANSPARENT)
                                        .corner_radius(4.0),
                                    )
                                    .clicked()
                                {
                                    unhide_action = Some(repo.name.clone());
                                }
                                ui.end_row();
                            }
                        });
                });
                if let Some(repo_name) = unhide_action {
                    state.toggle_hidden(&repo_name);
                    if let Some(ref c) = cache {
                        let _ = c.remove_hidden_repo(&repo_name);
                    }
                    toaster.push("", format!("Restored {repo_name}"));
                }
            });
    }

    None
}

fn format_duration(d: chrono::Duration) -> String {
    if d.num_weeks() >= 4 {
        format!("{}mo ago", d.num_days() / 30)
    } else if d.num_weeks() > 0 {
        format!("{}w ago", d.num_weeks())
    } else if d.num_days() > 0 {
        format!("{}d ago", d.num_days())
    } else if d.num_hours() > 0 {
        format!("{}h ago", d.num_hours())
    } else {
        "just now".to_string()
    }
}
