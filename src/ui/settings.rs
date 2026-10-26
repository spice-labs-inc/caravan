use eframe::egui;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::auth::github::DeviceFlowState;
use crate::auth::kubeconfig::DiscoveredKubeconfig;
use crate::config::Config;
use crate::ui::theme::{self, Colors, ThemeMode};

#[derive(Debug, Clone)]
pub enum GitHubStatus {
    Unknown,
    Checking,
    Connected { username: String },
    NotConnected,
    Error(String),
}

/// A kubeconfig entry for the settings UI — mirrors setup wizard's KubeconfigAssignment.
#[derive(Debug, Clone)]
struct ClusterEntry {
    kubeconfig: DiscoveredKubeconfig,
    context: Option<String>,
    selected: bool,
    env_label: String,
    kube_type: String,
}

pub struct SettingsPage {
    // GitHub
    pub github_status: Arc<Mutex<GitHubStatus>>,
    pub device_flow_rx: Option<tokio::sync::watch::Receiver<DeviceFlowState>>,
    pub device_flow_state: DeviceFlowState,
    pub checked_token: bool,
    // Kubernetes
    clusters: Vec<ClusterEntry>,
    clusters_loaded: bool,
    clusters_dirty: bool,
    save_message: Option<(String, std::time::Instant)>,
    file_picker_rx: Option<std::sync::mpsc::Receiver<std::path::PathBuf>>,
    // Everything else config.toml holds
    fields: ConfigFields,
    fields_loaded: bool,
    fields_dirty: bool,
}

impl Default for SettingsPage {
    fn default() -> Self {
        Self {
            github_status: Arc::new(Mutex::new(GitHubStatus::Unknown)),
            device_flow_rx: None,
            device_flow_state: DeviceFlowState::Idle,
            checked_token: false,
            clusters: Vec::new(),
            clusters_loaded: false,
            clusters_dirty: false,
            save_message: None,
            file_picker_rx: None,
            fields: ConfigFields::default(),
            fields_loaded: false,
            fields_dirty: false,
        }
    }
}

impl SettingsPage {
    fn check_token_if_needed(&mut self, runtime: &tokio::runtime::Runtime) {
        if self.checked_token {
            return;
        }
        self.checked_token = true;

        let token = crate::auth::keyring::retrieve_token()
            .ok()
            .flatten()
            .or_else(|| {
                crate::config::resolve_secret(&Some("gh auth token".to_string()), &None, &None)
                    .ok()
                    .flatten()
            });

        match token {
            Some(token) => {
                *self.github_status.lock().unwrap() = GitHubStatus::Checking;
                let status = self.github_status.clone();
                runtime.spawn(async move {
                    match crate::auth::github::verify_token(&token).await {
                        Some(username) => {
                            *status.lock().unwrap() = GitHubStatus::Connected { username };
                        }
                        None => {
                            *status.lock().unwrap() =
                                GitHubStatus::Error("Token is invalid or expired".to_string());
                        }
                    }
                });
            }
            None => {
                *self.github_status.lock().unwrap() = GitHubStatus::NotConnected;
            }
        }
    }

