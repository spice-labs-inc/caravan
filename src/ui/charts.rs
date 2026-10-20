use crate::state::AppState;
use crate::ui::icons;
use crate::ui::theme::{Colors, ThemeMode};
use chrono::{Duration, Utc};
use eframe::egui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use egui_plot::{Bar, BarChart, Legend, Plot};
use std::collections::HashMap;

// ── Pinned Tooltip State ───────────────────────────────────────────────────

/// Persistent state for a "sticky" release tooltip pinned by clicking a heatmap cell.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct PinnedTooltip {
    /// Which heatmap this belongs to (e.g. "heatmap" or "lib_heatmap")
    heatmap_id: String,
    /// Repo / service name
    name: String,
    /// Date string for the header
    date_str: String,
    /// Number of releases
    count: u32,
    /// Per-release details: (tag, author, time, body)
    releases: Vec<(String, String, String, String)>,
    /// Color for tags
    tag_color: egui::Color32,
    /// Screen position to anchor the popup
    anchor_pos: egui::Pos2,
}

/// Read the currently pinned tooltip from egui memory.
fn get_pinned_tooltip(ctx: &egui::Context) -> Option<PinnedTooltip> {
    ctx.data(|d| d.get_temp::<PinnedTooltip>(egui::Id::new("pinned_release_tooltip")))
}

/// Write a pinned tooltip to egui memory.
fn set_pinned_tooltip(ctx: &egui::Context, tooltip: Option<PinnedTooltip>) {
    ctx.data_mut(|d| {
        if let Some(t) = tooltip {
            d.insert_temp(egui::Id::new("pinned_release_tooltip"), t);
        } else {
            d.remove_temp::<PinnedTooltip>(egui::Id::new("pinned_release_tooltip"));
        }
    });
}

/// Clear the pinned tooltip.
fn clear_pinned_tooltip(ctx: &egui::Context) {
    ctx.data_mut(|d| {
        d.remove_temp::<PinnedTooltip>(egui::Id::new("pinned_release_tooltip"));
        d.remove_temp::<u32>(egui::Id::new("pinned_tooltip_frames"));
    });
}

/// Render the pinned tooltip popup if one exists. Call once per frame, after heatmaps.
pub fn show_pinned_release_popup(ctx: &egui::Context, mode: ThemeMode) {
    let pinned = match get_pinned_tooltip(ctx) {
        Some(p) => p,
        None => return,
    };

    let popup_id = egui::Id::new("pinned_release_popup");

    // Position the popup near the anchor, clamped to screen
    let screen_rect = ctx.screen_rect();
    let popup_width = 500.0_f32;
    let max_popup_height = 400.0_f32;
    let margin = 8.0_f32;

    let mut pos = egui::pos2(pinned.anchor_pos.x + 12.0, pinned.anchor_pos.y + 12.0);
    if pos.x + popup_width > screen_rect.right() - margin {
        pos.x = pinned.anchor_pos.x - popup_width - 12.0;
    }
    if pos.y + max_popup_height > screen_rect.bottom() - margin {
        pos.y = screen_rect.bottom() - max_popup_height - margin;
    }
    pos.x = pos.x.max(margin);
    pos.y = pos.y.max(margin);

    let area_response = egui::Area::new(popup_id)
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .inner_margin(12.0)
                .outer_margin(0.0)
                .corner_radius(8.0)
                .shadow(egui::epaint::Shadow {
                    offset: [0, 4],
                    blur: 16,
                    spread: 2,
                    color: egui::Color32::from_black_alpha(60),
                })
                .show(ui, |ui| {
                    ui.set_max_width(popup_width);

                    // Header with close button
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} — {} ({} release{})",
                                pinned.name,
                                pinned.date_str,
                                pinned.count,
                                if pinned.count == 1 { "" } else { "s" }
                            ))
                            .strong()
                            .size(13.0),
                        );

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(icons::icon(
                                        icons::CIRCLE_XMARK,
                                        14.0,
                                        Colors::text_muted(mode),
                                    ))
                                    .fill(egui::Color32::TRANSPARENT)
                                    .frame(false),
                                )
                                .on_hover_text("Close")
                                .clicked()
                            {
                                clear_pinned_tooltip(ui.ctx());
                            }
                        });
                    });

                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);

                    // Scrollable content
                    egui::ScrollArea::vertical()
                        .max_height(max_popup_height - 60.0)
                        .show(ui, |ui| {
                            render_release_content(
                                ui,
                                mode,
                                "pinned_body",
                                &pinned.releases,
                                pinned.tag_color,
                            );
                        });
                });
        });

    // Close on click outside the popup area.
    // Skip the first frame (the click that pinned the tooltip would also dismiss it).
    let shown_frames_id = egui::Id::new("pinned_tooltip_frames");
    let frames_shown: u32 = ctx.data(|d| d.get_temp(shown_frames_id)).unwrap_or(0);
    ctx.data_mut(|d| d.insert_temp(shown_frames_id, frames_shown + 1));

    if frames_shown > 0 {
        let popup_rect = area_response.response.rect;
        if ctx.input(|i| i.pointer.any_click()) {
            if let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) {
                if !popup_rect.contains(pos) {
                    clear_pinned_tooltip(ctx);
                }
            }
        }
    }
}

