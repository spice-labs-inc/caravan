use crate::models::deployment::DeploymentStatus;
use crate::models::deployment::HelmReleaseInfo;
use crate::models::release::WorkflowRun;
use crate::state::AppState;
use crate::ui::icons;
use crate::ui::theme::{self, Colors, ThemeMode};
use chrono::Utc;
use eframe::egui;

/// Color for a deployment status (theme-aware)
pub fn status_color(status: &DeploymentStatus, mode: ThemeMode) -> egui::Color32 {
    match status {
        DeploymentStatus::Healthy => Colors::GREEN,
        DeploymentStatus::Degraded => Colors::ORANGE,
        DeploymentStatus::Failed => Colors::RED,
        DeploymentStatus::Progressing => Colors::BLUE,
        DeploymentStatus::Suspended => Colors::GRAY,
        DeploymentStatus::Unknown => Colors::GRAY,
        DeploymentStatus::NotDeployed => Colors::text_muted(mode),
    }
}

/// Status icon character (FontAwesome)
pub fn status_icon_char(status: &DeploymentStatus) -> char {
    use super::icons;
    match status {
        DeploymentStatus::Healthy => icons::CIRCLE_CHECK,
        DeploymentStatus::Degraded => icons::TRIANGLE_EXCLAMATION,
        DeploymentStatus::Failed => icons::CIRCLE_XMARK,
        DeploymentStatus::Progressing => icons::ARROWS_ROTATE,
        DeploymentStatus::Suspended => icons::CIRCLE_PAUSE,
        DeploymentStatus::Unknown => icons::CIRCLE_QUESTION,
        DeploymentStatus::NotDeployed => icons::MINUS,
    }
}

/// Color for an environment name — delegates to theme::env_color
#[allow(dead_code)]
pub fn env_color(env_name: &str, mode: ThemeMode) -> egui::Color32 {
    theme::env_color(env_name, mode)
}

/// The commit sha carried by a version string: `sha-<hex>` or a bare hex
/// string of at least seven characters.
pub fn plain_sha(raw: &str) -> Option<&str> {
    let is_hex = |s: &str| s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit());
    if let Some(rest) = raw.strip_prefix("sha-") {
        if is_hex(rest) {
            return Some(rest);
        }
    }
    if is_hex(raw) {
        return Some(raw);
    }
    None
}

/// Every run of twelve or more hex characters cut to its first eight, so a
/// `tree-<40 hex>` tag or a `1.0.0-<40 hex>` chart version stays readable.
pub fn shorten_hashes(raw: &str) -> String {
    fn flush(run: &mut String, out: &mut String) {
        if run.len() >= 12 {
            out.extend(run.chars().take(8));
        } else {
            out.push_str(run);
        }
        run.clear();
    }
    let mut out = String::new();
    let mut run = String::new();
    for c in raw.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// The one place a version is turned into text. A commit sha shows as
/// `tag (sha8)` when a release or tag is known for it, otherwise `sha8`. Any
/// other string (a version tag, `pr-612`, `tree-...`) is shown as written with
/// long hashes shortened. A 40-character hash is never printed.
/// One identifier per build, never two: the release name when the build is
/// released, else the short commit, else `PR #N` for a PR build, else the
/// tag with its hashes shortened. Everything else belongs in the hover.
pub fn version_text(raw: &str, release: Option<&str>) -> String {
    let raw = raw.trim();
    let raw = raw.split('@').next().unwrap_or(raw);
    if raw.is_empty() {
        return "?".to_string();
    }
    if let Some(r) = release.map(str::trim).filter(|r| !r.is_empty()) {
        return r.to_string();
    }
    if let Some(sha) = plain_sha(raw) {
        return sha.chars().take(8).collect();
    }
    if let Some(n) = raw.strip_prefix("pr-") {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return format!("PR #{}", n);
        }
    }
    shorten_hashes(raw)
}

/// Eight characters of a digest, without its algorithm prefix.
pub fn digest_short(digest: &str) -> String {
    digest
        .rsplit(':')
        .next()
        .unwrap_or(digest)
        .chars()
        .take(8)
        .collect()
}