    fn load_clusters_if_needed(&mut self, config: &Option<Config>) {
        if self.clusters_loaded {
            return;
        }
        self.clusters_loaded = true;
        self.clusters_loaded = true;

        // Start with auto-discovered kubeconfigs
        let discovered = crate::auth::kubeconfig::discover_kubeconfigs();
        let mut seen_keys = std::collections::HashSet::new();

        // First, add entries from the current config (marked as selected)
        if let Some(cfg) = config {
            for env_cfg in cfg.environments.with_clusters() {
                let env = &env_cfg.id;
                let path = Config::expand_path(&env_cfg.kubeconfig);
                let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let seen_key = format!(
                    "{}:{}",
                    canonical.display(),
                    env_cfg.context.as_deref().unwrap_or("")
                );
                seen_keys.insert(seen_key);

                let kc = if path.exists() {
                    crate::auth::kubeconfig::parse_kubeconfig_file(&path)
                } else {
                    DiscoveredKubeconfig {
                        path: path.clone(),
                        contexts: vec![],
                        clusters: vec![],
                        suggested_env: Some(env.clone()),
                        suggested_type: Some("apps".to_string()),
                        valid: false,
                        error: Some("File not found".to_string()),
                    }
                };

                self.clusters.push(ClusterEntry {
                    kubeconfig: kc,
                    context: env_cfg.context.clone(),
                    selected: true,
                    env_label: env.clone(),
                    kube_type: "apps".to_string(),
                });
            }

            for (env, vault_cfg) in &cfg.vault {
                if let Some(ref kc_path) = vault_cfg.kubeconfig {
                    let path = Config::expand_path(kc_path);
                    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    let seen_key = format!("{}:", canonical.display());
                    seen_keys.insert(seen_key);

                    let kc = if path.exists() {
                        crate::auth::kubeconfig::parse_kubeconfig_file(&path)
                    } else {
                        DiscoveredKubeconfig {
                            path: path.clone(),
                            contexts: vec![],
                            clusters: vec![],
                            suggested_env: Some(env.clone()),
                            suggested_type: Some("vault".to_string()),
                            valid: false,
                            error: Some("File not found".to_string()),
                        }
                    };

                    self.clusters.push(ClusterEntry {
                        kubeconfig: kc,
                        context: None,
                        selected: true,
                        env_label: env.clone(),
                        kube_type: "vault".to_string(),
                    });
                }
            }
        }

        // Add discovered kubeconfigs not already in config
        for kc in discovered {
            let canonical = std::fs::canonicalize(&kc.path).unwrap_or_else(|_| kc.path.clone());

            if kc.contexts.len() <= 1 {
                let ctx = kc.contexts.first().map(|s| s.as_str()).unwrap_or("");
                let seen_key = format!("{}:{}", canonical.display(), ctx);
                if seen_keys.contains(&seen_key) {
                    continue;
                }
                seen_keys.insert(seen_key);

                let env_label = kc.suggested_env.clone().unwrap_or_default();
                let kube_type = kc
                    .suggested_type
                    .clone()
                    .unwrap_or_else(|| "apps".to_string());
                let context = kc.contexts.first().cloned();
                self.clusters.push(ClusterEntry {
                    kubeconfig: kc,
                    context,
                    selected: false,
                    env_label,
                    kube_type,
                });
            } else {
                // Multi-context: expand into one row per context
                for ctx_name in &kc.contexts {
                    let seen_key = format!("{}:{}", canonical.display(), ctx_name);
                    if seen_keys.contains(&seen_key) {
                        continue;
                    }
                    seen_keys.insert(seen_key);

                    let (env, ktype) = crate::auth::kubeconfig::suggest_from_context(ctx_name);
                    self.clusters.push(ClusterEntry {
                        kubeconfig: kc.clone(),
                        context: Some(ctx_name.clone()),
                        selected: false,
                        env_label: env.unwrap_or_default(),
                        kube_type: ktype.unwrap_or_else(|| "apps".to_string()),
                    });
                }
            }
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        runtime: &tokio::runtime::Runtime,
        config: &Option<Config>,
        mode: ThemeMode,
    ) {
        self.check_token_if_needed(runtime);
        self.load_clusters_if_needed(config);
        self.load_fields_if_needed(config);

        // Poll device flow if active
        if let Some(ref rx) = self.device_flow_rx {
            self.device_flow_state = rx.borrow().clone();
        }

        ui.heading(
            egui::RichText::new("Settings")
                .size(24.0)
                .color(Colors::text_primary(mode)),
        );
        ui.add_space(24.0);

        // ── GitHub Section ──
        theme::section_header(ui, mode, "GitHub Authentication");
        ui.add_space(8.0);
        self.show_github_section(ui, runtime, mode);

        ui.add_space(24.0);

        // ── Kubernetes Section ──
        theme::section_header(ui, mode, "Kubernetes Clusters");
        ui.add_space(8.0);
        self.show_kubernetes_section(ui, mode);

        ui.add_space(24.0);

        theme::section_header(ui, mode, "GitOps");
        ui.add_space(8.0);
        self.show_gitops_section(ui, mode);

        ui.add_space(24.0);

        theme::section_header(ui, mode, "Vault Unseal");
        ui.add_space(8.0);
        self.show_vault_section(ui, mode);

        ui.add_space(24.0);

        theme::section_header(ui, mode, "Repositories");
        ui.add_space(8.0);
        self.show_repos_section(ui, mode);

        ui.add_space(24.0);

        theme::section_header(ui, mode, "Promote");
        ui.add_space(8.0);
        self.show_promote_section(ui, mode);

        ui.add_space(24.0);

        theme::section_header(ui, mode, "OpenObserve");
        ui.add_space(8.0);
        self.show_openobserve_section(ui, mode);

        ui.add_space(24.0);

        theme::section_header(ui, mode, "Notifications");
        ui.add_space(8.0);
        self.show_notifications_section(ui, mode);

        ui.add_space(24.0);

        self.show_save_row(ui, mode);

        ui.add_space(24.0);

        // ── Config File Section ──
        theme::section_header(ui, mode, "Configuration File");
        ui.add_space(8.0);
        self.show_config_section(ui, mode);
    }

    fn show_github_section(
        &mut self,
        ui: &mut egui::Ui,
        runtime: &tokio::runtime::Runtime,
        mode: ThemeMode,
    ) {
        let status = self.github_status.lock().unwrap().clone();

        theme::card(ui, mode, |ui| match &status {
            GitHubStatus::Unknown | GitHubStatus::Checking => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("Checking token...").color(Colors::text_muted(mode)),
                    );
                });
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(200));
            }
            GitHubStatus::Connected { username } => {
                ui.label(
                    egui::RichText::new(format!("Connected as @{}", username))
                        .size(15.0)
                        .color(egui::Color32::from_rgb(46, 160, 67)),
                );
            }
            GitHubStatus::NotConnected => {
                ui.label(
                    egui::RichText::new("Not connected to GitHub.")
                        .size(14.0)
                        .color(Colors::text_secondary(mode)),
                );
            }
            GitHubStatus::Error(err) => {
                ui.label(
                    egui::RichText::new(err)
                        .size(14.0)
                        .color(egui::Color32::from_rgb(255, 199, 0)),
                );
            }
        });

        ui.add_space(8.0);

        self.text_row(ui, mode, "GitHub organization", |f| &mut f.github_org);
        self.text_row(ui, mode, "OAuth App client id", |f| &mut f.oauth_client_id);
        ui.label(
            egui::RichText::new(
                "Each deployment registers its own GitHub OAuth App (device flow enabled) \
                 and pastes its client id here.",
            )
            .size(12.0)
            .color(Colors::text_muted(mode)),
        );
        ui.add_space(8.0);

        let show_connect = matches!(status, GitHubStatus::NotConnected | GitHubStatus::Error(_))
            && !matches!(
                self.device_flow_state,
                DeviceFlowState::WaitingForUser { .. } | DeviceFlowState::Polling { .. }
            );

        let show_reauth = matches!(status, GitHubStatus::Connected { .. })
            && !matches!(
                self.device_flow_state,
                DeviceFlowState::WaitingForUser { .. } | DeviceFlowState::Polling { .. }
            );

        ui.horizontal(|ui| {
            if show_connect
                && ui
                    .add(
                        egui::Button::new(egui::RichText::new("Connect with GitHub").size(14.0))
                            .min_size(egui::vec2(200.0, 36.0))
                            .fill(egui::Color32::from_rgb(36, 41, 47))
                            .corner_radius(6.0),
                    )
                    .clicked()
            {
                self.start_device_flow(runtime);
            }

            if show_reauth {
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Re-authenticate").size(13.0))
                            .corner_radius(6.0),
                    )
                    .clicked()
                {
                    self.start_device_flow(runtime);
                }

                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Disconnect")
                                .size(13.0)
                                .color(egui::Color32::from_rgb(218, 54, 51)),
                        )
                        .corner_radius(6.0),
                    )
                    .clicked()
                {
                    let _ = crate::auth::keyring::delete_token();
                    *self.github_status.lock().unwrap() = GitHubStatus::NotConnected;
                    self.device_flow_rx = None;
                    self.device_flow_state = DeviceFlowState::Idle;
                }
            }
        });

        self.show_device_flow_progress(ui, mode);
    }

    fn show_kubernetes_section(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        // Coverage checklist
        let ladder = crate::models::environments::current();
        let needed: Vec<(String, &str)> = ladder
            .iter()
            .rev()
            .flat_map(|e| [(e.id.clone(), "apps"), (e.id.clone(), "vault")])
            .collect();

        theme::card(ui, mode, |ui| {
            ui.label(
                egui::RichText::new("Cluster coverage:")
                    .size(13.0)
                    .strong()
                    .color(Colors::text_primary(mode)),
            );
            ui.add_space(4.0);

            for (env, ktype) in &needed {
                let have = self
                    .clusters
                    .iter()
                    .any(|c| c.selected && c.env_label == *env && c.kube_type == *ktype);

                ui.horizontal(|ui| {
                    let (icon, color) = if have {
                        ("[x]", egui::Color32::from_rgb(46, 160, 67))
                    } else {
                        ("[ ]", Colors::text_muted(mode))
                    };
                    ui.label(
                        egui::RichText::new(icon)
                            .size(13.0)
                            .monospace()
                            .color(color),
                    );

                    let env_color = theme::env_color(env, mode);
                    theme::status_pill(ui, env, env_color);

                    let type_color = type_pill_color(ktype);
                    theme::status_pill(ui, ktype, type_color);
                });
            }
        });

        ui.add_space(8.0);

        // Poll file picker result
        if let Some(ref rx) = self.file_picker_rx {
            if let Ok(path) = rx.try_recv() {
                let already_exists = self.clusters.iter().any(|a| {
                    std::fs::canonicalize(&a.kubeconfig.path).ok()
                        == std::fs::canonicalize(&path).ok()
                });
                if !already_exists {
                    let kc = crate::auth::kubeconfig::parse_kubeconfig_file(&path);
                    if kc.contexts.len() <= 1 {
                        let env_label = kc.suggested_env.clone().unwrap_or_default();
                        let kube_type = kc
                            .suggested_type
                            .clone()
                            .unwrap_or_else(|| "apps".to_string());
                        let context = kc.contexts.first().cloned();
                        self.clusters.push(ClusterEntry {
                            selected: true,
                            kubeconfig: kc,
                            context,
                            env_label,
                            kube_type,
                        });
                    } else {
                        for ctx_name in &kc.contexts {
                            let (env, ktype) =
                                crate::auth::kubeconfig::suggest_from_context(ctx_name);
                            self.clusters.push(ClusterEntry {
                                kubeconfig: kc.clone(),
                                context: Some(ctx_name.clone()),
                                selected: false,
                                env_label: env.unwrap_or_default(),
                                kube_type: ktype.unwrap_or_else(|| "apps".to_string()),
                            });
                        }
                    }
                    self.clusters_dirty = true;
                }
                self.file_picker_rx = None;
            }
        }

        // Add file button
        let picker_busy = self.file_picker_rx.is_some();
        if ui
            .add_enabled(
                !picker_busy,
                egui::Button::new(egui::RichText::new("+ Add kubeconfig file...").size(13.0))
                    .corner_radius(6.0),
            )
            .clicked()
        {
            let (tx, rx) = std::sync::mpsc::channel();
            self.file_picker_rx = Some(rx);
            std::thread::spawn(move || {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Select kubeconfig file")
                    .add_filter("YAML files", &["yaml", "yml"])
                    .add_filter("All files", &["*"])
                    .set_directory(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
                    .pick_file()
                {
                    let _ = tx.send(path);
                }
            });
        }

        ui.add_space(8.0);

        // Cluster list
        let ladder = crate::models::environments::current();
        let mut env_options: Vec<&str> = vec![""];
        env_options.extend(ladder.pipeline_order_ids());
        let type_options = ["apps", "vault", "infra"];
        let row_width = ui.available_width();
        let mut remove_idx = None;

        egui::ScrollArea::vertical()
            .id_salt("settings_clusters")
            .max_height(500.0)
            .show(ui, |ui| {
                for (idx, entry) in self.clusters.iter_mut().enumerate() {
                    ui.add_space(4.0);

                    let frame_fill = if entry.selected {
                        Colors::bg_elevated(mode)
                    } else {
                        Colors::bg_base(mode)
                    };

                    egui::Frame::default()
                        .fill(frame_fill)
                        .corner_radius(8.0)
                        .inner_margin(12.0)
                        .stroke(egui::Stroke::new(
                            1.0_f32,
                            if entry.kubeconfig.valid {
                                Colors::border(mode)
                            } else {
                                egui::Color32::from_rgb(218, 54, 51)
                            },
                        ))
                        .show(ui, |ui| {
                            ui.set_min_width(row_width - 26.0);

                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut entry.selected, "").changed() {
                                    self.clusters_dirty = true;
                                }

                                ui.vertical(|ui| {
                                    // Path
                                    let path_str = entry.kubeconfig.path.to_string_lossy();
                                    let home = std::env::var("HOME").unwrap_or_default();
                                    let display_path = if path_str.starts_with(&home) {
                                        format!("~{}", &path_str[home.len()..])
                                    } else {
                                        path_str.to_string()
                                    };

                                    ui.horizontal(|ui| {
                                        // Show context name prominently for multi-context files
                                        if let Some(ref ctx) = entry.context {
                                            if entry.kubeconfig.contexts.len() > 1 {
                                                ui.label(
                                                    egui::RichText::new(ctx)
                                                        .size(13.0)
                                                        .strong()
                                                        .color(Colors::text_primary(mode)),
                                                );
                                                ui.label(
                                                    egui::RichText::new(&display_path)
                                                        .size(11.0)
                                                        .monospace()
                                                        .color(Colors::text_muted(mode)),
                                                );
                                            } else {
                                                ui.label(
                                                    egui::RichText::new(&display_path)
                                                        .size(13.0)
                                                        .monospace()
                                                        .color(Colors::text_primary(mode)),
                                                );
                                            }
                                        } else {
                                            ui.label(
                                                egui::RichText::new(&display_path)
                                                    .size(13.0)
                                                    .monospace()
                                                    .color(Colors::text_primary(mode)),
                                            );
                                        }

                                        // Remove button
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new("x")
                                                        .size(12.0)
                                                        .color(Colors::text_muted(mode)),
                                                )
                                                .frame(false),
                                            )
                                            .on_hover_text("Remove")
                                            .clicked()
                                        {
                                            remove_idx = Some(idx);
                                        }
                                    });

                                    if !entry.kubeconfig.valid {
                                        if let Some(ref err) = entry.kubeconfig.error {
                                            ui.label(
                                                egui::RichText::new(err)
                                                    .size(12.0)
                                                    .color(egui::Color32::from_rgb(218, 54, 51)),
                                            );
                                        }
                                    } else {
                                        // Cluster name + pills
                                        ui.horizontal(|ui| {
                                            if !entry.kubeconfig.clusters.is_empty() {
                                                ui.label(
                                                    egui::RichText::new(
                                                        entry.kubeconfig.clusters.join(", "),
                                                    )
                                                    .size(12.0)
                                                    .color(Colors::text_muted(mode)),
                                                );
                                                ui.add_space(4.0);
                                            }

                                            if !entry.env_label.is_empty() {
                                                theme::status_pill(
                                                    ui,
                                                    &entry.env_label,
                                                    theme::env_color(&entry.env_label, mode),
                                                );
                                            }
                                            if !entry.kube_type.is_empty() {
                                                theme::status_pill(
                                                    ui,
                                                    &entry.kube_type,
                                                    type_pill_color(&entry.kube_type),
                                                );
                                            }
                                        });

                                        // Env/type dropdowns when selected
                                        if entry.selected {
                                            ui.add_space(4.0);
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    egui::RichText::new("Environment:")
                                                        .size(13.0)
                                                        .color(Colors::text_secondary(mode)),
                                                );
                                                let prev_env = entry.env_label.clone();
                                                egui::ComboBox::from_id_salt(format!(
                                                    "settings_env_{}",
                                                    display_path
                                                ))
                                                .selected_text(if entry.env_label.is_empty() {
                                                    "Select..."
                                                } else {
                                                    &entry.env_label
                                                })
                                                .show_ui(ui, |ui| {
                                                    for opt in &env_options {
                                                        let label = if opt.is_empty() {
                                                            "Select..."
                                                        } else {
                                                            opt
                                                        };
                                                        ui.selectable_value(
                                                            &mut entry.env_label,
                                                            opt.to_string(),
                                                            label,
                                                        );
                                                    }
                                                });
                                                if entry.env_label != prev_env {
                                                    self.clusters_dirty = true;
                                                }

                                                ui.add_space(16.0);

                                                ui.label(
                                                    egui::RichText::new("Type:")
                                                        .size(13.0)
                                                        .color(Colors::text_secondary(mode)),
                                                );
                                                let prev_type = entry.kube_type.clone();
                                                egui::ComboBox::from_id_salt(format!(
                                                    "settings_type_{}",
                                                    display_path
                                                ))
                                                .selected_text(&entry.kube_type)
                                                .show_ui(ui, |ui| {
                                                    for opt in &type_options {
                                                        ui.selectable_value(
                                                            &mut entry.kube_type,
                                                            opt.to_string(),
                                                            *opt,
                                                        );
                                                    }
                                                });
                                                if entry.kube_type != prev_type {
                                                    self.clusters_dirty = true;
                                                }
                                            });
                                        }
                                    }
                                });
                            });
                        });
                }
            });

        // Handle remove
        if let Some(idx) = remove_idx {
            self.clusters.remove(idx);
            self.clusters_dirty = true;
        }
    }
    fn load_fields_if_needed(&mut self, config: &Option<Config>) {
        if self.fields_loaded {
            return;
        }
        self.fields_loaded = true;
        self.fields = match config {
            Some(c) => ConfigFields::from_config(c),
            None => ConfigFields::fresh(),
        };
    }

    fn text_row(
        &mut self,
        ui: &mut egui::Ui,
        mode: ThemeMode,
        label: &str,
        get: fn(&mut ConfigFields) -> &mut String,
    ) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(label)
                    .size(13.0)
                    .color(Colors::text_secondary(mode)),
            );
            let value = get(&mut self.fields);
            if ui
                .add(egui::TextEdit::singleline(value).desired_width(420.0))
                .changed()
            {
                self.fields_dirty = true;
            }
        });
    }

    fn show_gitops_section(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        theme::card(ui, mode, |ui| {
            self.text_row(ui, mode, "Gitops remote", |f| &mut f.app_gitops_remote);
            ui.label(
                egui::RichText::new(
                    "Caravan keeps a local mirror of this repo and pushes promotions to it over ssh.",
                )
                .size(12.0)
                .color(Colors::text_muted(mode)),
            );
            ui.add_space(6.0);
            self.text_row(ui, mode, "Mirror fetch interval (seconds)", |f| {
                &mut f.gitops_interval_secs
            });
            ui.label(
                egui::RichText::new(
                    "How often the mirror fetches app-gitops. Uses git, not the GitHub API.",
                )
                .size(12.0)
                .color(Colors::text_muted(mode)),
            );
        });
    }

    fn show_repos_section(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        theme::card(ui, mode, |ui| {
            ui.label(
                egui::RichText::new("Hidden repos and apps (one per line)")
                    .size(13.0)
                    .color(Colors::text_secondary(mode)),
            );
            if ui
                .add(
                    egui::TextEdit::multiline(&mut self.fields.hide_text)
                        .desired_rows(3)
                        .desired_width(420.0),
                )
                .changed()
            {
                self.fields_dirty = true;
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Repo to service names (repo = service, one per line)")
                    .size(13.0)
                    .color(Colors::text_secondary(mode)),
            );
            if ui
                .add(
                    egui::TextEdit::multiline(&mut self.fields.service_names_text)
                        .desired_rows(2)
                        .desired_width(420.0),
                )
                .changed()
            {
                self.fields_dirty = true;
            }
            ui.label(
                egui::RichText::new("Only needed when a repo deploys under another name.")
                    .size(12.0)
                    .color(Colors::text_muted(mode)),
            );
        });
    }

    fn show_promote_section(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        theme::card(ui, mode, |ui| {
            if ui
                .checkbox(
                    &mut self.fields.sync_after_push,
                    "Sync the promotion target after a push (default on in the modal)",
                )
                .changed()
            {
                self.fields_dirty = true;
            }
        });
    }

    fn show_openobserve_section(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        theme::card(ui, mode, |ui| {
            self.text_row(ui, mode, "URL", |f| &mut f.openobserve_url);
            self.text_row(ui, mode, "Organization", |f| &mut f.openobserve_org);
            ui.label(
                egui::RichText::new("Used by the Open in OpenObserve buttons on the Logs tab.")
                    .size(12.0)
                    .color(Colors::text_muted(mode)),
            );
        });
    }

    fn show_vault_section(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        // Every selected vault cluster gets an entry, so its fields are editable
        // before the first save.
        for entry in self
            .clusters
            .iter()
            .filter(|c| c.selected && c.kube_type == "vault" && !c.env_label.is_empty())
        {
            self.fields
                .vault
                .entry(entry.env_label.clone())
                .or_default();
        }
        let envs: Vec<String> = self.fields.vault.keys().cloned().collect();
        if envs.is_empty() {
            theme::card(ui, mode, |ui| {
                ui.label(
                    egui::RichText::new("No vault clusters selected.")
                        .size(13.0)
                        .color(Colors::text_muted(mode)),
                );
            });
            return;
        }
        let mut dirty = false;
        let mut forget: Option<String> = None;
        for env in envs {
            let vf = self.fields.vault.get_mut(&env).expect("vault entry exists");
            theme::card(ui, mode, |ui| {
                ui.label(
                    egui::RichText::new(theme::env_label(&env))
                        .size(14.0)
                        .strong()
                        .color(Colors::text_primary(mode)),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Unseal keys command")
                            .size(13.0)
                            .color(Colors::text_secondary(mode)),
                    );
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut vf.unseal_keys_cmd)
                                .desired_width(420.0)
                                .hint_text("op read op://vault/prod/unseal-1"),
                        )
                        .changed()
                    {
                        dirty = true;
                    }
                });
                ui.label(
                    egui::RichText::new("Prints one unseal key per line. Tried first; then remembered keys; then manual entry.")
                        .size(12.0)
                        .color(Colors::text_muted(mode)),
                );
                ui.add_space(4.0);
                if ui
                    .checkbox(
                        &mut vf.remember_keys,
                        "Remember keys in the OS keyring after a manual unseal",
                    )
                    .changed()
                {
                    dirty = true;
                }
                if ui
                    .checkbox(
                        &mut vf.auto_unseal,
                        "Auto-unseal when a key source resolves",
                    )
                    .changed()
                {
                    dirty = true;
                }
                ui.label(
                    egui::RichText::new(
                        "Ships off for every environment. Turn it on per environment only.",
                    )
                    .size(12.0)
                    .color(Colors::text_muted(mode)),
                );
                ui.add_space(4.0);
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Forget remembered keys").size(12.0))
                            .corner_radius(6.0),
                    )
                    .clicked()
                {
                    forget = Some(env.clone());
                }
            });
            ui.add_space(6.0);
        }
        if dirty {
            self.fields_dirty = true;
        }
        if let Some(env) = forget {
            let msg = match crate::actions::unseal_keys::forget_keys(&env) {
                Ok(()) => format!("Forgot remembered {} keys", theme::env_label(&env)),
                Err(e) => format!("Error forgetting {} keys: {}", theme::env_label(&env), e),
            };
            self.save_message = Some((msg, std::time::Instant::now()));
        }
    }

    fn show_notifications_section(&self, ui: &mut egui::Ui, mode: ThemeMode) {
        theme::card(ui, mode, |ui| {
            ui.label(
                egui::RichText::new(
                    "Desktop notifications are configured in the [notifications] section of config.toml. \
                     They now cover vault seal events: a vault going sealed notifies when enabled and \
                     the environment's failure flag is on.",
                )
                .size(13.0)
                .color(Colors::text_secondary(mode)),
            );
        });
    }

    fn show_save_row(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        ui.horizontal(|ui| {
            let save_enabled = self.clusters_dirty || self.fields_dirty;
            if ui
                .add_enabled(
                    save_enabled,
                    egui::Button::new(egui::RichText::new("Save settings").size(14.0))
                        .min_size(egui::vec2(180.0, 36.0))
                        .fill(if save_enabled {
                            Colors::accent(mode)
                        } else {
                            Colors::bg_elevated(mode)
                        })
                        .corner_radius(6.0),
                )
                .clicked()
            {
                match self.save_config() {
                    Ok(()) => {
                        self.clusters_dirty = false;
                        self.fields_dirty = false;
                        self.save_message = Some((
                            "Saved! Restart Caravan to apply changes.".to_string(),
                            std::time::Instant::now(),
                        ));
                    }
                    Err(e) => {
                        self.save_message =
                            Some((format!("Error saving: {}", e), std::time::Instant::now()));
                    }
                }
            }

            if let Some((ref msg, when)) = self.save_message {
                if when.elapsed() < std::time::Duration::from_secs(5) {
                    ui.add_space(8.0);
                    let color = if msg.starts_with("Error") {
                        egui::Color32::from_rgb(218, 54, 51)
                    } else {
                        egui::Color32::from_rgb(46, 160, 67)
                    };
                    ui.label(egui::RichText::new(msg).size(13.0).color(color));
                }
            }
        });
    }

    fn show_config_section(&self, ui: &mut egui::Ui, mode: ThemeMode) {
        theme::card(ui, mode, |ui| {
            let config_path = crate::branding::project_dirs()
                .map(|d| d.config_dir().join("config.toml"))
                .unwrap_or_else(|| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                    std::path::PathBuf::from(home)
                        .join(".config")
                        .join(crate::branding::APP_NAME)
                        .join("config.toml")
                });

            ui.label(
                egui::RichText::new(format!("{}", config_path.display()))
                    .size(13.0)
                    .monospace()
                    .color(Colors::text_secondary(mode)),
            );

            if config_path.exists() {
                ui.label(
                    egui::RichText::new("Loaded")
                        .size(13.0)
                        .color(egui::Color32::from_rgb(46, 160, 67)),
                );
            } else {
                ui.label(
                    egui::RichText::new("Not found")
                        .size(13.0)
                        .color(egui::Color32::from_rgb(255, 199, 0)),
                );
            }
        });
    }

    /// Write config.toml from the cluster selections and the field editors,
    /// keeping the token lines of [github] and the [notifications] and
    /// [polling] sections as they are.
    fn save_config(&self) -> anyhow::Result<()> {
        let config_dir = crate::config::config_dir();
        let config_path = config_dir.join("config.toml");

        let existing = if config_path.exists() {
            std::fs::read_to_string(&config_path).unwrap_or_default()
        } else {
            String::new()
        };
        let github_section = extract_section(&existing, "[github]")
            .unwrap_or_else(|| "[github]\ntoken_cmd = \"gh auth token\"\n".to_string());

        let clusters: Vec<ClusterSpec> = self
            .clusters
            .iter()
            .filter(|c| c.selected && !c.env_label.is_empty())
            .map(|c| ClusterSpec {
                env_label: c.env_label.clone(),
                kube_type: c.kube_type.clone(),
                kubeconfig_path: abbreviate_home(&c.kubeconfig.path.to_string_lossy()),
                context: c.context.clone(),
            })
            .collect();

        let toml = render_config(&github_section, &existing, &clusters, &self.fields);

        if config_path.exists() {
            let backup = config_path.with_extension("toml.bak");
            std::fs::copy(&config_path, &backup)?;
        }
        std::fs::create_dir_all(&config_dir)?;
        std::fs::write(&config_path, &toml)?;
        tracing::info!("Saved config to {:?}", config_path);
        Ok(())
    }

    fn show_device_flow_progress(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        match &self.device_flow_state {
            DeviceFlowState::Idle => {}
            DeviceFlowState::WaitingForUser {
                user_code,
                verification_uri,
            }
            | DeviceFlowState::Polling {
                user_code,
                verification_uri,
            } => {
                ui.add_space(12.0);

                theme::card(ui, mode, |ui| {
                    ui.label(
                        egui::RichText::new("Enter this code on GitHub:")
                            .size(13.0)
                            .color(Colors::text_secondary(mode)),
                    );
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(user_code)
                                .size(28.0)
                                .monospace()
                                .strong()
                                .color(Colors::text_primary(mode)),
                        )
                        .selectable(true),
                    );
                    ui.add_space(4.0);
                    ui.hyperlink_to(
                        egui::RichText::new(verification_uri.as_str()).size(13.0),
                        verification_uri.as_str(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new("Waiting for authorization...")
                                .size(12.0)
                                .color(Colors::text_muted(mode)),
                        );
                    });
                });

                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(500));
            }
            DeviceFlowState::Success { username, .. } => {
                *self.github_status.lock().unwrap() = GitHubStatus::Connected {
                    username: username.clone(),
                };
                self.device_flow_rx = None;
                self.device_flow_state = DeviceFlowState::Idle;
            }
            DeviceFlowState::Error(err) => {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!("Error: {}", err))
                        .size(13.0)
                        .color(egui::Color32::from_rgb(218, 54, 51)),
                );
                self.device_flow_rx = None;
                self.device_flow_state = DeviceFlowState::Idle;
            }
        }
    }

    fn start_device_flow(&mut self, runtime: &tokio::runtime::Runtime) {
        let rx = crate::auth::github::start_device_flow(
            runtime,
            self.fields.oauth_client_id.trim().to_string(),
        );
        self.device_flow_rx = Some(rx);
        self.device_flow_state = DeviceFlowState::Idle;
    }
}