/// Replace bare URLs in text with compact markdown links.
/// e.g. "https://github.com/org/repo/pull/123" → "[org/repo#123](url)"
/// and other URLs → "[link](url)"
pub fn compress_bare_urls(text: &str) -> String {
    // Regex-free approach: find bare URLs not already inside markdown link syntax
    let mut result = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();

    while let Some((i, ch)) = chars.next() {
        // Check if we're at a bare URL (not preceded by '(' or '](' which would mean markdown link)
        if text[i..].starts_with("https://") || text[i..].starts_with("http://") {
            // Check if this URL is already part of a markdown link
            let is_in_markdown_link =
                (i > 0 && text.as_bytes()[i - 1] == b'(') || (i > 1 && text[..i].ends_with("]("));

            if is_in_markdown_link {
                result.push(ch);
                continue;
            }

            // Extract the full URL
            let url_start = i;
            let mut url_end = i;
            for (j, c) in text[i..].char_indices() {
                if c.is_whitespace() || c == ')' || c == '>' || c == '"' || c == '\'' {
                    break;
                }
                url_end = i + j + c.len_utf8();
            }
            // Handle end of string
            if url_end == i {
                url_end = text.len();
            }

            let url = &text[url_start..url_end];
            let label = compact_url_label(url);
            result.push_str(&format!("[{}]({})", label, url));

            // Advance past the URL
            while chars.peek().is_some_and(|&(j, _)| j < url_end) {
                chars.next();
            }
        } else {
            result.push(ch);
        }
    }

    result
}

/// Generate a compact label for a URL.
fn compact_url_label(url: &str) -> String {
    // GitHub pull request: https://github.com/org/repo/pull/123
    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 4 && parts[2] == "pull" {
            return format!("{}#{}", parts[1], parts[3]);
        }
        if parts.len() >= 4 && parts[2] == "issues" {
            return format!("{}#{}", parts[1], parts[3]);
        }
        if parts.len() >= 4 && parts[2] == "compare" {
            return format!("{} {}", parts[1], parts[3]);
        }
        if parts.len() >= 2 {
            return format!("{}/{}", parts[0], parts[1]);
        }
    }
    "\u{f0c1} link".to_string()
}

// ── Release Activity (last 14 days) ────────────────────────────────────────

