use crate::config::Config;
use crate::models::image::ImageRef;
use crate::state::AppState;
use crate::ui::app_table;
use crate::ui::filters::GlobalFilters;

/// Temp-data key: a view that links to a service may ask for a tab.
pub const PENDING_SERVICE_TAB: &str = "pending_service_tab";
use crate::ui::icons;
use crate::ui::log_viewer::LogViewer;
use crate::ui::resources::ResourceViewer;
use crate::ui::sidebar::View;
use crate::ui::theme::{self, Colors, ThemeMode};
use crate::ui::widgets;
use eframe::egui;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceTab {
    #[default]
    Deployments,
    Logs,
    Resources,
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    service_name: &str,
    filters: &GlobalFilters,
    mode: ThemeMode,
    log_viewer: &mut LogViewer,
    resource_viewer: &mut ResourceViewer,
    active_tab: &mut ServiceTab,
    config: &Option<Config>,
    runtime: &tokio::runtime::Runtime,
) -> Option<View> {
    let mut nav_request: Option<View> = None;
    let is_vault = service_name == "vault";

    // ── Back button + title ────────────────────────────────────────────
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let back = ui.add(
            egui::Button::new(
                egui::RichText::new("← Back")
                    .color(Colors::text_secondary(mode))
                    .size(13.0),
            )
            .fill(Colors::bg_elevated(mode))
            .corner_radius(20.0),
        );
        if back.clicked() {
            let home = View::Apps;
            nav_request = Some(if is_vault { View::Vault } else { home });
        }
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(service_name)
                .size(26.0)
                .color(Colors::text_primary(mode))
                .strong(),
        );
        let copy_resp = ui.add(
            egui::Label::new(icons::icon(icons::COPY, 14.0, Colors::text_muted(mode)))
                .sense(egui::Sense::click()),
        );
        if copy_resp.clicked() {
            ui.ctx().copy_text(service_name.to_string());
        }
        if copy_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        copy_resp.on_hover_text("Copy name");
    });

    ui.add_space(12.0);

    // ── Tab bar ────────────────────────────────────────────────────────
    // Vault has no HelmReleases — skip Deployments tab, default to Logs
    if is_vault && *active_tab == ServiceTab::Deployments {
        *active_tab = ServiceTab::Logs;
    }
    ui.horizontal(|ui| {
        if !is_vault
            && theme::pill_button(
                ui,
                mode,
                "Deployments",
                *active_tab == ServiceTab::Deployments,
            )
        {
            *active_tab = ServiceTab::Deployments;
        }
        if theme::pill_button(ui, mode, "Logs", *active_tab == ServiceTab::Logs) {
            *active_tab = ServiceTab::Logs;
        }
        if theme::pill_button(ui, mode, "Resources", *active_tab == ServiceTab::Resources) {
            *active_tab = ServiceTab::Resources;
        }
    });

    ui.add_space(8.0);

    // Stop background work for inactive tabs
    if *active_tab != ServiceTab::Logs {
        log_viewer.stop_stream();
    }
    if *active_tab != ServiceTab::Resources {
        resource_viewer.stop();
    }

    match active_tab {
        ServiceTab::Deployments => {
            egui::ScrollArea::vertical().show(ui, |ui| {
                show_deployments(ui, state, service_name, filters, mode);
            });
        }
        ServiceTab::Logs => {
            let active_envs = filters.active_envs();
            let order = crate::ui::filters::env_order();
            let order: Vec<&str> = order.iter().map(String::as_str).collect();
            let ctx = crate::ui::log_viewer::LogViewerContext {
                state,
                service_name,
                config,
                runtime,
                mode,
                env_order: &order,
                active_envs: &active_envs,
            };
            log_viewer.show(ui, &ctx);
        }
        ServiceTab::Resources => {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let active_envs = filters.active_envs();
                resource_viewer.show(ui, state, service_name, &active_envs, config, runtime, mode);
            });
        }
    }

    nav_request
}

fn show_deployments(
    ui: &mut egui::Ui,
    state: &AppState,
    service_name: &str,
    filters: &GlobalFilters,
    mode: ThemeMode,
) {
    // Fleet instances exist in exactly one environment — a cross-env
    // comparison table is meaningless for them.
    let is_project = service_name.starts_with("project-");

    // ── Version comparison table ───────────────────────────────────────
    if !is_project {
        show_version_comparison(ui, state, service_name, filters, mode);
        ui.add_space(8.0);
    }

    // ── Recent CI Runs ─────────────────────────────────────────────────
    let runs = state.get_workflow_runs_for_repo(&crate::fleet::repo_for_service(service_name));
    if !runs.is_empty() {
        let recent: Vec<_> = runs.iter().take(5).cloned().collect();
        theme::section_header(ui, mode, "Recent CI Runs");
        theme::card(ui, mode, |ui| {
            widgets::workflow_table(ui, mode, &format!("svc_ci_{}", service_name), &recent);
        });
        ui.add_space(8.0);
    }

    show_env_cards(ui, state, service_name, filters, mode);
}