fn type_pill_color(ktype: &str) -> egui::Color32 {
    match ktype {
        "apps" => egui::Color32::from_rgb(33, 150, 243),
        "vault" => egui::Color32::from_rgb(156, 39, 176),
        "infra" => egui::Color32::from_rgb(121, 85, 72),
        _ => egui::Color32::from_rgb(158, 158, 158),
    }
}

pub fn abbreviate_home(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    }
}

/// Extract a TOML section from a string (e.g., "[github]" up to the next "[" section).
fn extract_section(toml: &str, header: &str) -> Option<String> {
    let start = toml.find(header)?;
    let rest = &toml[start + header.len()..];
    // Find the next section header
    let end = rest
        .find("\n[")
        .map(|i| i + 1) // include the newline before the next section
        .unwrap_or(rest.len());
    Some(format!("{}{}", header, &rest[..end]))
}

/// Editable copy of the config sections settings and setup can write.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigFields {
    /// `[github] org`
    pub github_org: String,
    /// `[github] oauth_client_id`: the deployment's own GitHub OAuth App
    pub oauth_client_id: String,
    pub app_gitops_remote: String,
    pub gitops_interval_secs: String,
    /// `[repos] hide`, one repo per line
    pub hide_text: String,
    /// `[repos] service_names`, one `repo = service` per line
    pub service_names_text: String,
    pub sync_after_push: bool,
    pub openobserve_url: String,
    pub openobserve_org: String,
    /// Per-env vault settings, keyed by env
    pub vault: BTreeMap<String, VaultFields>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VaultFields {
    pub url: String,
    pub kubeconfig: Option<String>,
    pub context: Option<String>,
    pub unseal_keys_cmd: String,
    pub remember_keys: bool,
    pub auto_unseal: bool,
}