/// Bar chart: releases per day, colored by repo category (service vs library)
#[allow(dead_code)]
pub fn release_activity(ui: &mut egui::Ui, state: &AppState, mode: ThemeMode) {
    let releases = state.get_releases();
    let now = Utc::now().date_naive();
    let days = 14;

    // Bucket releases by day offset (0 = today, 1 = yesterday, etc.)
    let mut service_counts = vec![0.0_f64; days];
    let mut library_counts = vec![0.0_f64; days];

    let library_repo_names = state.get_repo_names_by_kind(crate::cache::RepoKind::Library);

    for release in &releases {
        let date = release
            .published_at
            .unwrap_or(release.created_at)
            .date_naive();
        let age = (now - date).num_days();
        if age >= 0 && age < days as i64 {
            let idx = (days as i64 - 1 - age) as usize;
            if library_repo_names.contains(&release.repo) {
                library_counts[idx] += 1.0;
            } else {
                service_counts[idx] += 1.0;
            }
        }
    }

    let service_bars: Vec<Bar> = service_counts
        .iter()
        .enumerate()
        .map(|(i, &count)| Bar::new(i as f64, count).width(0.35).name("Apps"))
        .collect();

    let library_bars: Vec<Bar> = library_counts
        .iter()
        .enumerate()
        .map(|(i, &count)| {
            Bar::new(i as f64, count)
                .width(0.35)
                .base_offset(service_counts[i])
                .name("Libraries")
        })
        .collect();

    let svc_color = to_plot_color(Colors::BLUE);
    let lib_color = to_plot_color(Colors::PURPLE);

    let svc_chart = BarChart::new(service_bars).color(svc_color).name("Apps");
    let lib_chart = BarChart::new(library_bars)
        .color(lib_color)
        .name("Libraries");

    // Day labels for x-axis
    let day_labels: Vec<String> = (0..days)
        .map(|i| {
            let date = now - Duration::days(days as i64 - 1 - i as i64);
            date.format("%b %d").to_string()
        })
        .collect();

    let total: f64 = service_counts.iter().sum::<f64>() + library_counts.iter().sum::<f64>();

    Plot::new("release_activity")
        .height(160.0)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .show_axes([false, true])
        .show_grid([false, true])
        .y_axis_label("")
        .legend(Legend::default())
        .x_axis_formatter(move |mark, _range| {
            let idx = mark.value.round() as usize;
            if idx < day_labels.len() {
                day_labels[idx].clone()
            } else {
                String::new()
            }
        })
        .y_axis_formatter(|mark, _range| {
            let v = mark.value;
            if v >= 0.0 && v == v.round() {
                format!("{}", v as i64)
            } else {
                String::new()
            }
        })
        .show(ui, |plot_ui| {
            plot_ui.bar_chart(svc_chart);
            plot_ui.bar_chart(lib_chart);
        });

    // Summary line under chart
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!(
                "{} releases in the last {} days",
                total as i64, days
            ))
            .size(11.0)
            .color(Colors::text_muted(mode)),
        );
    });
}

// ── CI Pipeline Health (last 7 days) ───────────────────────────────────────

/// Stacked bar chart: workflow runs per day — success / failure / other
#[allow(dead_code)]
pub fn ci_health(ui: &mut egui::Ui, state: &AppState, mode: ThemeMode) {
    let runs = state.get_workflow_runs();
    let now = Utc::now().date_naive();
    let days = 7;

    let mut success = vec![0.0_f64; days];
    let mut failure = vec![0.0_f64; days];
    let mut other = vec![0.0_f64; days];

    for run in &runs {
        if run.status != "completed" {
            continue;
        }
        let date = run.updated_at.date_naive();
        let age = (now - date).num_days();
        if age >= 0 && age < days as i64 {
            let idx = (days as i64 - 1 - age) as usize;
            match run.conclusion.as_deref() {
                Some("success") => success[idx] += 1.0,
                Some("failure") => failure[idx] += 1.0,
                _ => other[idx] += 1.0,
            }
        }
    }

    let success_bars: Vec<Bar> = success
        .iter()
        .enumerate()
        .map(|(i, &c)| Bar::new(i as f64, c).width(0.5).name("Success"))
        .collect();

    let failure_bars: Vec<Bar> = failure
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            Bar::new(i as f64, c)
                .width(0.5)
                .base_offset(success[i])
                .name("Failure")
        })
        .collect();

    let other_bars: Vec<Bar> = other
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            Bar::new(i as f64, c)
                .width(0.5)
                .base_offset(success[i] + failure[i])
                .name("Other")
        })
        .collect();

    let day_labels: Vec<String> = (0..days)
        .map(|i| {
            let date = now - Duration::days(days as i64 - 1 - i as i64);
            date.format("%a").to_string()
        })
        .collect();

    Plot::new("ci_health")
        .height(160.0)
        .allow_zoom(false)
        .allow_drag(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .show_axes([false, true])
        .show_grid([false, true])
        .legend(Legend::default())
        .x_axis_formatter(move |mark, _range| {
            let idx = mark.value.round() as usize;
            if idx < day_labels.len() {
                day_labels[idx].clone()
            } else {
                String::new()
            }
        })
        .y_axis_formatter(|mark, _range| {
            let v = mark.value;
            if v >= 0.0 && v == v.round() {
                format!("{}", v as i64)
            } else {
                String::new()
            }
        })
        .show(ui, |plot_ui| {
            plot_ui.bar_chart(
                BarChart::new(success_bars)
                    .color(to_plot_color(Colors::GREEN))
                    .name("Success"),
            );
            plot_ui.bar_chart(
                BarChart::new(failure_bars)
                    .color(to_plot_color(Colors::RED))
                    .name("Failure"),
            );
            plot_ui.bar_chart(
                BarChart::new(other_bars)
                    .color(to_plot_color(Colors::GRAY))
                    .name("Other"),
            );
        });

    // Summary
    let total_success: f64 = success.iter().sum();
    let total_failure: f64 = failure.iter().sum();
    let total = total_success + total_failure + other.iter().sum::<f64>();
    let rate = if total > 0.0 {
        (total_success / total * 100.0) as i64
    } else {
        100
    };

    ui.horizontal(|ui| {
        let rate_color = if rate >= 90 {
            Colors::GREEN
        } else if rate >= 70 {
            Colors::YELLOW
        } else {
            Colors::RED
        };
        ui.label(
            egui::RichText::new(format!("{}% pass rate", rate))
                .size(12.0)
                .color(rate_color)
                .strong(),
        );
        ui.label(
            egui::RichText::new(format!("({} runs this week)", total as i64))
                .size(11.0)
                .color(Colors::text_muted(mode)),
        );
    });
}

