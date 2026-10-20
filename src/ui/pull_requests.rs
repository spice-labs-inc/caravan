use crate::cache::RepoKind;
use crate::models::release::PullRequest;
use crate::state::AppState;
use crate::ui::filters::GlobalFilters;
use crate::ui::icons;
use crate::ui::sidebar::View;
use crate::ui::theme::{self, Colors, ThemeMode};
use chrono::Utc;
use eframe::egui;

/// PR Dashboard — groups PRs by type: gitops release PRs, service PRs, library PRs.
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    _filters: &GlobalFilters,
    mode: ThemeMode,
) -> Option<View> {
    state.note_view_shown("pull_requests");
    let all_prs = state.get_pull_requests();
    let gitops_prs = state.get_open_gitops_prs();

    // Count only active open PRs (not draft, not do-not-merge/wip, not closed)
    let open_count = all_prs
        .iter()
        .filter(|pr| pr.is_open() && !is_deprioritized(pr))
        .count()
        + gitops_prs.len();

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Pull Requests")
                .size(26.0)
                .color(Colors::text_primary(mode))
                .strong(),
        );
        if open_count > 0 {
            theme::status_pill(ui, &format!("{} open", open_count), Colors::GREEN);
        }
    });
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("PRs across all tracked repositories")
            .color(Colors::text_secondary(mode)),
    );
    lazy_poll_caption(ui, state, mode, "polls while open");
    ui.add_space(16.0);

    // ── App-gitops Release PRs ─────────────────────────────────────────────
    theme::section_header(
        ui,
        mode,
        &format!("App-Gitops Release PRs ({})", gitops_prs.len()),
    );

    if gitops_prs.is_empty() {
        theme::card(ui, mode, |ui| {
            ui.label(egui::RichText::new("No open release PRs").color(Colors::text_muted(mode)));
        });
    } else {
        theme::card(ui, mode, |ui| {
            pr_table(
                ui,
                mode,
                "gitops_prs",
                &gitops_prs.iter().collect::<Vec<_>>(),
            );
        });
    }

    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);

    // ── Service Repo PRs ───────────────────────────────────────────────────
    let service_repo_names = state.get_repo_names_by_kind(RepoKind::Service);
    let mut service_prs: Vec<_> = all_prs
        .iter()
        .filter(|pr| service_repo_names.contains(&pr.repo))
        .collect();
    sort_prs(&mut service_prs);
    let service_open = service_prs
        .iter()
        .filter(|pr| pr.is_open() && !is_deprioritized(pr))
        .count();

    theme::section_header(ui, mode, &format!("App Repos ({})", service_open));

    if service_prs.is_empty() {
        theme::card(ui, mode, |ui| {
            ui.label(egui::RichText::new("No app PRs").color(Colors::text_muted(mode)));
        });
    } else {
        theme::card(ui, mode, |ui| {
            pr_table(ui, mode, "service_prs", &service_prs);
        });
    }

    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);

    // ── Library PRs ────────────────────────────────────────────────────────
    let library_repo_names = state.get_repo_names_by_kind(RepoKind::Library);
    let mut library_prs: Vec<_> = all_prs
        .iter()
        .filter(|pr| library_repo_names.contains(&pr.repo))
        .collect();
    sort_prs(&mut library_prs);
    let library_open = library_prs
        .iter()
        .filter(|pr| pr.is_open() && !is_deprioritized(pr))
        .count();

    theme::section_header(ui, mode, &format!("Libraries ({})", library_open));

    if library_prs.is_empty() {
        theme::card(ui, mode, |ui| {
            ui.label(egui::RichText::new("No library PRs").color(Colors::text_muted(mode)));
        });
    } else {
        theme::card(ui, mode, |ui| {
            pr_table(ui, mode, "library_prs", &library_prs);
        });
    }

    None
}

/// Whether a PR is draft or has "do not merge" / "work in progress" labels.
fn is_deprioritized(pr: &PullRequest) -> bool {
    pr.draft
        || pr.labels.iter().any(|l| {
            let lower = l.to_lowercase();
            lower == "do not merge" || lower == "work in progress"
        })
}

