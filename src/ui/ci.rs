//! The CI page: running, failed and recent GitHub Actions runs across the
//! tracked repos.

use crate::cache::RepoKind;
use crate::models::release::WorkflowRun;
use crate::state::AppState;
use crate::ui::filters::GlobalFilters;
use crate::ui::icons;
use crate::ui::sidebar::View;
use crate::ui::theme::{self, Colors, ThemeMode};
use crate::ui::widgets;
use chrono::{DateTime, Duration, Utc};
use eframe::egui;

/// Completed runs older than this stay out of the page.
pub const WINDOW_HOURS: i64 = 24;
/// Recent runs shown per repo before the "N more" note.
pub const RECENT_PER_REPO: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepoFilter {
    #[default]
    All,
    Services,
    Libraries,
    Actions,
}

impl RepoFilter {
    pub const ALL: [RepoFilter; 4] = [
        RepoFilter::All,
        RepoFilter::Services,
        RepoFilter::Libraries,
        RepoFilter::Actions,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RepoFilter::All => "All",
            RepoFilter::Services => "Services",
            RepoFilter::Libraries => "Libraries",
            RepoFilter::Actions => "Actions",
        }
    }

    pub fn kind(self) -> Option<RepoKind> {
        match self {
            RepoFilter::All => None,
            RepoFilter::Services => Some(RepoKind::Service),
            RepoFilter::Libraries => Some(RepoKind::Library),
            RepoFilter::Actions => Some(RepoKind::Action),
        }
    }
}

/// Completed runs of one repo, newest first, capped at [`RECENT_PER_REPO`].
#[derive(Debug, Clone)]
pub struct RepoRuns {
    pub repo: String,
    pub runs: Vec<WorkflowRun>,
    /// Runs inside the window that did not fit under the cap
    pub more: usize,
}

/// The runs whose repo the filter admits. `None` admits every repo.
pub fn filter_runs(runs: &[WorkflowRun], allowed: Option<&[String]>) -> Vec<WorkflowRun> {
    runs.iter()
        .filter(|r| allowed.is_none_or(|names| names.contains(&r.repo)))
        .cloned()
        .collect()
}

/// In-progress and queued runs, newest first.
pub fn running(runs: &[WorkflowRun]) -> Vec<WorkflowRun> {
    let mut out: Vec<WorkflowRun> = runs
        .iter()
        .filter(|r| r.is_in_progress())
        .cloned()
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    out
}

/// Runs that failed inside the window, most recently finished first.
pub fn failed(runs: &[WorkflowRun], now: DateTime<Utc>) -> Vec<WorkflowRun> {
    let cutoff = now - Duration::hours(WINDOW_HOURS);
    let mut out: Vec<WorkflowRun> = runs
        .iter()
        .filter(|r| r.is_failed() && r.updated_at > cutoff)
        .cloned()
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    out
}

/// Runs that completed inside the window, grouped by repo. Repos are ordered
/// by their newest run, runs within a repo newest first.
pub fn recent(runs: &[WorkflowRun], now: DateTime<Utc>) -> Vec<RepoRuns> {
    let cutoff = now - Duration::hours(WINDOW_HOURS);
    let mut groups: Vec<RepoRuns> = Vec::new();
    let mut done: Vec<&WorkflowRun> = runs
        .iter()
        .filter(|r| !r.is_in_progress() && r.updated_at > cutoff)
        .collect();
    done.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    for run in done {
        let group = match groups.iter_mut().find(|g| g.repo == run.repo) {
            Some(g) => g,
            None => {
                groups.push(RepoRuns {
                    repo: run.repo.clone(),
                    runs: Vec::new(),
                    more: 0,
                });
                groups.last_mut().unwrap()
            }
        };
        if group.runs.len() < RECENT_PER_REPO {
            group.runs.push(run.clone());
        } else {
            group.more += 1;
        }
    }
    groups
}