// ── Deploy Heatmap (custom painted) ────────────────────────────────────────

/// Compact heatmap: services (rows) × days (cols), colored by deploy count
/// Heatmap of service releases (non-draft GitHub releases for K8s-deployed services).
pub fn deploy_heatmap(
    ui: &mut egui::Ui,
    state: &AppState,
    service_names: &[String],
    mode: ThemeMode,
) {
    let releases = state.get_releases();
    let now = Utc::now().date_naive();

    // Compute how many days fit in available width
    let cell_size = 14.0_f32;
    let label_width = 100.0_f32;
    let spacing = 2.0_f32;
    let available = ui.available_width() - label_width - 16.0;
    let days = ((available / (cell_size + spacing)) as usize).clamp(14, 90);

    // Only count non-draft releases for known services
    let mut activity: HashMap<String, Vec<u32>> = HashMap::new();
    // Store release details per (service, day_idx) for tooltips
    type ReleaseDetail = (String, String, String, String); // (tag, author, time, body)
    let mut details: HashMap<(String, usize), Vec<ReleaseDetail>> = HashMap::new();

    for release in &releases {
        if release.draft {
            continue;
        }
        if !service_names.iter().any(|s| s == &release.repo) {
            continue;
        }
        let date = release
            .published_at
            .unwrap_or(release.created_at)
            .date_naive();
        let age = (now - date).num_days();
        if age >= 0 && age < days as i64 {
            let idx = (days as i64 - 1 - age) as usize;
            let entry = activity
                .entry(release.repo.clone())
                .or_insert_with(|| vec![0; days]);
            entry[idx] += 1;
            let ts = release.published_at.unwrap_or(release.created_at);
            let time_str = ts.with_timezone(&chrono::Local).format("%H:%M").to_string();
            details
                .entry((release.repo.clone(), idx))
                .or_default()
                .push((
                    release.tag.clone(),
                    release.author.clone().unwrap_or_else(|| "—".into()),
                    time_str,
                    truncate_body(release.body.as_deref()),
                ));
        }
    }

    if activity.is_empty() {
        ui.label(
            egui::RichText::new("No app releases recently")
                .size(12.0)
                .color(Colors::text_muted(mode)),
        );
        return;
    }

    // Sort services alphabetically
    let mut services: Vec<String> = activity.keys().cloned().collect();
    services.sort();

    let total_width = label_width + (cell_size + spacing) * days as f32;
    let total_height = (cell_size + spacing) * services.len() as f32 + 20.0; // +20 for day headers

    let (response, painter) = ui.allocate_painter(
        egui::vec2(total_width, total_height),
        egui::Sense::hover() | egui::Sense::click(),
    );
    let origin = response.rect.min;

    // Day headers
    for d in 0..days {
        let x = origin.x + label_width + d as f32 * (cell_size + spacing);
        let date = now - Duration::days(days as i64 - 1 - d as i64);
        let label = date.format("%d").to_string();
        painter.text(
            egui::pos2(x + cell_size / 2.0, origin.y + 8.0),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(9.0),
            Colors::text_muted(mode),
        );
    }

    let header_y = 20.0;
    let has_pinned = get_pinned_tooltip(ui.ctx()).is_some();

    // Determine which row is hovered
    let hovered_row = response.hover_pos().and_then(|hover_pos| {
        services.iter().enumerate().find_map(|(row, _)| {
            let y = origin.y + header_y + row as f32 * (cell_size + spacing);
            let row_rect = egui::Rect::from_min_size(
                egui::pos2(origin.x, y),
                egui::vec2(total_width, cell_size),
            );
            if row_rect.contains(hover_pos) {
                Some(row)
            } else {
                None
            }
        })
    });

    // Rows
    for (row, service) in services.iter().enumerate() {
        let y = origin.y + header_y + row as f32 * (cell_size + spacing);
        let is_hovered = hovered_row == Some(row);

        // Service label
        let label = if service.len() > 12 {
            format!("{}…", &service[..11])
        } else {
            service.clone()
        };
        painter.text(
            egui::pos2(origin.x + label_width - 4.0, y + cell_size / 2.0),
            egui::Align2::RIGHT_CENTER,
            label,
            egui::FontId::proportional(10.0),
            Colors::text_secondary(mode),
        );

        let counts = activity.get(service).unwrap();
        for (d, &count) in counts.iter().enumerate() {
            let x = origin.x + label_width + d as f32 * (cell_size + spacing);
            let mut color = heatmap_color(count, mode);
            if is_hovered {
                color = lighten_color(color, mode);
            }
            let cell_rect =
                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_size, cell_size));
            painter.rect_filled(cell_rect, 3.0, color);

            if let Some(hover_pos) = response.hover_pos() {
                if cell_rect.contains(hover_pos) && count > 0 {
                    let date = now - Duration::days(days as i64 - 1 - d as i64);
                    let date_str = date.format("%b %d").to_string();
                    if let Some(releases) = details.get(&(service.clone(), d)) {
                        // Click → pin tooltip (only if not already pinned)
                        if !has_pinned && response.clicked() {
                            set_pinned_tooltip(
                                ui.ctx(),
                                Some(PinnedTooltip {
                                    heatmap_id: "heatmap".into(),
                                    name: service.clone(),
                                    date_str: date_str.clone(),
                                    count,
                                    releases: releases.clone(),
                                    tag_color: Colors::ACCENT,
                                    anchor_pos: hover_pos,
                                }),
                            );
                        }
                        // Hover tooltip only if nothing is pinned
                        if !has_pinned {
                            show_release_tooltip(
                                ui,
                                mode,
                                "heatmap_tip",
                                service,
                                &date_str,
                                count,
                                releases,
                                Colors::ACCENT,
                            );
                        }
                    }
                }
            }
        }
    }
}