/// Version text for a raw tag or version string, with the release looked up
/// in the repo's releases and tags.
pub fn resolved_version(
    state: &AppState,
    repo: &str,
    raw: &str,
    chart_version: Option<&str>,
) -> String {
    let sha = plain_sha(raw).unwrap_or("");
    let release = state.find_release_for_service(repo, sha, chart_version);
    version_text(raw, release.as_deref())
}

/// The raw version string a HelmRelease runs: its image tag, else the commit
/// sha embedded in its chart version, else the chart version itself.
pub fn hr_version_raw(hr: &HelmReleaseInfo) -> String {
    hr.image_tag
        .clone()
        .or_else(|| hr.full_commit_sha().map(|s| format!("sha-{}", s)))
        .or_else(|| hr.chart_version.clone())
        .unwrap_or_else(|| "?".to_string())
}

/// Version text for a running HelmRelease.
pub fn version_label(state: &AppState, repo: &str, hr: &HelmReleaseInfo) -> String {
    resolved_version(
        state,
        repo,
        &hr_version_raw(hr),
        hr.chart_version.as_deref(),
    )
}

/// Format a duration in human-readable form
pub fn time_ago(dt: &chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let diff = now - *dt;

    if diff.num_seconds() < 60 {
        format!("{}s ago", diff.num_seconds())
    } else if diff.num_minutes() < 60 {
        format!("{}m ago", diff.num_minutes())
    } else if diff.num_hours() < 24 {
        format!("{}h ago", diff.num_hours())
    } else {
        format!("{}d ago", diff.num_days())
    }
}

/// Compact elapsed time without the "ago": 4m, 2h, 3d.
pub fn elapsed_short(d: chrono::Duration) -> String {
    if d.num_days() > 0 {
        format!("{}d", d.num_days())
    } else if d.num_hours() > 0 {
        format!("{}h", d.num_hours())
    } else {
        format!("{}m", d.num_minutes().max(0))
    }
}

/// A one-line strip with a chevron that expands in place. The open state is
/// kept in egui temp data under `id_source`.
pub fn collapsible_strip(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    id_source: &str,
    title: &str,
    badge: Option<(String, egui::Color32)>,
    default_open: bool,
    body: impl FnOnce(&mut egui::Ui),
) {
    let id = egui::Id::new(("collapsible_strip", id_source));
    let mut open: bool = ui
        .ctx()
        .data_mut(|d| d.get_temp(id).unwrap_or(default_open));
    theme::card(ui, mode, |ui| {
        let header = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let caret = if open {
                    icons::CHEVRON_DOWN
                } else {
                    icons::CHEVRON_RIGHT
                };
                ui.label(icons::icon(caret, 10.0, Colors::text_muted(mode)));
                ui.label(
                    egui::RichText::new(title)
                        .size(13.0)
                        .strong()
                        .color(Colors::text_primary(mode)),
                );
                if let Some((text, color)) = badge {
                    theme::status_pill(ui, &text, color);
                }
            })
            .response
            .interact(egui::Sense::click());
        if header.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if header.clicked() {
            open = !open;
            ui.ctx().data_mut(|d| d.insert_temp(id, open));
        }
        if open {
            ui.add_space(8.0);
            body(ui);
        }
    });
}

