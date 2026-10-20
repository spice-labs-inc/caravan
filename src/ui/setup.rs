use eframe::egui;

use crate::auth::github::DeviceFlowState;
use crate::auth::kubeconfig::DiscoveredKubeconfig;
use crate::ui::settings::{
    abbreviate_home, render_config, toml_str as toml_str_for_setup, ClusterSpec, ConfigFields,
};
use crate::ui::theme::{self, ThemeMode};

/// Which step of the setup wizard the user is on.
#[derive(Debug, Clone, PartialEq)]
pub enum SetupStep {
    Welcome,
    GitHub,
    Kubernetes,
    Done,
}

/// Assignment for a selected kubeconfig.
#[derive(Debug, Clone)]
pub struct KubeconfigAssignment {
    pub kubeconfig: DiscoveredKubeconfig,
    pub context: Option<String>,
    pub selected: bool,
    pub env_label: String,
    pub kube_type: String,
}

/// State of the setup wizard.
pub struct SetupWizard {
    pub step: SetupStep,
    pub device_flow_rx: Option<tokio::sync::watch::Receiver<DeviceFlowState>>,
    pub device_flow_state: DeviceFlowState,
    pub kubeconfigs: Vec<KubeconfigAssignment>,
    pub discovered: bool,
    pub skip_github: bool,
    pub github_username: Option<String>,
    file_picker_rx: Option<std::sync::mpsc::Receiver<std::path::PathBuf>>,
    /// The rest of config.toml; starts from the fresh-config defaults
    pub fields: Box<ConfigFields>,
}

impl SetupWizard {
    pub fn new() -> Self {
        Self {
            step: SetupStep::Welcome,
            device_flow_rx: None,
            device_flow_state: DeviceFlowState::Idle,
            kubeconfigs: Vec::new(),
            discovered: false,
            skip_github: false,
            github_username: None,
            file_picker_rx: None,
            fields: Box::new(ConfigFields::fresh()),
        }
    }