fn heatmap_color(count: u32, mode: ThemeMode) -> egui::Color32 {
    if count == 0 {
        Colors::bg_elevated(mode)
    } else {
        // Intensity based on count: 1 = dim, 2+ = bright
        let alpha = match count {
            1 => 80,
            2 => 140,
            3 => 190,
            _ => 240,
        };
        egui::Color32::from_rgba_unmultiplied(
            Colors::GREEN.r(),
            Colors::GREEN.g(),
            Colors::GREEN.b(),
            alpha,
        )
    }
}

// ── Library Release Heatmap ─────────────────────────────────────────────────

/// Compact heatmap: library repos (rows) × days (cols), colored by release count.
/// Only shows repos that had at least one release in the window.
pub fn library_release_heatmap(ui: &mut egui::Ui, state: &AppState, mode: ThemeMode) {
    let releases = state.get_releases();
    let now = Utc::now().date_naive();

    // Compute how many days fit in available width
    let cell_size = 14.0_f32;
    let label_width = 120.0_f32;
    let spacing = 2.0_f32;
    let available = ui.available_width() - label_width - 16.0; // 16 for padding
    let days = ((available / (cell_size + spacing)) as usize).clamp(14, 90);

    // Bucket releases per repo per day (only non-draft releases)
    let mut activity: HashMap<String, Vec<u32>> = HashMap::new();
    type LibReleaseDetail = (String, String, String, String); // (tag, author, time, body)
    let mut details: HashMap<(String, usize), Vec<LibReleaseDetail>> = HashMap::new();

    for release in &releases {
        if release.draft {
            continue;
        }
        let date = release
            .published_at
            .unwrap_or(release.created_at)
            .date_naive();
        let age = (now - date).num_days();
        if age >= 0 && age < days as i64 {
            let idx = (days as i64 - 1 - age) as usize;
            let entry = activity
                .entry(release.repo.clone())
                .or_insert_with(|| vec![0; days]);
            entry[idx] += 1;
            let ts = release.published_at.unwrap_or(release.created_at);
            let time_str = ts.with_timezone(&chrono::Local).format("%H:%M").to_string();
            details
                .entry((release.repo.clone(), idx))
                .or_default()
                .push((
                    release.tag.clone(),
                    release.author.clone().unwrap_or_else(|| "—".into()),
                    time_str,
                    truncate_body(release.body.as_deref()),
                ));
        }
    }

    if activity.is_empty() {
        ui.label(
            egui::RichText::new("No library releases recently")
                .size(12.0)
                .color(Colors::text_muted(mode)),
        );
        return;
    }

    let mut repos: Vec<String> = activity.keys().cloned().collect();
    repos.sort();

    let total_width = label_width + (cell_size + spacing) * days as f32;
    let total_height = (cell_size + spacing) * repos.len() as f32 + 20.0;

    let (response, painter) = ui.allocate_painter(
        egui::vec2(total_width, total_height),
        egui::Sense::hover() | egui::Sense::click(),
    );
    let origin = response.rect.min;

    // Day headers
    for d in 0..days {
        let x = origin.x + label_width + d as f32 * (cell_size + spacing);
        let date = now - Duration::days(days as i64 - 1 - d as i64);
        let label = date.format("%d").to_string();
        painter.text(
            egui::pos2(x + cell_size / 2.0, origin.y + 8.0),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(9.0),
            Colors::text_muted(mode),
        );
    }

    let header_y = 20.0;
    let has_pinned = get_pinned_tooltip(ui.ctx()).is_some();

    // Determine which row is hovered
    let hovered_row = response.hover_pos().and_then(|hover_pos| {
        repos.iter().enumerate().find_map(|(row, _)| {
            let y = origin.y + header_y + row as f32 * (cell_size + spacing);
            let row_rect = egui::Rect::from_min_size(
                egui::pos2(origin.x, y),
                egui::vec2(total_width, cell_size),
            );
            if row_rect.contains(hover_pos) {
                Some(row)
            } else {
                None
            }
        })
    });

    for (row, repo) in repos.iter().enumerate() {
        let y = origin.y + header_y + row as f32 * (cell_size + spacing);
        let is_hovered = hovered_row == Some(row);

        let label = if repo.len() > 15 {
            format!("{}…", &repo[..14])
        } else {
            repo.clone()
        };
        painter.text(
            egui::pos2(origin.x + label_width - 4.0, y + cell_size / 2.0),
            egui::Align2::RIGHT_CENTER,
            label,
            egui::FontId::proportional(10.0),
            Colors::text_secondary(mode),
        );

        let counts = activity.get(repo).unwrap();
        for (d, &count) in counts.iter().enumerate() {
            let x = origin.x + label_width + d as f32 * (cell_size + spacing);
            let mut color = heatmap_color_purple(count, mode);
            if is_hovered {
                color = lighten_color(color, mode);
            }
            let cell_rect =
                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_size, cell_size));
            painter.rect_filled(cell_rect, 3.0, color);

            if let Some(hover_pos) = response.hover_pos() {
                if cell_rect.contains(hover_pos) && count > 0 {
                    let date = now - Duration::days(days as i64 - 1 - d as i64);
                    let date_str = date.format("%b %d").to_string();
                    if let Some(rels) = details.get(&(repo.clone(), d)) {
                        // Click → pin tooltip (only if not already pinned)
                        if !has_pinned && response.clicked() {
                            set_pinned_tooltip(
                                ui.ctx(),
                                Some(PinnedTooltip {
                                    heatmap_id: "lib_heatmap".into(),
                                    name: repo.clone(),
                                    date_str: date_str.clone(),
                                    count,
                                    releases: rels.clone(),
                                    tag_color: Colors::PURPLE,
                                    anchor_pos: hover_pos,
                                }),
                            );
                        }
                        // Hover tooltip only if nothing is pinned
                        if !has_pinned {
                            show_release_tooltip(
                                ui,
                                mode,
                                "lib_heatmap_tip",
                                repo,
                                &date_str,
                                count,
                                rels,
                                Colors::PURPLE,
                            );
                        }
                    }
                }
            }
        }
    }

    // Summary
    let total_releases: u32 = activity.values().flat_map(|v| v.iter()).sum();
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(format!(
            "{} releases across {} repos in the last {} days",
            total_releases,
            repos.len(),
            days
        ))
        .size(11.0)
        .color(Colors::text_muted(mode)),
    );
}

