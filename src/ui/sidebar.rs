use crate::state::AppState;
use crate::ui::filters::{self, GlobalFilters};
use crate::ui::icons;
use crate::ui::theme::{self, Colors, ThemeMode};
use eframe::egui;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum View {
    /// The Status home
    #[default]
    Apps,
    AppDetail(String),
    Releases,
    Ci,
    Infra,
    Libraries,
    PullRequests,
    Issues,
    Vault,
    Settings,
}

impl View {
    pub fn as_str(&self) -> String {
        match self {
            View::Apps => "apps".to_string(),
            View::AppDetail(name) => format!("app_detail:{}", name),
            View::Releases => "releases".to_string(),
            View::Ci => "ci".to_string(),
            View::Infra => "infra".to_string(),
            View::Libraries => "libraries".to_string(),
            View::PullRequests => "pull_requests".to_string(),
            View::Issues => "issues".to_string(),
            View::Vault => "vault".to_string(),
            View::Settings => "settings".to_string(),
        }
    }

    pub fn from_str(s: &str) -> Option<View> {
        Some(match s {
            // Views folded into the Status home
            "overview" | "problems" | "apps" => View::Apps,
            "releases" => View::Releases,
            "ci" => View::Ci,
            "infra" => View::Infra,
            "libraries" => View::Libraries,
            "pull_requests" => View::PullRequests,
            "issues" => View::Issues,
            "vault" => View::Vault,
            "settings" => View::Settings,
            _ if s.starts_with("app_detail:") => {
                View::AppDetail(s.strip_prefix("app_detail:").unwrap().to_string())
            }
            _ => return None,
        })
    }
}

pub struct Sidebar {
    pub current_view: View,
    pub theme_mode: ThemeMode,
    history_back: Vec<View>,
    history_forward: Vec<View>,
    logo_texture: Option<egui::TextureHandle>,
    /// Update state: None = idle, Some(status_string) = in progress or done
    update_status: Option<String>,
}

impl Sidebar {
    pub fn new() -> Self {
        Self {
            current_view: View::default(),
            theme_mode: ThemeMode::default(),
            history_back: Vec::new(),
            history_forward: Vec::new(),
            logo_texture: None,
            update_status: None,
        }
    }

    /// Navigate to a new view, pushing current onto back stack
    pub fn navigate(&mut self, view: View) {
        if view != self.current_view {
            self.history_back.push(self.current_view.clone());
            self.current_view = view;
            self.history_forward.clear();
            // Cap history at 50
            if self.history_back.len() > 50 {
                self.history_back.remove(0);
            }
        }
    }

    pub fn go_back(&mut self) {
        if let Some(prev) = self.history_back.pop() {
            self.history_forward.push(self.current_view.clone());
            self.current_view = prev;
        }
    }

    pub fn go_forward(&mut self) {
        if let Some(next) = self.history_forward.pop() {
            self.history_back.push(self.current_view.clone());
            self.current_view = next;
        }
    }

    pub fn can_go_back(&self) -> bool {
        !self.history_back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.history_forward.is_empty()
    }