/// Label and color of a run's state pill.
pub fn state_pill(run: &WorkflowRun) -> (&str, egui::Color32) {
    if run.is_in_progress() {
        if run.status == "queued" {
            ("queued", Colors::YELLOW)
        } else {
            ("running", Colors::BLUE)
        }
    } else if run.is_success() {
        ("success", Colors::GREEN)
    } else if run.is_failed() {
        ("failure", Colors::RED)
    } else {
        (run.conclusion.as_deref().unwrap_or("unknown"), Colors::GRAY)
    }
}

pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    _filters: &GlobalFilters,
    mode: ThemeMode,
) -> Option<View> {
    let filter_id = egui::Id::new("ci_repo_filter");
    let mut filter: RepoFilter = ui
        .ctx()
        .data_mut(|d| d.get_temp(filter_id).unwrap_or_default());

    let all_runs = state.get_workflow_runs();
    let allowed = filter.kind().map(|k| state.get_repo_names_by_kind(k));
    let runs = filter_runs(&all_runs, allowed.as_deref());
    let now = Utc::now();
    let running = running(&runs);
    let failed = failed(&runs, now);
    let recent = recent(&runs, now);

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("CI")
                .size(26.0)
                .color(Colors::text_primary(mode))
                .strong(),
        );
        if !running.is_empty() {
            theme::status_pill(ui, &format!("{} running", running.len()), Colors::BLUE);
        }
        if !failed.is_empty() {
            theme::status_pill(ui, &format!("{} failed", failed.len()), Colors::RED);
        }
    });
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("GitHub Actions across the tracked repos")
            .color(Colors::text_secondary(mode)),
    );
    ui.add_space(12.0);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for choice in RepoFilter::ALL {
            if theme::pill_button(ui, mode, choice.label(), filter == choice) {
                filter = choice;
                ui.ctx().data_mut(|d| d.insert_temp(filter_id, filter));
            }
        }
    });

    theme::section_header(ui, mode, &format!("Running ({})", running.len()));
    theme::card(ui, mode, |ui| {
        if running.is_empty() {
            empty_note(ui, mode, "Nothing running");
        } else {
            runs_grid(ui, mode, "ci_running", &running, |r| {
                format!("started {}", widgets::time_ago(&r.created_at))
            });
        }
    });

    theme::section_header(
        ui,
        mode,
        &format!(
            "Failed in the last {} hours ({})",
            WINDOW_HOURS,
            failed.len()
        ),
    );
    if failed.is_empty() {
        theme::card(ui, mode, |ui| {
            empty_note(
                ui,
                mode,
                &format!("No failures in the last {} hours", WINDOW_HOURS),
            );
        });
    } else {
        theme::accent_card(ui, mode, Colors::RED, |ui| {
            runs_grid(ui, mode, "ci_failed", &failed, |r| {
                format!("finished {}", widgets::time_ago(&r.updated_at))
            });
        });
    }

    theme::section_header(ui, mode, "Recent");
    if recent.is_empty() {
        theme::card(ui, mode, |ui| {
            empty_note(
                ui,
                mode,
                &format!("No runs completed in the last {} hours", WINDOW_HOURS),
            );
        });
    }
    for group in &recent {
        theme::card(ui, mode, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&group.repo)
                        .size(14.0)
                        .strong()
                        .color(Colors::text_primary(mode)),
                );
                let total = group.runs.len() + group.more;
                ui.label(
                    egui::RichText::new(format!(
                        "{} run{}",
                        total,
                        if total == 1 { "" } else { "s" }
                    ))
                    .size(12.0)
                    .color(Colors::text_muted(mode)),
                );
            });
            ui.add_space(6.0);
            runs_grid(
                ui,
                mode,
                &format!("ci_recent_{}", group.repo),
                &group.runs,
                |r| widgets::time_ago(&r.updated_at),
            );
            if group.more > 0 {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("{} more", group.more))
                        .size(11.0)
                        .color(Colors::text_muted(mode)),
                );
            }
        });
    }

    None
}

fn empty_note(ui: &mut egui::Ui, mode: ThemeMode, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(12.0)
            .color(Colors::text_muted(mode)),
    );
}