fn show_version_comparison(
    ui: &mut egui::Ui,
    state: &AppState,
    service_name: &str,
    filters: &GlobalFilters,
    mode: ThemeMode,
) {
    theme::section_header(ui, mode, "Version Comparison");

    theme::card(ui, mode, |ui| {
        egui::Grid::new("svc_version_grid")
            .min_col_width(100.0)
            .spacing([24.0, 8.0])
            .show(ui, |ui| {
                // Header row
                ui.label(
                    egui::RichText::new("ENVIRONMENT")
                        .color(Colors::text_muted(mode))
                        .strong()
                        .size(11.0),
                );
                ui.label(
                    egui::RichText::new("CHART")
                        .color(Colors::text_muted(mode))
                        .strong()
                        .size(11.0),
                );
                ui.label(
                    egui::RichText::new("IMAGE")
                        .color(Colors::text_muted(mode))
                        .strong()
                        .size(11.0),
                );
                ui.label(
                    egui::RichText::new("STATUS")
                        .color(Colors::text_muted(mode))
                        .strong()
                        .size(11.0),
                );
                ui.end_row();

                let active_envs = filters.active_envs();
                for env_name in crate::ui::filters::env_order() {
                    let env_name = &env_name.as_str();
                    if !active_envs.contains(env_name) {
                        continue;
                    }
                    let env_state = state.get_environment(env_name);
                    let hr = env_state
                        .as_ref()
                        .and_then(|e| e.helm_releases.iter().find(|hr| hr.name == service_name));

                    // Env label pill
                    theme::status_pill(
                        ui,
                        &theme::env_label(env_name),
                        theme::env_color(env_name, mode),
                    );

                    match hr {
                        None => {
                            ui.label(egui::RichText::new("—").color(Colors::text_muted(mode)));
                            ui.label(egui::RichText::new("—").color(Colors::text_muted(mode)));
                            theme::status_pill(ui, "Not Deployed", Colors::GRAY);
                        }
                        Some(hr) => {
                            // Chart version
                            let chart_label = hr
                                .chart_version
                                .as_deref()
                                .map(widgets::shorten_hashes)
                                .unwrap_or_else(|| icons::MINUS.to_string());
                            ui.label(
                                egui::RichText::new(chart_label)
                                    .color(Colors::text_secondary(mode))
                                    .monospace()
                                    .size(12.0),
                            );

                            // Running version, with its release when known
                            let repo = &crate::fleet::repo_for_service(service_name);
                            let release_tag = state.find_release_for_service(
                                repo,
                                &hr.full_commit_sha().unwrap_or_default(),
                                hr.chart_version.as_deref(),
                            );
                            let version = widgets::version_label(state, repo, hr);
                            ui.label(
                                egui::RichText::new(version)
                                    .color(if release_tag.is_some() {
                                        Colors::GREEN
                                    } else {
                                        Colors::text_secondary(mode)
                                    })
                                    .monospace()
                                    .size(12.0),
                            );

                            // Status pill
                            let status_col = widgets::status_color(&hr.status, mode);
                            let icon_ch = widgets::status_icon_char(&hr.status);
                            theme::status_pill(
                                ui,
                                &format!("{} {}", icon_ch, hr.status),
                                status_col,
                            );
                        }
                    }
                    ui.end_row();
                }
            });
    });
}

