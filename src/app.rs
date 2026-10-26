use crate::cache::Cache;
use crate::config::Config;
use crate::state::AppState;
use crate::ui::filters::GlobalFilters;
use crate::ui::log_viewer::LogViewer;
use crate::ui::resources::ResourceViewer;
use crate::ui::settings::SettingsPage;
use crate::ui::setup::SetupWizard;
use crate::ui::sidebar::{Sidebar, View};
use crate::ui::theme::{self, ThemeMode};
use crate::ui::toast::Toaster;
use crate::ui::{
    apps, charts, ci, infra, issues, libraries, pull_requests, releases, service_detail, vault,
    vault_banner,
};
use eframe::egui;
use std::collections::HashMap;
use std::sync::Arc;

/// How long teardown may run after the window closes before the process exits anyway.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether we're showing the setup wizard or the main dashboard.
enum AppMode {
    Setup(Box<SetupWizard>),
    Dashboard,
}

pub struct CaravanApp {
    mode: AppMode,
    state: AppState,
    sidebar: Sidebar,
    filters: GlobalFilters,
    cache: Option<Arc<Cache>>,
    config: Option<Config>,
    settings: SettingsPage,
    toaster: Toaster,
    theme_applied: Option<ThemeMode>,
    first_frame: bool,
    shots: Option<crate::screenshots::ShotPlan>,
    pollers_started: bool,
    runtime: tokio::runtime::Runtime,
    /// Per-service log viewers (keyed by service name).
    log_viewers: HashMap<String, LogViewer>,
    /// Per-service resource viewers (keyed by service name).
    resource_viewers: HashMap<String, ResourceViewer>,
    /// Per-service active tab (keyed by service name).
    service_tabs: HashMap<String, service_detail::ServiceTab>,
    /// Track the previous view for stop-on-navigate-away.
    prev_view: Option<View>,
    /// Vault page state
    vault_page: vault::VaultPage,
    /// Owns the instance lock while this process is the active caravan.
    instance_guard: Option<crate::instance::InstanceGuard>,
    shutting_down: bool,
}

impl CaravanApp {
    pub fn new_with_runtime(
        cc: &eframe::CreationContext<'_>,
        state: AppState,
        cache: Option<Arc<Cache>>,
        config: Option<Config>,
        needs_setup: bool,
        runtime: tokio::runtime::Runtime,
        instance_guard: Option<crate::instance::InstanceGuard>,
    ) -> Self {
        crate::ui::icons::install_fonts(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let mut filters = GlobalFilters::default();
        if let Some(ref c) = cache {
            filters.load_from_cache(c);
        }

        let mode = if needs_setup {
            AppMode::Setup(Box::new(SetupWizard::new()))
        } else {
            AppMode::Dashboard
        };

        let pollers_started = !needs_setup; // pollers already started if no setup needed

        let mut sidebar = Sidebar::new();
        if !needs_setup {
            if let Some(ref c) = cache {
                if let Ok(Some(view_str)) = c.get_meta("last_view") {
                    if let Some(view) = View::from_str(&view_str) {
                        sidebar.current_view = view;
                    }
                }
            }
        }

        Self {
            mode,
            state,
            sidebar,
            filters,
            cache,
            config,
            settings: SettingsPage::default(),
            toaster: Toaster::default(),
            theme_applied: None,
            first_frame: true,
            shots: crate::screenshots::ShotPlan::from_env(),
            pollers_started,
            runtime,
            log_viewers: HashMap::new(),
            resource_viewers: HashMap::new(),
            service_tabs: HashMap::new(),
            prev_view: None,
            vault_page: vault::VaultPage::default(),
            instance_guard,
            shutting_down: false,
        }
    }

    /// Start pollers after setup completes (reloads config).
    fn start_pollers(&mut self) {
        if self.pollers_started {
            return;
        }
        self.pollers_started = true;

        // Reload config (setup wizard just wrote it)
        let config = match crate::config::load_config() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to load config after setup: {}", e);
                return;
            }
        };

        self.config = Some(config.clone());
        self.instance_guard = crate::pollers::start_background(
            &self.runtime,
            &config,
            &self.state,
            self.cache.clone(),
        );