fn heatmap_color_purple(count: u32, mode: ThemeMode) -> egui::Color32 {
    if count == 0 {
        Colors::bg_elevated(mode)
    } else {
        let alpha = match count {
            1 => 80,
            2 => 140,
            3 => 190,
            _ => 240,
        };
        egui::Color32::from_rgba_unmultiplied(
            Colors::PURPLE.r(),
            Colors::PURPLE.g(),
            Colors::PURPLE.b(),
            alpha,
        )
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Truncate release body to first ~500 chars for tooltip display.
fn truncate_body(body: Option<&str>) -> String {
    match body {
        None | Some("") => String::new(),
        Some(b) => {
            let trimmed = b.trim();
            let max_len = 500;
            if trimmed.len() <= max_len {
                trimmed.to_string()
            } else {
                // Find a char boundary at or before max_len bytes
                let mut end = max_len;
                while end > 0 && !trimmed.is_char_boundary(end) {
                    end -= 1;
                }
                // Avoid truncating inside a markdown link — find last complete line
                let slice = &trimmed[..end];
                let safe_end = if let Some(pos) = slice.rfind('\n') {
                    pos
                } else {
                    end
                };
                format!("{}…", &trimmed[..safe_end])
            }
        }
    }
}

/// Slightly lighten a color for hovered-row cells.
fn lighten_color(color: egui::Color32, _mode: ThemeMode) -> egui::Color32 {
    let r = color.r() as u16;
    let g = color.g() as u16;
    let b = color.b() as u16;
    let boost = 30_u16;
    egui::Color32::from_rgba_unmultiplied(
        (r + boost).min(255) as u8,
        (g + boost).min(255) as u8,
        (b + boost).min(255) as u8,
        color.a(),
    )
}

/// Render release content (shared between hover tooltip and pinned popup).
fn render_release_content(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    id_prefix: &str,
    releases: &[(String, String, String, String)],
    tag_color: egui::Color32,
) {
    for (i, (tag, author, time, body)) in releases.iter().enumerate() {
        if i > 0 {
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
        }

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(
                egui::RichText::new(time)
                    .monospace()
                    .size(11.0)
                    .color(Colors::text_muted(mode)),
            );
            ui.label(
                egui::RichText::new(tag)
                    .monospace()
                    .size(11.0)
                    .color(tag_color),
            );
            ui.label(
                egui::RichText::new(format!("by @{}", author))
                    .size(11.0)
                    .color(Colors::text_secondary(mode)),
            );
        });

        if !body.is_empty() {
            ui.add_space(2.0);
            ui.indent(format!("{}_{}", id_prefix, i), |ui| {
                let processed = compress_bare_urls(body);
                let mut cache = CommonMarkCache::default();
                CommonMarkViewer::new()
                    .max_image_width(Some(440))
                    .show(ui, &mut cache, &processed);
            });
        }
    }
}

/// Show a release tooltip at the pointer with tag, author, time, and notes.
#[allow(clippy::too_many_arguments)]
fn show_release_tooltip(
    ui: &egui::Ui,
    mode: ThemeMode,
    tip_id: &str,
    name: &str,
    date_str: &str,
    count: u32,
    releases: &[(String, String, String, String)],
    tag_color: egui::Color32,
) {
    egui::show_tooltip_at_pointer(ui.ctx(), ui.layer_id(), egui::Id::new(tip_id), |ui| {
        ui.set_max_width(480.0);
        ui.label(
            egui::RichText::new(format!(
                "{} — {} ({} release{})",
                name,
                date_str,
                count,
                if count == 1 { "" } else { "s" }
            ))
            .strong()
            .size(13.0),
        );
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);
        render_release_content(ui, mode, &format!("tip_{}", tip_id), releases, tag_color);
    });
}

#[allow(dead_code)]
fn to_plot_color(c: egui::Color32) -> eframe::egui::Color32 {
    c
}