/// One row per run: state, repo, workflow, branch, event, time, link.
fn runs_grid(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    id: &str,
    runs: &[WorkflowRun],
    when: impl Fn(&WorkflowRun) -> String,
) {
    egui::Grid::new(egui::Id::new(id))
        .num_columns(7)
        .spacing([16.0, 6.0])
        .min_col_width(40.0)
        .striped(true)
        .show(ui, |ui| {
            for run in runs {
                let (label, color) = state_pill(run);
                theme::status_pill(ui, label, color);

                let repo = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&run.repo)
                            .size(12.0)
                            .strong()
                            .color(Colors::text_primary(mode)),
                    )
                    .sense(egui::Sense::click()),
                );
                if repo.clicked() {
                    let _ = open::that(&run.html_url);
                }
                if repo.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }

                ui.label(
                    egui::RichText::new(&run.workflow_name)
                        .size(12.0)
                        .color(Colors::text_secondary(mode)),
                );

                match run.head_branch.as_deref() {
                    Some(branch) => ui.label(
                        egui::RichText::new(branch)
                            .monospace()
                            .size(11.0)
                            .color(Colors::PURPLE),
                    ),
                    None => ui.label(
                        egui::RichText::new(run.short_sha())
                            .monospace()
                            .size(11.0)
                            .color(Colors::text_muted(mode)),
                    ),
                };

                ui.label(
                    egui::RichText::new(&run.event)
                        .size(11.0)
                        .color(Colors::text_muted(mode)),
                );

                ui.label(
                    egui::RichText::new(when(run))
                        .size(12.0)
                        .color(Colors::text_muted(mode)),
                );

                let link = ui.add(
                    egui::Label::new(icons::icon(
                        icons::ARROW_UP_RIGHT_FROM_SQUARE,
                        12.0,
                        Colors::text_muted(mode),
                    ))
                    .sense(egui::Sense::click()),
                );
                if link.clicked() {
                    let _ = open::that(&run.html_url);
                }
                if link.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                link.on_hover_text("Open in GitHub");

                ui.end_row();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(repo: &str, status: &str, conclusion: Option<&str>, ago_minutes: i64) -> WorkflowRun {
        let at = Utc::now() - Duration::minutes(ago_minutes);
        WorkflowRun {
            repo: repo.to_string(),
            workflow_name: "Build".to_string(),
            workflow_file: None,
            run_number: 1,
            id: ago_minutes as u64,
            status: status.to_string(),
            conclusion: conclusion.map(str::to_string),
            head_branch: Some("main".to_string()),
            head_sha: "abcdef0123456789".to_string(),
            event: "push".to_string(),
            created_at: at,
            updated_at: at,
            html_url: String::new(),
        }
    }

    #[test]
    fn sections_split_running_failed_and_recent() {
        let runs = vec![
            run("notes-api", "in_progress", None, 5),
            run("notes-api", "queued", None, 1),
            run("notes-api", "completed", Some("failure"), 30),
            run("engine", "completed", Some("success"), 60),
            run("engine", "completed", Some("failure"), 25 * 60),
        ];
        let now = Utc::now();
        let running = running(&runs);
        assert_eq!(running.len(), 2);
        assert_eq!(running[0].status, "queued");
        let failed = failed(&runs, now);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].repo, "notes-api");
        let recent = recent(&runs, now);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].repo, "notes-api");
        assert_eq!(recent[1].repo, "engine");
        assert_eq!(recent[1].runs.len(), 1);
    }

    #[test]
    fn recent_caps_per_repo_and_counts_the_rest() {
        let runs: Vec<WorkflowRun> = (0..13)
            .map(|i| run("notes-api", "completed", Some("success"), i + 1))
            .collect();
        let recent = recent(&runs, Utc::now());
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].runs.len(), RECENT_PER_REPO);
        assert_eq!(recent[0].more, 3);
        assert_eq!(recent[0].runs[0].id, 1);
    }

    #[test]
    fn filter_admits_only_listed_repos() {
        let runs = vec![
            run("notes-api", "completed", Some("success"), 1),
            run("goatrodeo", "completed", Some("success"), 1),
        ];
        let libraries = vec!["goatrodeo".to_string()];
        let kept = filter_runs(&runs, Some(&libraries));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].repo, "goatrodeo");
        assert_eq!(filter_runs(&runs, None).len(), 2);
    }
}