        tracing::info!("Started pollers after setup");
    }

    /// Release the instance lock and bound teardown: dropping the runtime
    /// waits on every blocking task, so a stuck one would otherwise keep a
    /// windowless process alive.
    fn begin_shutdown(&mut self) {
        drop(self.instance_guard.take());
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        tracing::info!("Window closed, shutting down");
        std::thread::spawn(|| {
            std::thread::sleep(EXIT_GRACE);
            tracing::warn!("Shutdown still running after {:?}, exiting", EXIT_GRACE);
            std::process::exit(0);
        });
    }

    /// Toast when a Flux sync finishes, regardless of the current view.
    fn flux_sync_toasts(&mut self, ctx: &egui::Context) {
        use crate::state::FluxSyncState;
        let syncs = self.state.get_flux_syncs();
        let seen_id = egui::Id::new("flux_sync_seen");
        let mut seen: HashMap<String, u8> =
            ctx.data_mut(|d| d.get_temp(seen_id).unwrap_or_default());
        for (env, sync) in &syncs {
            let kind = match sync {
                FluxSyncState::Running { .. } => 0u8,
                FluxSyncState::Done { .. } => 1,
                FluxSyncState::Failed { .. } => 2,
            };
            let prev = seen.get(env).copied();
            if prev != Some(kind) {
                match sync {
                    FluxSyncState::Done { revision, .. } if prev == Some(0) => {
                        self.toaster
                            .push("", format!("Flux sync {} complete: {}", env, revision));
                    }
                    FluxSyncState::Failed { error, .. } if prev == Some(0) => {
                        self.toaster
                            .push("", format!("Flux sync {} failed: {}", env, error));
                    }
                    _ => {}
                }
                seen.insert(env.clone(), kind);
            }
        }
        ctx.data_mut(|d| d.insert_temp(seen_id, seen));
    }
}