    /// Show the Spotify-style top navigation bar (back/forward + context title + env filters)
    pub fn show_top_bar(
        &mut self,
        ctx: &egui::Context,
        state: &AppState,
        filters: &mut GlobalFilters,
        config: &Option<crate::config::Config>,
        runtime: &tokio::runtime::Runtime,
        toaster: &mut crate::ui::toast::Toaster,
    ) {
        let mode = self.theme_mode;
        egui::TopBottomPanel::top("nav_bar")
            .frame(
                egui::Frame::default()
                    .fill(Colors::bg_base(mode))
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Back button
                    let back_color = if self.can_go_back() {
                        Colors::text_primary(mode)
                    } else {
                        Colors::text_muted(mode)
                    };
                    let back_btn = ui.add_enabled(
                        self.can_go_back(),
                        egui::Button::new(super::icons::icon(
                            super::icons::CHEVRON_LEFT,
                            16.0,
                            back_color,
                        ))
                        .fill(Colors::bg_elevated(mode))
                        .corner_radius(16),
                    );
                    if back_btn.clicked() {
                        self.go_back();
                    }
                    back_btn.on_hover_text("Back");

                    // Forward button
                    let fwd_color = if self.can_go_forward() {
                        Colors::text_primary(mode)
                    } else {
                        Colors::text_muted(mode)
                    };
                    let fwd_btn = ui.add_enabled(
                        self.can_go_forward(),
                        egui::Button::new(super::icons::icon(
                            super::icons::CHEVRON_RIGHT,
                            16.0,
                            fwd_color,
                        ))
                        .fill(Colors::bg_elevated(mode))
                        .corner_radius(16),
                    );
                    if fwd_btn.clicked() {
                        self.go_forward();
                    }
                    fwd_btn.on_hover_text("Forward");

                    ui.add_space(12.0);

                    // Context title
                    let title = match &self.current_view {
                        View::Apps => "Status".to_string(),
                        View::AppDetail(name) => name.clone(),
                        View::Releases => "Releases".to_string(),
                        View::Ci => "CI".to_string(),
                        View::Infra => "Infrastructure".to_string(),
                        View::Libraries => "Libraries".to_string(),
                        View::PullRequests => "Pull Requests".to_string(),
                        View::Issues => "Issues".to_string(),
                        View::Vault => "Vault".to_string(),
                        View::Settings => "Settings".to_string(),
                    };
                    ui.label(
                        egui::RichText::new(title)
                            .size(16.0)
                            .color(Colors::text_primary(mode))
                            .strong(),
                    );

                    ui.separator();

                    filters::show_env_filter_bar(ui, state, filters, mode);

                    // Poll status + manual refresh (right-aligned)
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let refresh_btn = ui.add(
                            egui::Button::new(super::icons::icon(
                                super::icons::ARROWS_ROTATE,
                                14.0,
                                Colors::text_secondary(mode),
                            ))
                            .fill(Colors::bg_elevated(mode))
                            .corner_radius(16),
                        );
                        if refresh_btn.clicked() {
                            state.request_refresh();
                        }
                        refresh_btn.on_hover_text("Refresh GitHub + GitOps data now");

                        let polling = config
                            .as_ref()
                            .map(|c| c.polling.clone())
                            .unwrap_or_default();
                        let (last_github, last_gitops, last_k8s, rate_core, rate_search, head) =
                            state.read(|s| {
                                (
                                    s.last_github_poll,
                                    s.last_gitops_poll,
                                    s.last_k8s_poll,
                                    s.gh_rate_core,
                                    s.gh_rate_search,
                                    s.gitops_head.clone(),
                                )
                            });
                        let run_mode = state.run_mode();
                        let last_vault = state.vault_last_poll();
                        let rate_suffix = rate_core
                            .map(|r| format!(" · API {}", format_count(r.remaining)))
                            .unwrap_or_default();
                        let (label, label_color) = match &run_mode {
                            crate::instance::RunMode::Passive { reason } => {
                                (format!("passive: {}", reason), Colors::ORANGE)
                            }
                            crate::instance::RunMode::Active => {
                                let label = match last_github {
                                    Some(t) => format!(
                                        "GitHub {} · every {}{}",
                                        crate::ui::widgets::time_ago(&t),
                                        format_interval(polling.github_interval_secs),
                                        rate_suffix
                                    ),
                                    None => "GitHub: first poll…".to_string(),
                                };
                                let rate_low = rate_core
                                    .is_some_and(|r| r.limit > 0 && r.remaining < r.limit / 5);
                                let color = if rate_low {
                                    Colors::ORANGE
                                } else {
                                    Colors::text_muted(mode)
                                };
                                (label, color)
                            }
                        };
                        ui.label(egui::RichText::new(label).size(11.0).color(label_color))
                            .on_hover_ui(|ui| {
                                let line = |ui: &mut egui::Ui,
                                            name: &str,
                                            last: Option<chrono::DateTime<chrono::Utc>>,
                                            every: Option<u64>| {
                                    let ago = last
                                        .map(|t| crate::ui::widgets::time_ago(&t))
                                        .unwrap_or_else(|| "never".to_string());
                                    let every = every
                                        .map(|e| format!(" (every {})", format_interval(e)))
                                        .unwrap_or_default();
                                    ui.label(
                                        egui::RichText::new(format!("{}: {}{}", name, ago, every))
                                            .size(11.0)
                                            .color(Colors::text_secondary(mode)),
                                    );
                                };
                                if let crate::instance::RunMode::Passive { reason } = &run_mode {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Passive: {}. Reading the shared cache and mirror only.",
                                            reason
                                        ))
                                        .size(11.0)
                                        .color(Colors::ORANGE),
                                    );
                                }
                                line(ui, "K8s", last_k8s, Some(polling.k8s_interval_secs));
                                line(
                                    ui,
                                    "GitHub",
                                    last_github,
                                    Some(polling.github_interval_secs),
                                );
                                line(ui, "GitOps", last_gitops, None);
                                let mirror_line = match &head {
                                    Some(h) => format!(
                                        "Mirror: fetched {} · main {}",
                                        crate::ui::widgets::time_ago(&h.fetched_at),
                                        crate::ui::widgets::version_text(&h.sha, None)
                                    ),
                                    None => "Mirror: not fetched yet".to_string(),
                                };
                                ui.label(
                                    egui::RichText::new(mirror_line)
                                        .size(11.0)
                                        .color(Colors::text_secondary(mode)),
                                );
                                line(ui, "Vault", last_vault, Some(polling.vault_interval_secs));
                                let rate_line = |ui: &mut egui::Ui,
                                                 name: &str,
                                                 rl: Option<crate::state::RateLimitInfo>| {
                                    let Some(rl) = rl else { return };
                                    let reset = rl
                                        .reset
                                        .map(|t| {
                                            format!(
                                                ", resets in {}m",
                                                (t - chrono::Utc::now()).num_minutes().max(0)
                                            )
                                        })
                                        .unwrap_or_default();
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "API {}: {}/{} left{}",
                                            name,
                                            format_count(rl.remaining),
                                            format_count(rl.limit),
                                            reset
                                        ))
                                        .size(11.0)
                                        .color(Colors::text_secondary(mode)),
                                    );
                                };
                                rate_line(ui, "core", rate_core);
                                rate_line(ui, "search", rate_search);
                            });

                    });
                });
            });
        egui::TopBottomPanel::top("action_bar")
            .frame(
                egui::Frame::default()
                    .fill(Colors::bg_base(mode))
                    .inner_margin(egui::Margin::symmetric(16, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    crate::ui::topbar_actions::show(ui, state, config, mode, runtime, toaster);
                });
            });
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        state: &crate::state::AppState,
        config: &Option<crate::config::Config>,
        open_pr_count: usize,
    ) {
        let mode = self.theme_mode;
        // Cache update info outside the UI layout to avoid lock contention
        let update_available = state.get_update_available();
        let config_for_update = config.clone();

        // Poll async update status from background thread
        if self.update_status.is_some() && self.update_status.as_deref() != Some("restart") {
            if let Some(status) =
                ctx.data_mut(|d| d.get_temp::<String>(egui::Id::new("update_status")))
            {
                self.update_status = Some(status);
            }
        }

        egui::SidePanel::left("sidebar")
            .resizable(false)
            .default_width(200.0)
            .frame(
                egui::Frame::default()
                    .fill(Colors::bg_sidebar(mode))
                    .stroke(egui::Stroke::new(0.5_f32, Colors::border(mode)))
                    .inner_margin(egui::Margin::symmetric(12, 16)),
            )
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    // Logo / App name
                    ui.horizontal(|ui| {
                        let logo_texture: &egui::TextureHandle =
                            self.logo_texture.get_or_insert_with(|| {
                                let image_data = image::load_from_memory(include_bytes!(
                                    "../../assets/logomark.png"
                                ))
                                .expect("Failed to load logomark")
                                .to_rgba8();
                                let size = [image_data.width() as _, image_data.height() as _];
                                let pixels = image_data.into_raw();
                                ui.ctx().load_texture(
                                    "logomark",
                                    egui::ColorImage::from_rgba_unmultiplied(size, &pixels),
                                    egui::TextureOptions::LINEAR,
                                )
                            });
                        let logo_size = egui::vec2(24.0, 24.0);
                        ui.image(egui::load::SizedTexture::new(logo_texture.id(), logo_size));
                        ui.label(
                            egui::RichText::new(crate::branding::DISPLAY_NAME)
                                .size(20.0)
                                .color(Colors::text_primary(mode))
                                .strong(),
                        );
                    });
                    ui.add_space(20.0);

                    let badge =
                        |count: usize, color: egui::Color32| (count > 0).then_some((count, color));
                    self.nav_item(ui, mode, icons::CHART_LINE, "Status", View::Apps, None);
                    let release_prs = state.get_open_gitops_prs().len();
                    self.nav_item(
                        ui,
                        mode,
                        icons::TAG,
                        "Releases",
                        View::Releases,
                        badge(release_prs, Colors::ORANGE),
                    );
                    let running = state.get_in_progress_runs().len();
                    self.nav_item(
                        ui,
                        mode,
                        icons::ARROWS_ROTATE,
                        "CI",
                        View::Ci,
                        badge(running, Colors::BLUE),
                    );
                    let sealed_pods: usize =
                        state.sealed_summary().iter().map(|s| s.sealed_pods).sum();
                    self.nav_item(
                        ui,
                        mode,
                        icons::LOCK,
                        "Vault",
                        View::Vault,
                        badge(sealed_pods, Colors::RED),
                    );
                    self.nav_item(
                        ui,
                        mode,
                        icons::HELMET_SAFETY,
                        "Infrastructure",
                        View::Infra,
                        None,
                    );
                    self.nav_item(ui, mode, icons::CUBE, "Libraries", View::Libraries, None);
                    self.nav_item(
                        ui,
                        mode,
                        icons::CODE_BRANCH,
                        "Pull Requests",
                        View::PullRequests,
                        badge(open_pr_count, Colors::RED),
                    );
                    let open_issue_count =
                        state.read(|s| s.issues.iter().filter(|i| i.is_open()).count());
                    self.nav_item(
                        ui,
                        mode,
                        icons::CLIPBOARD_LIST,
                        "Issues",
                        View::Issues,
                        badge(open_issue_count, Colors::RED),
                    );

                    // Bottom area
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        // Bottom-up: first item is at the very bottom
                        ui.horizontal(|ui| {
                            theme::theme_toggle(ui, &mut self.theme_mode);
                            ui.label(
                                egui::RichText::new(format!("v{}", crate::app_version()))
                                    .small()
                                    .color(Colors::text_muted(mode)),
                            );
                        });

                        ui.add_space(4.0);
                        self.nav_item(ui, mode, icons::GEAR, "Settings", View::Settings, None);

                        // Update banner (above Settings in the sidebar)
                        if let Some(ref update) = update_available {
                            ui.add_space(2.0);
                            // Row 1: version label + Update button
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("v{} available", update.version))
                                        .size(10.0)
                                        .strong()
                                        .color(Colors::ACCENT),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if let Some(ref status) = self.update_status {
                                            if status == "restart" {
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("Restart")
                                                                .size(9.5)
                                                                .color(Colors::bg_base(mode)),
                                                        )
                                                        .fill(Colors::GREEN)
                                                        .corner_radius(4.0),
                                                    )
                                                    .clicked()
                                                {
                                                    let exe_path: Option<String> =
                                                        ui.ctx().data_mut(|d| {
                                                            d.get_temp(egui::Id::new(
                                                                "update_exe_path",
                                                            ))
                                                        });
                                                    restart_app(exe_path);
                                                }
                                            } else if status.starts_with("error:") {
                                                // Show truncated error with hover for full text, plus a retry button
                                                let err_msg =
                                                    status.strip_prefix("error:").unwrap();
                                                let err_owned = err_msg.to_string();
                                                let max_len = 16;
                                                let display_msg = if err_owned.len() > max_len {
                                                    format!("{}…", &err_owned[..max_len])
                                                } else {
                                                    err_owned.clone()
                                                };
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            egui::RichText::new("Retry")
                                                                .size(9.0)
                                                                .color(Colors::bg_base(mode)),
                                                        )
                                                        .fill(Colors::ACCENT)
                                                        .corner_radius(4.0),
                                                    )
                                                    .clicked()
                                                {
                                                    self.update_status = None;
                                                }
                                                ui.label(
                                                    egui::RichText::new(&display_msg)
                                                        .size(9.0)
                                                        .color(Colors::RED),
                                                )
                                                .on_hover_text(&err_owned);
                                            } else {
                                                ui.label(
                                                    egui::RichText::new(status.as_str())
                                                        .size(9.0)
                                                        .color(Colors::text_muted(mode)),
                                                );
                                            }
                                        } else if crate::app_version() != "0.0.0"
                                            && update.download_url.is_some()
                                            && ui
                                                .add(
                                                    egui::Button::new(
                                                        egui::RichText::new("Update")
                                                            .size(9.5)
                                                            .color(Colors::bg_base(mode)),
                                                    )
                                                    .fill(Colors::ACCENT)
                                                    .corner_radius(4.0),
                                                )
                                                .clicked()
                                        {
                                            trigger_update(
                                                ui.ctx(),
                                                update,
                                                config_for_update.as_ref(),
                                            );
                                            self.update_status = Some("Downloading…".to_string());
                                        }
                                    },
                                );
                            });
                            // Row 2: release notes snippet + View release link
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                if let Some(ref notes) = update.release_notes {
                                    let first_line = notes
                                        .lines()
                                        .find(|l| {
                                            let t = l.trim();
                                            !t.is_empty()
                                                && !t.starts_with("**Full")
                                                && !t.starts_with("http")
                                        })
                                        .map(|l| l.trim().trim_start_matches('#').trim())
                                        .unwrap_or("");
                                    if !first_line.is_empty() {
                                        let max = 24;
                                        let truncated = if first_line.chars().count() > max {
                                            let head: String =
                                                first_line.chars().take(max).collect();
                                            format!("{}…", head)
                                        } else {
                                            first_line.to_string()
                                        };
                                        ui.label(
                                            egui::RichText::new(truncated)
                                                .size(9.0)
                                                .color(Colors::text_muted(mode)),
                                        )
                                        .on_hover_text(notes.as_str());
                                    }
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let html_url = update.html_url.clone();
                                        if ui
                                            .add(
                                                egui::Label::new(
                                                    egui::RichText::new("release notes")
                                                        .size(9.0)
                                                        .color(Colors::ACCENT),
                                                )
                                                .sense(egui::Sense::click()),
                                            )
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .clicked()
                                        {
                                            let _ = open::that(&html_url);
                                        }
                                    },
                                );
                            });
                        }
                    });
                });
            });
    }

    fn nav_item(
        &mut self,
        ui: &mut egui::Ui,
        mode: ThemeMode,
        icon_char: char,
        label: &str,
        view: View,
        badge: Option<(usize, egui::Color32)>,
    ) {
        let is_active = self.current_view == view;

        // Use previous frame's hover state via memory
        let item_id = ui.id().with(label);
        let is_hovered = ui
            .ctx()
            .data_mut(|d| d.get_temp::<bool>(item_id).unwrap_or(false));

        let bg = if is_active {
            Colors::ACCENT.linear_multiply(0.15)
        } else if is_hovered {
            Colors::bg_hover(mode)
        } else {
            egui::Color32::TRANSPARENT
        };
        let text_color = if is_active {
            Colors::ACCENT
        } else if is_hovered {
            Colors::text_primary(mode)
        } else {
            Colors::text_secondary(mode)
        };

        let response = egui::Frame::default()
            .fill(bg)
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    if is_active {
                        // Active indicator bar
                        let rect = ui.min_rect();
                        let bar = egui::Rect::from_min_size(
                            egui::pos2(rect.left() - 10.0, rect.top()),
                            egui::vec2(3.0, rect.height()),
                        );
                        ui.painter()
                            .rect_filled(bar, egui::CornerRadius::same(2), Colors::ACCENT);
                    }

                    ui.label(icons::icon(icon_char, 14.0, text_color));
                    ui.label(egui::RichText::new(label).color(text_color).strong());

                    if let Some((count, color)) = badge {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            theme::status_pill(ui, &count.to_string(), color);
                        });
                    }
                });
            })
            .response;

        let response = response.interact(egui::Sense::click() | egui::Sense::hover());

        // Store hover state for next frame
        ui.ctx()
            .data_mut(|d| d.insert_temp(item_id, response.hovered()));

        if response.clicked() {
            self.current_view = view;
        }
    }

    #[allow(dead_code)]
    fn nav_item_disabled(&self, ui: &mut egui::Ui, mode: ThemeMode, icon_char: char, label: &str) {
        egui::Frame::default()
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(icons::icon(icon_char, 14.0, Colors::text_muted(mode)));
                    ui.label(egui::RichText::new(label).color(Colors::text_muted(mode)));
                });
            });
    }
}