impl ConfigFields {
    /// What a fresh config gets: the demo apps hidden, promote syncs, the
    /// shared-services OpenObserve.
    pub fn fresh() -> Self {
        let oo = crate::config::OpenObserveConfig::default();
        Self {
            github_org: String::new(),
            oauth_client_id: String::new(),
            app_gitops_remote: String::new(),
            gitops_interval_secs: "120".to_string(),
            hide_text: String::new(),
            service_names_text: String::new(),
            sync_after_push: true,
            openobserve_url: oo.url,
            openobserve_org: oo.org,
            vault: BTreeMap::new(),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        let mut service_names: Vec<String> = config
            .repos
            .service_names
            .iter()
            .map(|(repo, service)| format!("{repo} = {service}"))
            .collect();
        service_names.sort();
        let vault = config
            .vault
            .iter()
            .map(|(env, v)| {
                (
                    env.clone(),
                    VaultFields {
                        url: v.url.clone(),
                        kubeconfig: v.kubeconfig.clone(),
                        context: None,
                        unseal_keys_cmd: v.unseal_keys_cmd.clone().unwrap_or_default(),
                        remember_keys: v.remember_keys,
                        auto_unseal: v.auto_unseal,
                    },
                )
            })
            .collect();
        Self {
            github_org: config.github.org.clone(),
            oauth_client_id: config.github.oauth_client_id.clone().unwrap_or_default(),
            app_gitops_remote: config.github.app_gitops_remote.clone(),
            gitops_interval_secs: config.polling.gitops_interval_secs.to_string(),
            hide_text: config.repos.hide.join("\n"),
            service_names_text: service_names.join("\n"),
            sync_after_push: config.promote.sync_after_push,
            openobserve_url: config.openobserve.url.clone(),
            openobserve_org: config.openobserve.org.clone(),
            vault,
        }
    }

    fn hide_list(&self) -> Vec<String> {
        self.hide_text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn service_names(&self) -> Vec<(String, String)> {
        self.service_names_text
            .lines()
            .filter_map(|l| {
                let (repo, service) = l.split_once('=')?;
                let (repo, service) = (repo.trim(), service.trim());
                (!repo.is_empty() && !service.is_empty())
                    .then(|| (repo.to_string(), service.to_string()))
            })
            .collect()
    }
}

/// A cluster selection as written to config.toml.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSpec {
    pub env_label: String,
    /// "apps" or "vault"
    pub kube_type: String,
    pub kubeconfig_path: String,
    pub context: Option<String>,
}

/// Render config.toml. `github_section` is written first with its
/// `app_gitops_remote` set from the fields; the `[notifications]` and
/// `[polling]` sections of `existing` are kept, so are the `[promote]` keys
/// other than `sync_after_push`; the rest is regenerated.
pub fn render_config(
    github_section: &str,
    existing: &str,
    clusters: &[ClusterSpec],
    fields: &ConfigFields,
) -> String {
    let mut toml = remove_section_key(github_section, "app_gitops_remote");
    if !fields.github_org.trim().is_empty() {
        toml = set_section_key(&toml, "org", &toml_str(fields.github_org.trim()));
    }
    if !fields.oauth_client_id.trim().is_empty() {
        toml = set_section_key(
            &toml,
            "oauth_client_id",
            &toml_str(fields.oauth_client_id.trim()),
        );
    }
    if !fields.app_gitops_remote.trim().is_empty() {
        toml = set_section_key(
            &toml,
            "gitops_remote",
            &toml_str(fields.app_gitops_remote.trim()),
        );
    }
    toml.push('\n');

    // The ladder metadata (label, colour, upstream, reference) comes from the
    // ladder in force, else the public default, else a neutral entry.
    let ladder = crate::models::environments::current();
    let defaults = crate::models::environments::Environments::public_default();
    let mut apps: Vec<&ClusterSpec> = clusters.iter().filter(|c| c.kube_type == "apps").collect();
    apps.sort_by_key(|c| (ladder.rank(&c.env_label), defaults.rank(&c.env_label)));
    for entry in apps {
        let known = ladder
            .get(&entry.env_label)
            .or_else(|| defaults.get(&entry.env_label));
        toml.push_str("[[environments]]\n");
        toml.push_str(&format!("id = {}\n", toml_str(&entry.env_label)));
        toml.push_str(&format!(
            "kubeconfig = {}\n",
            toml_str(&entry.kubeconfig_path)
        ));
        if let Some(ref ctx) = entry.context {
            toml.push_str(&format!("context = {}\n", toml_str(ctx)));
        }
        let label = known
            .map(|e| e.label().to_string())
            .unwrap_or_else(|| entry.env_label.clone());
        toml.push_str(&format!("label = {}\n", toml_str(&label)));
        let color = known
            .and_then(|e| e.color.clone())
            .unwrap_or_else(|| "#2196F3".to_string());
        toml.push_str(&format!("color = {}\n", toml_str(&color)));
        if let Some(up) = known.and_then(|e| e.upstream.as_deref()) {
            toml.push_str(&format!("upstream = {}\n", toml_str(up)));
        }
        if known.is_some_and(|e| e.reference) {
            toml.push_str("reference = true\n");
        }
        toml.push('\n');
    }

    let mut vault_envs: std::collections::BTreeSet<String> = fields.vault.keys().cloned().collect();
    vault_envs.extend(
        clusters
            .iter()
            .filter(|c| c.kube_type == "vault")
            .map(|c| c.env_label.clone()),
    );
    for env in &vault_envs {
        let vf = fields.vault.get(env).cloned().unwrap_or_default();
        let cluster = clusters
            .iter()
            .find(|c| c.kube_type == "vault" && &c.env_label == env);
        let url = if vf.url.is_empty() {
            crate::config::default_vault_pattern().replace("{env}", env)
        } else {
            vf.url.clone()
        };
        toml.push_str(&format!("[vault.{env}]\n"));
        toml.push_str(&format!("url = {}\n", toml_str(&url)));
        let kubeconfig = cluster
            .map(|c| c.kubeconfig_path.clone())
            .or_else(|| vf.kubeconfig.clone());
        if let Some(kc) = kubeconfig {
            toml.push_str(&format!("kubeconfig = {}\n", toml_str(&kc)));
        }
        let context = cluster
            .and_then(|c| c.context.clone())
            .or_else(|| vf.context.clone());
        if let Some(ctx) = context {
            toml.push_str(&format!("context = {}\n", toml_str(&ctx)));
        }
        let cmd = vf.unseal_keys_cmd.trim();
        if cmd.is_empty() {
            toml.push_str("# unseal_keys_cmd = \"op read op://vault/prod/unseal-1\"\n");
        } else {
            toml.push_str(&format!("unseal_keys_cmd = {}\n", toml_str(cmd)));
        }
        toml.push_str(&format!("remember_keys = {}\n", vf.remember_keys));
        toml.push_str(&format!("auto_unseal = {}\n\n", vf.auto_unseal));
    }

    match extract_section(existing, "[notifications]") {
        Some(s) => {
            toml.push_str(s.trim_end());
            toml.push_str("\n\n");
        }
        None => toml.push_str("[notifications]\nenabled = true\nrelease_pr_reminders = true\ndeploy_completions = true\n\n"),
    }

    let polling = extract_section(existing, "[polling]").unwrap_or_else(|| {
        "[polling]\nk8s_interval_secs = 10\ngithub_interval_secs = 120\ngithub_issues_interval_secs = 300\ngithub_releases_interval_secs = 900\nvault_interval_secs = 30\nvault_sealed_interval_secs = 5\n".to_string()
    });
    let interval = fields
        .gitops_interval_secs
        .trim()
        .parse::<u64>()
        .unwrap_or(120)
        .max(30);
    toml.push_str(&set_section_key(
        &polling,
        "gitops_interval_secs",
        &interval.to_string(),
    ));
    toml.push('\n');

    toml.push_str("[repos]\n");
    let hide: Vec<String> = fields.hide_list().iter().map(|h| toml_str(h)).collect();
    toml.push_str(&format!("hide = [{}]\n", hide.join(", ")));
    let names = fields.service_names();
    if !names.is_empty() {
        toml.push_str("[repos.service_names]\n");
        for (repo, service) in names {
            toml.push_str(&format!("{} = {}\n", toml_str(&repo), toml_str(&service)));
        }
    }
    toml.push('\n');

    let promote =
        extract_section(existing, "[promote]").unwrap_or_else(|| "[promote]\n".to_string());
    toml.push_str(
        set_section_key(
            &promote,
            "sync_after_push",
            &fields.sync_after_push.to_string(),
        )
        .trim_end(),
    );
    toml.push_str("\n\n");
    toml.push_str(&format!(
        "[openobserve]\nurl = {}\norg = {}\n",
        toml_str(fields.openobserve_url.trim()),
        toml_str(fields.openobserve_org.trim())
    ));
    toml
}

/// Quote a TOML basic string.
pub fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Set `key = value` inside one section's text, replacing an existing line
/// or appending after the last non-blank line.
/// The section without any line setting `key`.
fn remove_section_key(section: &str, key: &str) -> String {
    let mut out: String = section
        .lines()
        .filter(|l| {
            !l.trim_start()
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
        .map(|l| format!("{l}\n"))
        .collect();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn set_section_key(section: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = section.lines().map(str::to_string).collect();
    let is_key = |l: &str| {
        l.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    let new_line = format!("{key} = {value}");
    if let Some(l) = lines.iter_mut().find(|l| is_key(l)) {
        *l = new_line;
    } else {
        let at = lines
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .map(|i| i + 1)
            .unwrap_or(lines.len());
        lines.insert(at, new_line);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_config_round_trips() {
        let existing = "[github]\ntoken_cmd = \"gh auth token\"\norg = \"notes-co\"\napp_gitops_remote = \"git@old\"\n\n[environments.prod]\nkubeconfig = \"x\"\n\n[notifications]\nenabled = false\nprod_failures = true\n\n[polling]\nk8s_interval_secs = 15\ngitops_interval_secs = 120\n";
        let clusters = vec![
            ClusterSpec {
                env_label: "prod".to_string(),
                kube_type: "apps".to_string(),
                kubeconfig_path: "~/kubeconfig-prod.yaml".to_string(),
                context: Some("prod-apps".to_string()),
            },
            ClusterSpec {
                env_label: "prod".to_string(),
                kube_type: "vault".to_string(),
                kubeconfig_path: "~/kubeconfig-prod-vault.yaml".to_string(),
                context: None,
            },
        ];
        let mut fields = ConfigFields::fresh();
        fields.app_gitops_remote = "git@github.com:me/app-gitops.git".to_string();
        fields.gitops_interval_secs = "300".to_string();
        fields.service_names_text = "pqc-rogues-gallery = rogues\n".to_string();
        fields.sync_after_push = false;
        fields.openobserve_org = "ops".to_string();
        fields.vault.insert(
            "prod".to_string(),
            VaultFields {
                url: String::new(),
                kubeconfig: None,
                context: None,
                unseal_keys_cmd: "op read \"op://vault/prod/key\"".to_string(),
                remember_keys: true,
                auto_unseal: false,
            },
        );
        fields.vault.insert(
            "staging".to_string(),
            VaultFields {
                url: "https://vault.staging.example:8200".to_string(),
                kubeconfig: Some("~/kubeconfig-staging-vault.yaml".to_string()),
                context: None,
                unseal_keys_cmd: String::new(),
                remember_keys: false,
                auto_unseal: true,
            },
        );

        let github = extract_section(existing, "[github]").unwrap();
        let rendered = render_config(&github, existing, &clusters, &fields);
        let config: Config = toml::from_str(&rendered).expect(&rendered);

        assert_eq!(config.github.token_cmd.as_deref(), Some("gh auth token"));
        assert_eq!(
            config.github.app_gitops_remote,
            "git@github.com:me/app-gitops.git"
        );
        assert_eq!(
            config.environments.get("prod").unwrap().context.as_deref(),
            Some("prod-apps")
        );
        assert!(!config.notifications.enabled);
        assert_eq!(config.polling.k8s_interval_secs, 15);
        assert_eq!(config.polling.gitops_interval_secs, 300);
        assert!(config.repos.hide.is_empty());
        assert_eq!(
            config
                .repos
                .service_names
                .get("pqc-rogues-gallery")
                .map(String::as_str),
            Some("rogues")
        );
        assert!(!config.promote.sync_after_push);
        assert_eq!(config.openobserve.org, "ops");
        let prod = &config.vault["prod"];
        assert_eq!(prod.url, "https://vault.prod.example.com:8200");
        assert_eq!(
            prod.kubeconfig.as_deref(),
            Some("~/kubeconfig-prod-vault.yaml")
        );
        assert_eq!(
            prod.unseal_keys_cmd.as_deref(),
            Some("op read \"op://vault/prod/key\"")
        );
        assert!(prod.remember_keys);
        assert!(!prod.auto_unseal);
        let staging = &config.vault["staging"];
        assert_eq!(staging.url, "https://vault.staging.example:8200");
        assert_eq!(staging.unseal_keys_cmd, None);
        assert!(staging.auto_unseal);

        // Loading the rendered config back gives the same fields.
        let reloaded = ConfigFields::from_config(&config);
        assert_eq!(reloaded.app_gitops_remote, fields.app_gitops_remote);
        assert_eq!(reloaded.hide_text, fields.hide_text);
        assert_eq!(reloaded.service_names_text, "pqc-rogues-gallery = rogues");
        assert_eq!(
            reloaded.vault["prod"].unseal_keys_cmd,
            fields.vault["prod"].unseal_keys_cmd
        );
    }

    #[test]
    fn fresh_config_hides_nothing_and_has_no_vault_by_default() {
        let fields = ConfigFields::fresh();
        let clusters = vec![ClusterSpec {
            env_label: "dev".to_string(),
            kube_type: "apps".to_string(),
            kubeconfig_path: "~/.kube/dev.yaml".to_string(),
            context: None,
        }];
        let rendered = render_config("[github]\norg = \"o\"\n", "", &clusters, &fields);
        let config: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(
            config.environments.get("dev").unwrap().label.as_deref(),
            Some("Development")
        );
        assert!(config.repos.hide.is_empty());
        assert!(config.vault.is_empty());
        assert!(config.promote.sync_after_push);
        assert_eq!(config.polling.gitops_interval_secs, 120);
        assert_eq!(config.github.app_gitops_remote, fields.app_gitops_remote);
    }

    #[test]
    fn rendered_config_keeps_promote_disabled() {
        let existing = "[github]\norg = \"o\"\n\n[promote]\nenabled = false\nsync_after_push = true\n\n[openobserve]\norg = \"default\"\n";
        let mut fields = ConfigFields::fresh();
        fields.sync_after_push = false;
        let github = extract_section(existing, "[github]").unwrap();
        let rendered = render_config(&github, existing, &[], &fields);
        let config: Config = toml::from_str(&rendered).expect(&rendered);
        assert!(!config.promote.enabled);
        assert!(!config.promote.sync_after_push);
        assert_eq!(config.promote.pair(&config.environments), None);
    }

    #[test]
    fn section_key_replace_and_append() {
        let s = "[github]\norg = \"o\"\nfoo=\"bar\"\n\n";
        let out = set_section_key(s, "foo", "\"baz\"");
        assert!(out.contains("foo = \"baz\""));
        assert!(!out.contains("bar"));
        let out = set_section_key(s, "app_gitops_remote", "\"x\"");
        assert_eq!(
            out,
            "[github]\norg = \"o\"\nfoo=\"bar\"\napp_gitops_remote = \"x\"\n\n"
        );
    }
}