/// One card per environment where the service is deployed.
fn show_env_cards(
    ui: &mut egui::Ui,
    state: &AppState,
    service_name: &str,
    filters: &GlobalFilters,
    mode: ThemeMode,
) {
    let active_envs = filters.active_envs();
    for env_name in crate::ui::filters::env_order() {
        let env_name = &env_name.as_str();
        if !active_envs.contains(env_name) {
            continue;
        }
        let env_state = state.get_environment(env_name);
        let env = match env_state {
            Some(e) => e,
            None => continue,
        };

        let hr = match env.helm_releases.iter().find(|hr| hr.name == service_name) {
            Some(hr) => hr,
            None => continue,
        };

        let ec = theme::env_color(env_name, mode);

        theme::section_header(ui, mode, &theme::env_label_full(env_name));

        theme::card(ui, mode, |ui| {
            // ── HelmRelease info ──
            ui.horizontal(|ui| {
                theme::status_pill(ui, &theme::env_label(env_name), ec);
                ui.add_space(8.0);
                let sc = widgets::status_color(&hr.status, mode);
                theme::status_pill(
                    ui,
                    &format!("{} {}", widgets::status_icon_char(&hr.status), hr.status),
                    sc,
                );
            });

            ui.add_space(8.0);

            egui::Grid::new(format!("hr_info_{}_{}", env_name, service_name))
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    let lbl = |ui: &mut egui::Ui, t: &str| {
                        ui.label(
                            egui::RichText::new(t)
                                .color(Colors::text_muted(mode))
                                .size(12.0),
                        );
                    };
                    let val = |ui: &mut egui::Ui, t: &str| {
                        ui.label(
                            egui::RichText::new(t)
                                .color(Colors::text_primary(mode))
                                .size(13.0),
                        );
                    };

                    lbl(ui, "Namespace");
                    val(ui, &hr.namespace);
                    ui.end_row();

                    lbl(ui, "Chart");
                    let chart = hr
                        .chart_version
                        .as_deref()
                        .map(widgets::shorten_hashes)
                        .unwrap_or_else(|| icons::MINUS.to_string());
                    ui.label(
                        egui::RichText::new(chart)
                            .color(Colors::text_primary(mode))
                            .size(13.0),
                    )
                    .on_hover_text(hr.chart_version.clone().unwrap_or_default());
                    ui.end_row();

                    if let Some(ref tag) = hr.image_tag {
                        lbl(ui, "Image Tag");
                        ui.label(
                            egui::RichText::new(widgets::version_text(tag, None))
                                .color(Colors::text_primary(mode))
                                .size(13.0),
                        )
                        .on_hover_text(tag);
                        ui.end_row();
                    }

                    if let Some(release_tag) = state.find_release_for_service(
                        &crate::fleet::repo_for_service(service_name),
                        &hr.full_commit_sha().unwrap_or_default(),
                        hr.chart_version.as_deref(),
                    ) {
                        lbl(ui, "Release");
                        ui.label(
                            egui::RichText::new(&release_tag)
                                .color(Colors::GREEN)
                                .strong()
                                .size(13.0),
                        );
                        ui.end_row();
                    }

                    if let Some(ref msg) = hr.status_message {
                        lbl(ui, "Message");
                        ui.label(
                            egui::RichText::new(msg)
                                .color(Colors::text_secondary(mode))
                                .size(12.0),
                        );
                        ui.end_row();
                    }

                    if let Some(ref t) = hr.last_transition_time {
                        lbl(ui, "Last Transition");
                        val(ui, &widgets::time_ago(t));
                        ui.end_row();
                    }
                });

            ui.add_space(12.0);

            // ── Components (project instances) ──
            if hr.is_fleet_instance() && !hr.values_images.is_empty() {
                ui.label(
                    egui::RichText::new("Components")
                        .color(Colors::text_secondary(mode))
                        .strong()
                        .size(12.0),
                );
                ui.add_space(6.0);
                egui::Grid::new(format!("components_{}_{}", env_name, service_name))
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for (key, reference) in &hr.values_images {
                            let Some(component) = app_table::component_of_key(key) else {
                                continue;
                            };
                            let image = ImageRef::parse(reference);
                            let repo = &crate::fleet::repo_for_service(&component);
                            // Version identity is the digest: name it by the
                            // best tag seen for that digest in any env.
                            let raw = image
                                .digest
                                .as_deref()
                                .and_then(|d| state.best_name_for_digest(d))
                                .or_else(|| image.tag.clone())
                                .or_else(|| image.digest.as_deref().map(widgets::digest_short))
                                .unwrap_or_else(|| "?".to_string());
                            let version = widgets::resolved_version(state, repo, &raw, None);
                            ui.label(
                                egui::RichText::new(component)
                                    .color(Colors::text_muted(mode))
                                    .size(12.0),
                            );
                            ui.label(
                                egui::RichText::new(version)
                                    .color(Colors::text_primary(mode))
                                    .monospace()
                                    .size(12.0),
                            )
                            .on_hover_text(reference);
                            ui.end_row();
                        }
                    });
                ui.add_space(12.0);
            }

            // ── Pod table ──
            // Fleet instances: the HR lives in the operator namespace but pods are in "project-{uuid}"
            let pod_namespace = if hr.is_fleet_instance() {
                &hr.name
            } else {
                &hr.namespace
            };
            let pods: Vec<_> = env
                .pods
                .iter()
                .filter(|p| p.namespace == *pod_namespace)
                .collect();

            if pods.is_empty() {
                ui.label(
                    egui::RichText::new("No pods found")
                        .color(Colors::text_muted(mode))
                        .size(12.0),
                );
            } else {
                ui.label(
                    egui::RichText::new(format!("Pods ({})", pods.len()))
                        .color(Colors::text_secondary(mode))
                        .strong()
                        .size(12.0),
                );
                ui.add_space(6.0);

                egui::Grid::new(format!("pods_{}_{}", env_name, service_name))
                    .min_col_width(80.0)
                    .spacing([20.0, 4.0])
                    .show(ui, |ui| {
                        // Header
                        let hdr = |ui: &mut egui::Ui, t: &str| {
                            ui.label(
                                egui::RichText::new(t.to_uppercase())
                                    .color(Colors::text_muted(mode))
                                    .strong()
                                    .size(10.0),
                            );
                        };
                        hdr(ui, "Name");
                        hdr(ui, "Status");
                        hdr(ui, "Ready");
                        hdr(ui, "Restarts");
                        hdr(ui, "Age");
                        ui.end_row();

                        for pod in &pods {
                            // Row with hover highlight
                            let row_resp = ui.label(
                                egui::RichText::new(&pod.name)
                                    .color(Colors::text_primary(mode))
                                    .size(12.0),
                            );

                            let pod_color = if pod.is_problematic() {
                                Colors::RED
                            } else if pod.phase == "Running" && pod.ready {
                                Colors::GREEN
                            } else {
                                Colors::YELLOW
                            };

                            theme::status_pill(ui, &pod.status, pod_color);

                            ui.label(
                                egui::RichText::new(&pod.ready_count)
                                    .color(Colors::text_secondary(mode))
                                    .size(12.0),
                            );

                            let restart_col = if pod.restarts > 5 {
                                Colors::RED
                            } else if pod.restarts > 0 {
                                Colors::ORANGE
                            } else {
                                Colors::text_muted(mode)
                            };
                            ui.label(
                                egui::RichText::new(pod.restarts.to_string())
                                    .color(restart_col)
                                    .size(12.0),
                            );

                            if let Some(ref started) = pod.started_at {
                                ui.label(
                                    egui::RichText::new(widgets::time_ago(started))
                                        .color(Colors::text_muted(mode))
                                        .size(12.0),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new("—")
                                        .color(Colors::text_muted(mode))
                                        .size(12.0),
                                );
                            }

                            ui.end_row();

                            // Hover tooltip with extra details
                            if row_resp.hovered() {
                                egui::show_tooltip_at_pointer(
                                    ui.ctx(),
                                    ui.layer_id(),
                                    ui.id().with(format!("pod_tip_{}", pod.name)),
                                    |ui: &mut egui::Ui| {
                                        ui.label(
                                            egui::RichText::new(&pod.name)
                                                .strong()
                                                .color(Colors::text_primary(mode)),
                                        );
                                        ui.label(format!("Phase: {}", pod.phase));
                                        ui.label(format!("Ready: {}", pod.ready_count));
                                        if let Some(ref node) = pod.node_name {
                                            ui.label(format!("Node: {}", node));
                                        }
                                        if let Some(ref reason) = pod.reason {
                                            ui.label(
                                                egui::RichText::new(format!("Reason: {}", reason))
                                                    .color(Colors::RED),
                                            );
                                        }
                                        if pod.containers.is_empty() {
                                            for img in &pod.images {
                                                let r = ImageRef::parse(img);
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "Image: {}:{}",
                                                        r.name,
                                                        widgets::version_text(
                                                            r.tag.as_deref().unwrap_or("latest"),
                                                            None
                                                        )
                                                    ))
                                                    .monospace()
                                                    .size(11.0)
                                                    .color(Colors::text_muted(mode)),
                                                );
                                            }
                                        }
                                        for c in &pod.containers {
                                            let r = ImageRef::parse(&c.image);
                                            let digest = ImageRef::parse(&c.image_id)
                                                .digest
                                                .map(|d| format!(" @{}", widgets::digest_short(&d)))
                                                .unwrap_or_default();
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "{}{}: {}:{}{}",
                                                    if c.init { "init " } else { "" },
                                                    c.name,
                                                    r.name,
                                                    widgets::version_text(
                                                        r.tag.as_deref().unwrap_or("latest"),
                                                        None
                                                    ),
                                                    digest
                                                ))
                                                .monospace()
                                                .size(11.0)
                                                .color(if c.ready {
                                                    Colors::text_muted(mode)
                                                } else {
                                                    Colors::YELLOW
                                                }),
                                            );
                                        }
                                    },
                                );
                            }
                        }
                    });
            }
        });

        ui.add_space(4.0);
    }
}