pub fn workflow_table(ui: &mut egui::Ui, mode: ThemeMode, table_id: &str, runs: &[WorkflowRun]) {
    egui::Grid::new(egui::Id::new(table_id))
        .num_columns(7)
        .spacing([16.0, 6.0])
        .min_col_width(40.0)
        .striped(true)
        .show(ui, |ui| {
            for col in &["STATUS", "REPO", "WORKFLOW", "BRANCH", "SHA", "AGE", ""] {
                ui.label(
                    egui::RichText::new(*col)
                        .size(10.0)
                        .strong()
                        .color(Colors::text_muted(mode)),
                );
            }
            ui.end_row();

            for run in runs {
                // Status pill
                ui.horizontal(|ui| {
                    let (label, color) = if run.is_in_progress() {
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
                        (run.conclusion.as_deref().unwrap_or("—"), Colors::GRAY)
                    };
                    theme::status_pill(ui, label, color);
                });

                // Repo — clickable
                let repo_resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&run.repo)
                            .size(12.0)
                            .color(Colors::text_primary(mode))
                            .strong(),
                    )
                    .sense(egui::Sense::click()),
                );
                if repo_resp.clicked() {
                    let _ = open::that(&run.html_url);
                }
                if repo_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }

                // Workflow name
                ui.label(
                    egui::RichText::new(&run.workflow_name)
                        .size(12.0)
                        .color(Colors::text_secondary(mode)),
                );

                // Branch
                if let Some(ref branch) = run.head_branch {
                    ui.label(
                        egui::RichText::new(branch)
                            .monospace()
                            .size(11.0)
                            .color(Colors::PURPLE),
                    );
                } else {
                    ui.label(
                        egui::RichText::new("—")
                            .size(11.0)
                            .color(Colors::text_muted(mode)),
                    );
                }

                // SHA
                ui.label(
                    egui::RichText::new(run.short_sha())
                        .monospace()
                        .size(11.0)
                        .color(Colors::text_muted(mode)),
                );

                // Age
                let duration = Utc::now() - run.created_at;
                let age_str = time_ago_short(duration);
                ui.label(
                    egui::RichText::new(age_str)
                        .size(12.0)
                        .color(Colors::text_muted(mode)),
                );

                // External link
                let link_resp = ui.add(
                    egui::Label::new(icons::icon(
                        icons::ARROW_UP_RIGHT_FROM_SQUARE,
                        12.0,
                        Colors::text_muted(mode),
                    ))
                    .sense(egui::Sense::click()),
                );
                if link_resp.clicked() {
                    let _ = open::that(&run.html_url);
                }
                if link_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                link_resp.on_hover_text("Open in GitHub");

                ui.end_row();
            }
        });
}

pub fn time_ago_short(d: chrono::Duration) -> String {
    if d.num_days() > 0 {
        format!("{}d ago", d.num_days())
    } else if d.num_hours() > 0 {
        format!("{}h ago", d.num_hours())
    } else if d.num_minutes() > 0 {
        format!("{}m ago", d.num_minutes())
    } else {
        "just now".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_with_release_shows_only_the_release() {
        assert_eq!(
            version_text(
                "sha-2b5ba6be2808cadf5e0414f6fa817c76fd79b952",
                Some("v1.3.3")
            ),
            "v1.3.3"
        );
        assert_eq!(
            version_text("2b5ba6be2808cadf5e0414f6fa817c76fd79b952", None),
            "2b5ba6be"
        );
        assert_eq!(version_text("sha-2b5ba6be", None), "2b5ba6be");
    }

    #[test]
    fn non_version_tags_keep_their_prefix_with_short_hashes() {
        assert_eq!(
            version_text("tree-3438098b2d850f296438bfe9d15b485e47c4077d", None),
            "tree-3438098b"
        );
        assert_eq!(version_text("pr-612", None), "PR #612");
        assert_eq!(version_text("pr-9@sha256:75be7dc4", None), "PR #9");
        assert_eq!(
            version_text("1.0.0-2b5ba6be2808cadf5e0414f6fa817c76fd79b952", None),
            "1.0.0-2b5ba6be"
        );
    }

    #[test]
    fn version_tags_do_not_repeat_their_release() {
        assert_eq!(version_text("v0.17.1", Some("v0.17.1")), "v0.17.1");
        assert_eq!(version_text("0.8.2", Some("v0.8.2")), "v0.8.2");
        assert_eq!(version_text("pr-612", Some("v0.17.1")), "v0.17.1");
        assert_eq!(
            version_text("sha-e3f01e86a467c48f", Some("v0.17.1")),
            "v0.17.1"
        );
        assert_eq!(version_text("", None), "?");
    }

    #[test]
    fn never_prints_a_full_hash() {
        let full = "2b5ba6be2808cadf5e0414f6fa817c76fd79b952";
        for raw in [
            full.to_string(),
            format!("sha-{}", full),
            format!("tree-{}", full),
            format!("0.1.0-{}", full),
        ] {
            assert!(!version_text(&raw, Some("v9")).contains(full), "{}", raw);
        }
        assert_eq!(digest_short("sha256:f07e8119b33a266da781329f"), "f07e8119");
    }
}