/// Sort PRs: active open first, then recently closed/merged, then deprioritized.
fn sort_prs(prs: &mut Vec<&PullRequest>) {
    prs.sort_by_key(|pr| {
        if pr.is_open() && !is_deprioritized(pr) {
            0 // Active open — top
        } else if !pr.is_open() {
            1 // Recently closed/merged — middle
        } else {
            2 // Deprioritized (draft, do-not-merge, wip) — bottom
        }
    });
}

fn pr_table(ui: &mut egui::Ui, mode: ThemeMode, table_id: &str, prs: &[&PullRequest]) {
    egui::Grid::new(egui::Id::new(table_id))
        .num_columns(8)
        .spacing([16.0, 6.0])
        .min_col_width(40.0)
        .striped(true)
        .show(ui, |ui| {
            // Header
            for col in &[
                "REPO",
                "#",
                "TITLE",
                "AUTHOR",
                "REVIEWERS",
                "STATUS",
                "AGE",
                "",
            ] {
                ui.label(
                    egui::RichText::new(*col)
                        .size(10.0)
                        .strong()
                        .color(Colors::text_muted(mode)),
                );
            }
            ui.end_row();

            for pr in prs {
                let is_closed = !pr.is_open();
                let dimmed = is_closed || is_deprioritized(pr);
                let text_color = if dimmed {
                    Colors::text_muted(mode)
                } else {
                    Colors::text_secondary(mode)
                };

                // Repo
                ui.label(
                    egui::RichText::new(&pr.repo)
                        .monospace()
                        .size(12.0)
                        .color(text_color),
                );

                // PR number — clickable link
                let number_color = if dimmed {
                    Colors::text_muted(mode)
                } else {
                    Colors::ACCENT
                };
                let number_resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("#{}", pr.number))
                            .monospace()
                            .size(12.0)
                            .color(number_color),
                    )
                    .sense(egui::Sense::click()),
                );
                if number_resp.clicked() {
                    let _ = open::that(&pr.html_url);
                }
                if number_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                number_resp.on_hover_text(&pr.html_url);

                // Title — also clickable
                let title_short = if pr.title.len() > 60 {
                    format!("{}…", &pr.title[..59])
                } else {
                    pr.title.clone()
                };
                let title_color = if dimmed {
                    Colors::text_muted(mode)
                } else {
                    Colors::text_primary(mode)
                };
                let title_resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&title_short)
                            .size(12.0)
                            .color(title_color)
                            .strong(),
                    )
                    .sense(egui::Sense::click()),
                );
                if title_resp.clicked() {
                    let _ = open::that(&pr.html_url);
                }
                if title_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if pr.title.len() > 60 {
                    title_resp.on_hover_text(&pr.title);
                }

                // Author — clickable link to GitHub profile
                if let Some(ref author) = pr.author {
                    let author_color = if dimmed {
                        Colors::text_muted(mode)
                    } else {
                        Colors::ACCENT
                    };
                    let author_resp = ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("@{}", author))
                                .size(12.0)
                                .color(author_color),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if author_resp.clicked() {
                        let _ = open::that(format!("https://github.com/{}", author));
                    }
                    if author_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    author_resp.on_hover_text(format!("https://github.com/{}", author));
                } else {
                    ui.label(
                        egui::RichText::new("—")
                            .size(12.0)
                            .color(Colors::text_muted(mode)),
                    );
                }

                // Reviewers — requested (pending) + completed reviews
                let has_any = !pr.reviewers.is_empty() || !pr.reviews.is_empty();
                if !has_any {
                    ui.label(
                        egui::RichText::new("—")
                            .size(11.0)
                            .color(Colors::text_muted(mode)),
                    );
                } else {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        // Completed reviews first — show with state icon
                        for (reviewer, state) in &pr.reviews {
                            let (icon_char, color) = match state.as_str() {
                                "Approved" => (icons::CIRCLE_CHECK, Colors::GREEN),
                                "ChangesRequested" => (icons::TRIANGLE_EXCLAMATION, Colors::RED),
                                _ => (icons::COMMENT, Colors::GRAY),
                            };
                            let rev_color = if dimmed {
                                Colors::text_muted(mode)
                            } else {
                                color
                            };
                            reviewer_label(ui, icon_char, reviewer, rev_color, state);
                        }
                        // Requested reviewers — shown dimmer, with clock icon
                        let reviewed_logins: std::collections::HashSet<&str> =
                            pr.reviews.iter().map(|(login, _)| login.as_str()).collect();
                        for reviewer in &pr.reviewers {
                            if reviewed_logins.contains(reviewer.as_str()) {
                                continue;
                            }
                            let rev_color = if dimmed {
                                Colors::text_muted(mode)
                            } else {
                                Colors::YELLOW
                            };
                            reviewer_label(ui, icons::CLOCK, reviewer, rev_color, "requested");
                        }
                    });
                }

                // Status pills
                ui.horizontal(|ui| {
                    if is_closed {
                        if pr.merged_at.is_some() || pr.state == "merged" {
                            theme::status_pill(ui, "merged", Colors::PURPLE);
                        } else {
                            theme::status_pill(ui, "closed", Colors::RED);
                        }
                    } else if pr.draft {
                        theme::status_pill(ui, "draft", Colors::GRAY);
                    } else if pr.is_release_pr() {
                        theme::status_pill(ui, "release", Colors::ORANGE);
                    } else {
                        theme::status_pill(ui, "open", Colors::GREEN);
                    }
                    match pr.mergeable {
                        Some(false) if pr.is_open() => {
                            theme::status_pill(ui, "conflicts", Colors::RED);
                        }
                        _ => {}
                    }
                    for label in &pr.labels {
                        theme::status_pill(ui, label, Colors::PURPLE);
                    }
                });

                // Age
                let age = Utc::now() - pr.created_at;
                let age_str = format_duration(age);
                let age_color = if is_closed {
                    Colors::text_muted(mode)
                } else if age.num_days() >= 7 {
                    Colors::RED
                } else if age.num_days() >= 3 {
                    Colors::ORANGE
                } else {
                    Colors::text_muted(mode)
                };
                ui.label(egui::RichText::new(&age_str).color(age_color).size(12.0));

                // External link icon
                let link_resp = ui.add(
                    egui::Label::new(icons::icon(
                        icons::ARROW_UP_RIGHT_FROM_SQUARE,
                        12.0,
                        Colors::text_muted(mode),
                    ))
                    .sense(egui::Sense::click()),
                );
                if link_resp.clicked() {
                    let _ = open::that(&pr.html_url);
                }
                if link_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                link_resp.on_hover_text("Open in GitHub");

                ui.end_row();
            }
        });
}