/// Trigger the async self-update download in the background.
/// Updates sidebar status via egui's data store.
fn trigger_update(
    ctx: &egui::Context,
    update: &crate::state::LatestVersion,
    config: Option<&crate::config::Config>,
) {
    let download_url = match update.download_url {
        Some(ref url) => url.clone(),
        None => return,
    };
    let checksum_url = update.checksum_url.clone();
    let asset_name = update.asset_name.clone().unwrap_or_default();
    let token = config
        .and_then(|c| c.resolve_github_token().ok().flatten())
        .unwrap_or_default();
    let ctx = ctx.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let result = rt.block_on(crate::pollers::self_update::download_and_replace(
            &download_url,
            checksum_url.as_deref(),
            &asset_name,
            &token,
        ));

        match result {
            Ok(installed_path) => {
                ctx.data_mut(|d| {
                    d.insert_temp(egui::Id::new("update_status"), "restart".to_string());
                    d.insert_temp(
                        egui::Id::new("update_exe_path"),
                        installed_path.to_string_lossy().to_string(),
                    );
                });
            }
            Err(e) => {
                let msg = format!("error:{}", e);
                tracing::error!("Self-update failed: {}", e);
                ctx.data_mut(|d| {
                    d.insert_temp(egui::Id::new("update_status"), msg);
                });
            }
        }
        ctx.request_repaint();
    });
}

/// Restart the application by re-executing the updated binary.
/// Uses the provided path (from self-update) if available, falling back to current_exe().
fn restart_app(exe_path: Option<String>) {
    let exe = if let Some(path) = exe_path {
        std::path::PathBuf::from(path)
    } else {
        match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("Failed to get current exe for restart: {}", e);
                return;
            }
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    tracing::info!("Restarting: {} {:?}", exe.display(), args);

    // On Unix, exec replaces the current process
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).args(&args).exec();
        tracing::error!("exec failed: {}", err);
    }

    // On non-Unix, spawn and exit
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new(&exe).args(&args).spawn();
        std::process::exit(0);
    }
}

/// Compact interval display: 120 -> "2m", 45 -> "45s".
fn format_interval(secs: u64) -> String {
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

/// Compact count display: 4900 -> "4.9k", 320 -> "320".
fn format_count(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}