    /// Returns true when the wizard is complete and the main app should start.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        runtime: &tokio::runtime::Runtime,
        mode: ThemeMode,
    ) -> bool {
        // Poll device flow state if active
        if let Some(ref rx) = self.device_flow_rx {
            self.device_flow_state = rx.borrow().clone();
        }

        let mut done = false;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(theme::Colors::bg_base(mode))
                    .inner_margin(48.0),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    // Constrain width for readability
                    let max_width = 600.0;
                    ui.set_max_width(max_width);

                    ui.add_space(40.0);

                    match self.step.clone() {
                        SetupStep::Welcome => self.show_welcome(ui, mode),
                        SetupStep::GitHub => self.show_github(ui, runtime, mode),
                        SetupStep::Kubernetes => self.show_kubernetes(ui, mode),
                        SetupStep::Done => {
                            done = self.show_done(ui, mode);
                        }
                    }
                });
            });

        done
    }

    fn show_welcome(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        ui.heading(
            egui::RichText::new(format!("Welcome to {}", crate::branding::DISPLAY_NAME))
                .size(28.0)
                .color(theme::Colors::text_primary(mode)),
        );
        ui.add_space(16.0);

        ui.label(
            egui::RichText::new(
                "A GitOps dashboard for monitoring releases and deployments \
                 across your Kubernetes environments.",
            )
            .size(16.0)
            .color(theme::Colors::text_secondary(mode)),
        );

        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("Let's get you set up in just a couple of steps.")
                .size(15.0)
                .color(theme::Colors::text_secondary(mode)),
        );

        ui.add_space(32.0);

        // Step indicators
        ui.horizontal(|ui| {
            step_pill(ui, "1. GitHub", true, mode);
            ui.add_space(8.0);
            step_pill(ui, "2. Kubernetes", false, mode);
        });

        ui.add_space(32.0);

        if ui
            .add(
                egui::Button::new(egui::RichText::new("Get Started ->").size(16.0))
                    .min_size(egui::vec2(200.0, 44.0))
                    .fill(theme::Colors::accent(mode))
                    .corner_radius(8.0),
            )
            .clicked()
        {
            self.step = SetupStep::GitHub;
        }
    }

    fn show_github(
        &mut self,
        ui: &mut egui::Ui,
        runtime: &tokio::runtime::Runtime,
        mode: ThemeMode,
    ) {
        ui.heading(
            egui::RichText::new("Connect to GitHub")
                .size(24.0)
                .color(theme::Colors::text_primary(mode)),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Caravan needs read access to your GitHub organization to show \
                 releases, PRs, and CI status.",
            )
            .size(14.0)
            .color(theme::Colors::text_secondary(mode)),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Caravan's access is limited to what your kubeconfig allows.")
                .size(13.0)
                .color(theme::Colors::text_muted(mode)),
        );

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("GitHub organization")
                    .size(13.0)
                    .color(theme::Colors::text_secondary(mode)),
            );
            ui.add(egui::TextEdit::singleline(&mut self.fields.github_org).desired_width(240.0));
        });
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("OAuth App client id")
                    .size(13.0)
                    .color(theme::Colors::text_secondary(mode)),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.fields.oauth_client_id).desired_width(240.0),
            );
        });
        ui.label(
            egui::RichText::new(
                "Each deployment registers its own GitHub OAuth App (device flow enabled) \
                 and pastes its client id here.",
            )
            .size(12.0)
            .color(theme::Colors::text_muted(mode)),
        );

        ui.add_space(16.0);

        match &self.device_flow_state {
            DeviceFlowState::Idle => {
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Connect with GitHub").size(15.0))
                            .min_size(egui::vec2(220.0, 40.0))
                            .fill(egui::Color32::from_rgb(36, 41, 47))
                            .corner_radius(8.0),
                    )
                    .clicked()
                {
                    let rx = crate::auth::github::start_device_flow(
                        runtime,
                        self.fields.oauth_client_id.trim().to_string(),
                    );
                    self.device_flow_rx = Some(rx);
                }
            }

            DeviceFlowState::WaitingForUser {
                user_code,
                verification_uri,
            }
            | DeviceFlowState::Polling {
                user_code,
                verification_uri,
            } => {
                ui.label(
                    egui::RichText::new("Enter this code on GitHub:")
                        .size(14.0)
                        .color(theme::Colors::text_secondary(mode)),
                );
                ui.add_space(12.0);

                // Big user code display
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(user_code)
                            .size(32.0)
                            .monospace()
                            .strong()
                            .color(theme::Colors::text_primary(mode)),
                    )
                    .selectable(true),
                );

                ui.add_space(12.0);

                ui.hyperlink_to(
                    egui::RichText::new(format!("Open {}", verification_uri)).size(14.0),
                    verification_uri,
                );

                ui.add_space(16.0);
                ui.spinner();
                ui.label(
                    egui::RichText::new("Waiting for authorization...")
                        .size(13.0)
                        .color(theme::Colors::text_muted(mode)),
                );

                // Request repaints while polling
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(500));
            }

            DeviceFlowState::Success { username, .. } => {
                self.github_username = Some(username.clone());

                ui.label(
                    egui::RichText::new(format!("Connected as @{}", username))
                        .size(18.0)
                        .color(egui::Color32::from_rgb(46, 160, 67)),
                );

                ui.add_space(24.0);

                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Continue ->").size(15.0))
                            .min_size(egui::vec2(160.0, 40.0))
                            .fill(theme::Colors::accent(mode))
                            .corner_radius(8.0),
                    )
                    .clicked()
                {
                    self.step = SetupStep::Kubernetes;
                }
            }

            DeviceFlowState::Error(err) => {
                ui.label(
                    egui::RichText::new(format!("{} {}", super::icons::CIRCLE_XMARK, err))
                        .size(14.0)
                        .color(egui::Color32::from_rgb(218, 54, 51)),
                );

                ui.add_space(16.0);

                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Try Again").size(14.0))
                            .min_size(egui::vec2(120.0, 36.0))
                            .corner_radius(8.0),
                    )
                    .clicked()
                {
                    self.device_flow_rx = None;
                    self.device_flow_state = DeviceFlowState::Idle;
                }
            }
        }

        ui.add_space(24.0);
        ui.separator();
        ui.add_space(8.0);

        // Skip option
        if !matches!(self.device_flow_state, DeviceFlowState::Success { .. })
            && ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Skip, I'll configure manually")
                            .size(13.0)
                            .color(theme::Colors::text_muted(mode)),
                    )
                    .frame(false),
                )
                .clicked()
        {
            self.skip_github = true;
            self.step = SetupStep::Kubernetes;
        }
    }

    fn show_kubernetes(&mut self, ui: &mut egui::Ui, mode: ThemeMode) {
        // Discover kubeconfigs on first visit
        if !self.discovered {
            self.discovered = true;
            let discovered = crate::auth::kubeconfig::discover_kubeconfigs();
            let mut assignments = Vec::new();
            for kc in discovered {
                if kc.contexts.len() <= 1 {
                    // Single-context file: one row
                    let env_label = kc.suggested_env.clone().unwrap_or_default();
                    let kube_type = kc
                        .suggested_type
                        .clone()
                        .unwrap_or_else(|| "apps".to_string());
                    let context = kc.contexts.first().cloned();
                    assignments.push(KubeconfigAssignment {
                        kubeconfig: kc,
                        context,
                        selected: false,
                        env_label,
                        kube_type,
                    });
                } else {
                    // Multi-context file: expand into one row per context
                    for ctx_name in &kc.contexts {
                        let (env, ktype) = crate::auth::kubeconfig::suggest_from_context(ctx_name);
                        assignments.push(KubeconfigAssignment {
                            kubeconfig: kc.clone(),
                            context: Some(ctx_name.clone()),
                            selected: false,
                            env_label: env.unwrap_or_default(),
                            kube_type: ktype.unwrap_or_else(|| "apps".to_string()),
                        });
                    }
                }
            }
            self.kubeconfigs = assignments;
        }

        ui.heading(
            egui::RichText::new("Kubernetes Clusters")
                .size(24.0)
                .color(theme::Colors::text_primary(mode)),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Caravan works best with access to both apps and vault clusters \
                 for each environment in your ladder. Select the kubeconfig files below \
                 or add them manually.",
            )
            .size(14.0)
            .color(theme::Colors::text_secondary(mode)),
        );

        ui.add_space(12.0);

        // Progress checklist
        {
            let ladder = crate::models::environments::current();
            let needed: Vec<(String, &str)> = ladder
                .iter()
                .rev()
                .flat_map(|e| [(e.id.clone(), "apps"), (e.id.clone(), "vault")])
                .collect();

            egui::Frame::default()
                .fill(theme::Colors::bg_elevated(mode))
                .corner_radius(8.0)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Cluster coverage:")
                            .size(13.0)
                            .strong()
                            .color(theme::Colors::text_primary(mode)),
                    );
                    ui.add_space(4.0);

                    // Use a grid-like layout: 3 envs x 2 types
                    for (env, ktype) in &needed {
                        let have = self
                            .kubeconfigs
                            .iter()
                            .any(|a| a.selected && a.env_label == *env && a.kube_type == *ktype);

                        ui.horizontal(|ui| {
                            let (icon, color) = if have {
                                ("[x]", egui::Color32::from_rgb(46, 160, 67))
                            } else {
                                ("[ ]", theme::Colors::text_muted(mode))
                            };
                            ui.label(
                                egui::RichText::new(icon)
                                    .size(13.0)
                                    .monospace()
                                    .color(color),
                            );
                            ui.label(
                                egui::RichText::new(format!("{} {}", env, ktype))
                                    .size(13.0)
                                    .color(if have {
                                        theme::Colors::text_primary(mode)
                                    } else {
                                        theme::Colors::text_muted(mode)
                                    }),
                            );
                        });
                    }
                });
        }

        ui.add_space(12.0);

        // Poll file picker result
        if let Some(ref rx) = self.file_picker_rx {
            if let Ok(path) = rx.try_recv() {
                let already_exists = self.kubeconfigs.iter().any(|a| {
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
                        self.kubeconfigs.push(KubeconfigAssignment {
                            selected: kc.valid,
                            kubeconfig: kc,
                            context,
                            env_label,
                            kube_type,
                        });
                    } else {
                        for ctx_name in &kc.contexts {
                            let (env, ktype) =
                                crate::auth::kubeconfig::suggest_from_context(ctx_name);
                            self.kubeconfigs.push(KubeconfigAssignment {
                                kubeconfig: kc.clone(),
                                context: Some(ctx_name.clone()),
                                selected: false,
                                env_label: env.unwrap_or_default(),
                                kube_type: ktype.unwrap_or_else(|| "apps".to_string()),
                            });
                        }
                    }
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

        if self.kubeconfigs.is_empty() {
            ui.label(
                egui::RichText::new(
                    "No kubeconfig files found. Use the button above to add one, \
                     or configure them manually in ~/.config/caravan/config.toml",
                )
                .size(14.0)
                .color(theme::Colors::text_muted(mode)),
            );
        } else {
            let ladder = crate::models::environments::current();
            let mut env_options: Vec<&str> = vec![""];
            env_options.extend(ladder.pipeline_order_ids());
            let type_options = ["apps", "vault", "infra"];

            // Scrollable list of kubeconfigs

            let row_width = ui.available_width();
            egui::ScrollArea::vertical()
                .max_height(500.0)
                .show(ui, |ui| {
                    for assignment in &mut self.kubeconfigs {
                        ui.add_space(4.0);

                        let frame_fill = if assignment.selected {
                            theme::Colors::bg_elevated(mode)
                        } else {
                            theme::Colors::bg_base(mode)
                        };

                        egui::Frame::default()
                            .fill(frame_fill)
                            .corner_radius(8.0)
                            .inner_margin(12.0)
                            .stroke(egui::Stroke::new(
                                1.0_f32,
                                if assignment.kubeconfig.valid {
                                    theme::Colors::border(mode)
                                } else {
                                    egui::Color32::from_rgb(218, 54, 51)
                                },
                            ))
                            .show(ui, |ui| {
                                ui.set_min_width(row_width - 26.0); // account for frame margin

                                ui.horizontal(|ui| {
                                    ui.checkbox(&mut assignment.selected, "");

                                    ui.vertical(|ui| {
                                        // Path — abbreviate home dir
                                        let path_str = assignment.kubeconfig.path.to_string_lossy();
                                        let home = std::env::var("HOME").unwrap_or_default();
                                        let display_path = if path_str.starts_with(&home) {
                                            format!("~{}", &path_str[home.len()..])
                                        } else {
                                            path_str.to_string()
                                        };

                                        // Show context name prominently if from a multi-context file
                                        if let Some(ref ctx) = assignment.context {
                                            if assignment.kubeconfig.contexts.len() > 1 {
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        egui::RichText::new(ctx)
                                                            .size(14.0)
                                                            .strong()
                                                            .color(theme::Colors::text_primary(
                                                                mode,
                                                            )),
                                                    );
                                                    ui.label(
                                                        egui::RichText::new(&display_path)
                                                            .size(12.0)
                                                            .monospace()
                                                            .color(theme::Colors::text_muted(mode)),
                                                    );
                                                });
                                            } else {
                                                ui.label(
                                                    egui::RichText::new(&display_path)
                                                        .size(14.0)
                                                        .monospace()
                                                        .color(theme::Colors::text_primary(mode)),
                                                );
                                            }
                                        } else {
                                            ui.label(
                                                egui::RichText::new(&display_path)
                                                    .size(14.0)
                                                    .monospace()
                                                    .color(theme::Colors::text_primary(mode)),
                                            );
                                        }

                                        if !assignment.kubeconfig.valid {
                                            if let Some(ref err) = assignment.kubeconfig.error {
                                                ui.label(
                                                    egui::RichText::new(err).size(12.0).color(
                                                        egui::Color32::from_rgb(218, 54, 51),
                                                    ),
                                                );
                                            }
                                        } else {
                                            // Show cluster name + env/type pills
                                            ui.horizontal(|ui| {
                                                // Cluster name for clarity
                                                if !assignment.kubeconfig.clusters.is_empty() {
                                                    ui.label(
                                                        egui::RichText::new(
                                                            assignment
                                                                .kubeconfig
                                                                .clusters
                                                                .join(", "),
                                                        )
                                                        .size(12.0)
                                                        .color(theme::Colors::text_muted(mode)),
                                                    );
                                                    ui.add_space(4.0);
                                                }

                                                if !assignment.env_label.is_empty() {
                                                    let env_color = theme::env_color(
                                                        &assignment.env_label,
                                                        mode,
                                                    );
                                                    theme::status_pill(
                                                        ui,
                                                        &assignment.env_label,
                                                        env_color,
                                                    );
                                                }
                                                if !assignment.kube_type.is_empty() {
                                                    let type_color = match assignment
                                                        .kube_type
                                                        .as_str()
                                                    {
                                                        "apps" => {
                                                            egui::Color32::from_rgb(33, 150, 243)
                                                        }
                                                        "vault" => {
                                                            egui::Color32::from_rgb(156, 39, 176)
                                                        }
                                                        "infra" => {
                                                            egui::Color32::from_rgb(121, 85, 72)
                                                        }
                                                        _ => egui::Color32::from_rgb(158, 158, 158),
                                                    };
                                                    theme::status_pill(
                                                        ui,
                                                        &assignment.kube_type,
                                                        type_color,
                                                    );
                                                }
                                                if assignment.env_label.is_empty()
                                                    && assignment.kube_type.is_empty()
                                                {
                                                    ui.label(
                                                        egui::RichText::new("unrecognized")
                                                            .size(11.0)
                                                            .italics()
                                                            .color(theme::Colors::text_muted(mode)),
                                                    );
                                                }
                                            });

                                            if assignment.selected {
                                                // Editable env/type dropdowns
                                                ui.add_space(4.0);
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        egui::RichText::new("Environment:")
                                                            .size(13.0)
                                                            .color(theme::Colors::text_secondary(
                                                                mode,
                                                            )),
                                                    );
                                                    egui::ComboBox::from_id_salt(format!(
                                                        "env_{}",
                                                        display_path
                                                    ))
                                                    .selected_text(
                                                        if assignment.env_label.is_empty() {
                                                            "Select..."
                                                        } else {
                                                            &assignment.env_label
                                                        },
                                                    )
                                                    .show_ui(ui, |ui| {
                                                        for opt in &env_options {
                                                            let label = if opt.is_empty() {
                                                                "Select..."
                                                            } else {
                                                                opt
                                                            };
                                                            ui.selectable_value(
                                                                &mut assignment.env_label,
                                                                opt.to_string(),
                                                                label,
                                                            );
                                                        }
                                                    });

                                                    ui.add_space(16.0);

                                                    ui.label(
                                                        egui::RichText::new("Type:")
                                                            .size(13.0)
                                                            .color(theme::Colors::text_secondary(
                                                                mode,
                                                            )),
                                                    );
                                                    egui::ComboBox::from_id_salt(format!(
                                                        "type_{}",
                                                        display_path
                                                    ))
                                                    .selected_text(&assignment.kube_type)
                                                    .show_ui(ui, |ui| {
                                                        for opt in &type_options {
                                                            ui.selectable_value(
                                                                &mut assignment.kube_type,
                                                                opt.to_string(),
                                                                *opt,
                                                            );
                                                        }
                                                    });
                                                });
                                            }
                                        }
                                    });
                                });
                            });
                    }
                });
        }

        ui.add_space(12.0);

        egui::CollapsingHeader::new(
            egui::RichText::new("Advanced")
                .size(14.0)
                .color(theme::Colors::text_secondary(mode)),
        )
        .id_salt("setup_advanced")
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Gitops remote")
                        .size(13.0)
                        .color(theme::Colors::text_secondary(mode)),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.fields.app_gitops_remote)
                        .desired_width(360.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("OpenObserve URL")
                        .size(13.0)
                        .color(theme::Colors::text_secondary(mode)),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.fields.openobserve_url)
                        .desired_width(360.0),
                );
            });
            let vault_envs: Vec<String> = self
                .kubeconfigs
                .iter()
                .filter(|a| a.selected && a.kube_type == "vault" && !a.env_label.is_empty())
                .map(|a| a.env_label.clone())
                .collect();
            for env in vault_envs {
                let vf = self.fields.vault.entry(env.clone()).or_default();
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{} unseal keys command", env))
                            .size(13.0)
                            .color(theme::Colors::text_secondary(mode)),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut vf.unseal_keys_cmd)
                            .desired_width(300.0)
                            .hint_text("op read op://vault/prod/unseal-1"),
                    );
                });
                ui.checkbox(&mut vf.remember_keys, "Remember keys in the OS keyring");
            }
            ui.label(
                egui::RichText::new(
                    "Auto-unseal ships off; enable it per environment in Settings.",
                )
                .size(12.0)
                .color(theme::Colors::text_muted(mode)),
            );
        });

        ui.add_space(16.0);

        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("<- Back").size(14.0))
                        .min_size(egui::vec2(100.0, 36.0))
                        .corner_radius(8.0),
                )
                .clicked()
            {
                self.step = SetupStep::GitHub;
            }

            ui.add_space(16.0);

            if ui
                .add(
                    egui::Button::new(egui::RichText::new("Finish Setup ->").size(15.0))
                        .min_size(egui::vec2(160.0, 40.0))
                        .fill(theme::Colors::accent(mode))
                        .corner_radius(8.0),
                )
                .clicked()
            {
                // Generate config from selections
                if let Err(e) = self.generate_config() {
                    tracing::error!("Failed to generate config: {}", e);
                }
                self.step = SetupStep::Done;
            }
        });
    }

    fn show_done(&mut self, ui: &mut egui::Ui, mode: ThemeMode) -> bool {
        ui.heading(
            egui::RichText::new("All Set!")
                .size(28.0)
                .color(theme::Colors::text_primary(mode)),
        );
        ui.add_space(16.0);

        // Summary
        if let Some(ref username) = self.github_username {
            ui.label(
                egui::RichText::new(format!("GitHub: connected as @{}", username))
                    .size(15.0)
                    .color(egui::Color32::from_rgb(46, 160, 67)),
            );
        } else if self.skip_github {
            ui.label(
                egui::RichText::new("GitHub: skipped (configure in config.toml)")
                    .size(15.0)
                    .color(theme::Colors::text_muted(mode)),
            );
        }

        let selected_count = self
            .kubeconfigs
            .iter()
            .filter(|a| a.selected && !a.env_label.is_empty())
            .count();
        ui.label(
            egui::RichText::new(format!(
                "Kubernetes: {} cluster(s) configured",
                selected_count
            ))
            .size(15.0)
            .color(theme::Colors::text_secondary(mode)),
        );

        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("You can revisit this from Settings anytime.")
                .size(13.0)
                .color(theme::Colors::text_muted(mode)),
        );

        ui.add_space(32.0);

        let clicked = ui
            .add(
                egui::Button::new(egui::RichText::new("Open Dashboard ->").size(16.0))
                    .min_size(egui::vec2(200.0, 44.0))
                    .fill(theme::Colors::accent(mode))
                    .corner_radius(8.0),
            )
            .clicked();

        clicked
    }

    /// Generate config.toml from wizard selections.
    fn generate_config(&self) -> anyhow::Result<()> {
        let config_dir = crate::config::config_dir();

        std::fs::create_dir_all(&config_dir)?;
        let config_path = config_dir.join("config.toml");

        let mut toml = String::new();

        // GitHub section
        toml.push_str("[github]\n");
        if self.skip_github {
            toml.push_str("token_cmd = \"gh auth token\"\n");
        } else {
            // Token is stored in keyring, so no need to put it in config
            toml.push_str("# Token stored in system keyring (via OAuth)\n");
            toml.push_str("# Fallback: uncomment one of these\n");
            toml.push_str("# token_cmd = \"gh auth token\"\n");
            toml.push_str("# token_env = \"GH_TOKEN\"\n");
        }
        toml.push_str(&format!(
            "org = {}\n",
            toml_str_for_setup(self.fields.github_org.trim())
        ));
        if !self.fields.oauth_client_id.trim().is_empty() {
            toml.push_str(&format!(
                "oauth_client_id = {}\n",
                toml_str_for_setup(self.fields.oauth_client_id.trim())
            ));
        }
        let clusters: Vec<ClusterSpec> = self
            .kubeconfigs
            .iter()
            .filter(|a| a.selected && !a.env_label.is_empty())
            .map(|a| ClusterSpec {
                env_label: a.env_label.clone(),
                kube_type: a.kube_type.clone(),
                kubeconfig_path: abbreviate_home(&a.kubeconfig.path.to_string_lossy()),
                context: a.context.clone(),
            })
            .collect();
        let toml = render_config(&toml, "", &clusters, &self.fields);

        // Only write if config doesn't already exist (don't overwrite user edits)
        if config_path.exists() {
            let backup = config_path.with_extension("toml.bak");
            std::fs::copy(&config_path, &backup)?;
            tracing::info!("Backed up existing config to {:?}", backup);
        }

        std::fs::write(&config_path, &toml)?;
        tracing::info!("Generated config at {:?}", config_path);

        Ok(())
    }
}

fn step_pill(ui: &mut egui::Ui, text: &str, active: bool, mode: ThemeMode) {
    let (bg, fg) = if active {
        (theme::Colors::accent(mode), egui::Color32::WHITE)
    } else {
        (
            theme::Colors::bg_elevated(mode),
            theme::Colors::text_muted(mode),
        )
    };

    egui::Frame::default()
        .fill(bg)
        .corner_radius(12.0)
        .inner_margin(egui::vec2(12.0, 6.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(13.0).color(fg));
        });
}