fn format_duration(d: chrono::Duration) -> String {
    if d.num_weeks() > 0 {
        format!("{}w ago", d.num_weeks())
    } else if d.num_days() > 0 {
        format!("{}d ago", d.num_days())
    } else if d.num_hours() > 0 {
        format!("{}h ago", d.num_hours())
    } else if d.num_minutes() > 0 {
        format!("{}m ago", d.num_minutes())
    } else {
        "just now".to_string()
    }
}

/// Render a single reviewer as icon + @name on one line, clickable.
fn reviewer_label(
    ui: &mut egui::Ui,
    icon_char: char,
    login: &str,
    color: egui::Color32,
    tooltip: &str,
) {
    let resp = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.label(icons::icon(icon_char, 11.0, color));
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("@{}", login))
                        .size(11.0)
                        .color(color),
                )
                .sense(egui::Sense::click()),
            )
        })
        .inner;
    if resp.clicked() {
        let _ = open::that(format!("https://github.com/{}", login));
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(format!("{} — {}", login, tooltip));
}

/// Small "updated <ago>" caption for views whose GitHub tier only runs
/// while they are open.
pub fn lazy_poll_caption(ui: &mut egui::Ui, state: &AppState, mode: ThemeMode, note: &str) {
    let updated = state
        .read(|s| s.last_github_poll)
        .map(|t| crate::ui::widgets::time_ago(&t))
        .unwrap_or_else(|| "never".to_string());
    ui.label(
        egui::RichText::new(format!("updated {} · {}", updated, note))
            .size(11.0)
            .color(Colors::text_muted(mode)),
    );
}