impl eframe::App for CaravanApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) {
            self.begin_shutdown();
        }

        if self.first_frame {
            self.first_frame = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1600.0, 1000.0)));
        }

        if let Some(shots) = self.shots.as_mut() {
            if matches!(self.mode, AppMode::Dashboard) {
                shots.step(ctx, &mut self.sidebar);
            }
        }

        // Apply theme when mode changes
        let mode = self.sidebar.theme_mode;
        if self.theme_applied != Some(mode) {
            theme::apply_theme(ctx, mode);
            self.theme_applied = Some(mode);
        }

        // Check if we're in setup mode
        if matches!(self.mode, AppMode::Setup(_)) {
            let wizard_done = if let AppMode::Setup(ref mut wizard) = self.mode {
                wizard.show(ctx, &self.runtime, mode)
            } else {
                false
            };

            if wizard_done {
                self.mode = AppMode::Dashboard;
                self.start_pollers();
                if let Some(ref cache) = self.cache {
                    let _ = cache.set_meta("setup_complete", "true");
                }
            } else {
                return; // Don't show dashboard during setup
            }
        }

        // --- Dashboard mode ---
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        let open_pr_count = self
            .state
            .get_pull_requests()
            .iter()
            .filter(|pr| {
                pr.is_open()
                    && !pr.draft
                    && !pr.labels.iter().any(|l| {
                        let lower = l.to_lowercase();
                        lower == "do not merge" || lower == "work in progress"
                    })
            })
            .count()
            + self.state.get_open_gitops_prs().len();

        // Sidebar navigation
        self.sidebar
            .show(ctx, &self.state, &self.config, open_pr_count);

        // Top bar with back/forward + env filters + sync/promote actions
        let prev_time_period = self.filters.time_period;
        let prev_envs = self.filters.visible_envs.clone();
        self.sidebar.show_top_bar(
            ctx,
            &self.state,
            &mut self.filters,
            &self.config,
            &self.runtime,
            &mut self.toaster,
        );
        if self.filters.time_period != prev_time_period || self.filters.visible_envs != prev_envs {
            if let Some(ref cache) = self.cache {
                self.filters.save_to_cache(cache);
            }
        }

        // Stop background work when navigating away from AppDetail
        let current_view = self.sidebar.current_view.clone();
        self.state.set_active_view(&current_view.as_str());
        if let Some(ref prev) = self.prev_view {
            if *prev != current_view {
                // Navigated away from previous view
                if let View::AppDetail(ref prev_name) = prev {
                    if let Some(lv) = self.log_viewers.get_mut(prev_name) {
                        lv.stop_stream();
                    }
                    if let Some(rv) = self.resource_viewers.get_mut(prev_name) {
                        rv.stop();
                    }
                }
                // Persist current view to cache
                if let Some(ref cache) = self.cache {
                    let _ = cache.set_meta("last_view", &current_view.as_str());
                }
            }
        }
        self.prev_view = Some(current_view);

        // Main content
        let mut nav_request: Option<View> = None;
        let mut banner_nav: Option<View> = None;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(theme::Colors::bg_base(mode))
                    .inner_margin(egui::Margin {
                        left: 24,
                        right: 0,
                        top: 24,
                        bottom: 24,
                    }),
            )
            .show(ctx, |ui| {
                // Sealed-vault banner sits above every view
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 0,
                        right: 24,
                        top: 0,
                        bottom: 0,
                    })
                    .show(ui, |ui| {
                        banner_nav =
                            vault_banner::show(ui, &self.state, &self.config, mode, &self.runtime);
                    });

                // AppDetail manages its own scrolling (Logs tab needs fixed height,
                // not infinite scroll area). All other views use a shared outer scroll.
                match self.sidebar.current_view.clone() {
                    View::AppDetail(ref name) => {
                        let name = name.clone();
                        let log_viewer = self.log_viewers.entry(name.clone()).or_default();
                        let resource_viewer =
                            self.resource_viewers.entry(name.clone()).or_default();
                        let tab = self.service_tabs.entry(name.clone()).or_default();
                        // A view that linked here may have asked for a tab
                        let pending_id =
                            egui::Id::new((service_detail::PENDING_SERVICE_TAB, name.clone()));
                        if let Some(wanted) = ctx
                            .data_mut(|d| d.remove_temp::<service_detail::ServiceTab>(pending_id))
                        {
                            *tab = wanted;
                        }
                        egui::Frame::NONE
                            .inner_margin(egui::Margin {
                                left: 0,
                                right: 24,
                                top: 0,
                                bottom: 0,
                            })
                            .show(ui, |ui| {
                                nav_request = service_detail::show(
                                    ui,
                                    &self.state,
                                    &name,
                                    &self.filters,
                                    mode,
                                    log_viewer,
                                    resource_viewer,
                                    tab,
                                    &self.config,
                                    &self.runtime,
                                );
                            });
                    }
                    _ => {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            // Right padding (scrollbar sits at panel edge)
                            ui.spacing_mut().item_spacing.x = 0.0;
                            let inset = egui::Frame::NONE.inner_margin(egui::Margin {
                                left: 0,
                                right: 24,
                                top: 0,
                                bottom: 0,
                            });
                            inset.show(ui, |ui| match self.sidebar.current_view.clone() {
                                View::Apps => {
                                    nav_request = apps::show(ui, &self.state, &self.filters, mode);
                                }
                                View::Releases => {
                                    nav_request = releases::show(
                                        ui,
                                        &self.state,
                                        &self.filters,
                                        mode,
                                        &self.config,
                                        &self.cache,
                                        &self.runtime,
                                        &mut self.toaster,
                                    );
                                }
                                View::Ci => {
                                    nav_request = ci::show(ui, &self.state, &self.filters, mode);
                                }
                                View::Infra => {
                                    nav_request = infra::show(ui, &self.state, &self.filters, mode);
                                }
                                View::Libraries => {
                                    nav_request = libraries::show(
                                        ui,
                                        &self.state,
                                        &self.filters,
                                        mode,
                                        &self.cache,
                                        &mut self.toaster,
                                    );
                                }
                                View::PullRequests => {
                                    nav_request =
                                        pull_requests::show(ui, &self.state, &self.filters, mode);
                                }
                                View::Issues => {
                                    nav_request = issues::show(
                                        ui,
                                        &self.state,
                                        &self.filters,
                                        mode,
                                        &self.cache,
                                        &self.config,
                                        &self.runtime,
                                    );
                                }
                                View::Vault => {
                                    nav_request = vault::show(
                                        ui,
                                        &self.state,
                                        &self.filters,
                                        mode,
                                        &mut self.vault_page,
                                        &self.config,
                                        &self.runtime,
                                    );
                                }
                                View::Settings => {
                                    self.settings.show(ui, &self.runtime, &self.config, mode);
                                }
                                View::AppDetail(_) => unreachable!(),
                            });
                        });
                    }
                }
            });

        if let Some(view) = nav_request.or(banner_nav) {
            self.sidebar.navigate(view);
        }

        // Show pinned release tooltip popup (from heatmap clicks)
        charts::show_pinned_release_popup(ctx, mode);

        self.flux_sync_toasts(ctx);
        for line in self.state.take_vault_events() {
            self.toaster.push("", line);
        }
        self.toaster.show(ctx, mode);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.begin_shutdown();
    }
}
