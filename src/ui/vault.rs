//! Vault dashboard page: per-environment cards with pod status and the
//! unseal workflow (key source button, manual key entry, run progress).

use crate::actions::unseal_keys::{self, KeySource, UnsealKeys};
use crate::actions::vault::{self as vault_actions, KeyInput, UnsealTrigger};
use crate::config::Config;
use crate::pollers::vault::{VaultCapability, VaultState};
use crate::state::AppState;
use crate::ui::filters::GlobalFilters;
use crate::ui::icons;
use crate::ui::theme::{status_pill, Colors, ThemeMode};
use crate::ui::vault_banner;
use eframe::egui;
use std::collections::HashMap;
use zeroize::Zeroizing;

/// Persistent state for the vault page, stored in the app.
#[derive(Default)]
pub struct VaultPage {
    /// Manual key entry per environment
    unseal_panels: HashMap<String, UnsealPanel>,
    /// Keys from a manual run kept until the run is dismissed so the user
    /// can still choose to remember them on this machine
    remember_offers: HashMap<String, UnsealKeys>,
}

impl VaultPage {
    /// Open the manual key entry for an env (the banner's manual path).
    pub fn open_manual(&mut self, env: &str) {
        self.unseal_panels
            .entry(env.to_string())
            .or_default()
            .focus_input = true;
    }
}

/// Environment-level manual entry. Keys entered once, applied to all sealed pods.
#[derive(Default)]
struct UnsealPanel {
    keys: UnsealKeys,
    key_input: Zeroizing<String>,
    focus_input: bool,
}

/// What one env card needs beyond the poller's `VaultState`.
struct EnvContext {
    cfg: Option<crate::config::VaultConfig>,
    run: Option<vault_actions::VaultUnsealRun>,
    remember_override: Option<bool>,
}

impl EnvContext {
    fn remember_enabled(&self) -> bool {
        self.cfg
            .as_ref()
            .is_some_and(|c| unseal_keys::remember_enabled(c, self.remember_override))
    }
}

/// Render the vault page. Returns a nav request if any.
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    filters: &GlobalFilters,
    mode: ThemeMode,
    vault_page: &mut VaultPage,
    config: &Option<Config>,
    runtime: &tokio::runtime::Runtime,
) -> Option<super::sidebar::View> {
    let mut nav_request = None;
    let open_id = egui::Id::new(vault_banner::OPEN_MANUAL_ID);
    if let Some(env) = ui.ctx().data_mut(|d| d.get_temp::<String>(open_id)) {
        ui.ctx().data_mut(|d| d.remove::<String>(open_id));
        vault_page.open_manual(&env);
    }

    let (vault_states, runs, overrides) = state.read(|s| {
        (
            s.vault_states.clone(),
            s.vault_ops.runs.clone(),
            s.vault_ops.remember_keys.clone(),
        )
    });
    let has_vault_config = config.as_ref().is_some_and(|c| !c.vault.is_empty());

    // Offers live only as long as their run
    vault_page.remember_offers.retain(|env, _| {
        runs.get(env)
            .is_some_and(|r| r.is_running() || r.succeeded())
    });

    if vault_states.is_empty() {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            if has_vault_config {
                let dots = ".".repeat((ui.input(|i| i.time) as usize % 3) + 1);
                ui.label(icons::icon(icons::LOCK, 24.0, Colors::text_muted(mode)));
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!("Connecting to Vault{}", dots))
                        .size(14.0)
                        .color(Colors::text_muted(mode)),
                );
                ui.ctx().request_repaint();
            } else {
                ui.label(icons::icon(icons::LOCK, 32.0, Colors::text_muted(mode)));
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Vault not configured")
                        .size(16.0)
                        .color(Colors::text_muted(mode)),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Add [vault.<env>] sections to your config to enable vault monitoring.",
                    )
                    .size(12.0)
                    .color(Colors::text_muted(mode)),
                );
            }
        });
        return None;
    }

    let ladder = crate::models::environments::current();
    let mut sorted_envs: Vec<&String> = vault_states.keys().collect();
    sorted_envs.sort_by_key(|name| ladder.rank(name));

    for env_name in sorted_envs {
        if !filters.visible_envs.contains(env_name) {
            continue;
        }
        let vault = &vault_states[env_name];
        let ctx = EnvContext {
            cfg: config.as_ref().and_then(|c| c.vault.get(env_name)).cloned(),
            run: runs.get(env_name).cloned(),
            remember_override: overrides.get(env_name).copied(),
        };
        if let Some(nav) = show_env_card(
            ui, env_name, vault, &ctx, mode, vault_page, state, config, runtime,
        ) {
            nav_request = Some(nav);
        }
        ui.add_space(16.0);
    }

    if runs.values().any(|r| r.is_running()) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }

    nav_request
}

#[allow(clippy::too_many_arguments)]
fn show_env_card(
    ui: &mut egui::Ui,
    env_name: &str,
    vault: &VaultState,
    ctx: &EnvContext,
    mode: ThemeMode,
    vault_page: &mut VaultPage,
    state: &AppState,
    config: &Option<Config>,
    runtime: &tokio::runtime::Runtime,
) -> Option<super::sidebar::View> {
    let mut nav_request = None;

    let sealed = vault.is_sealed();
    let (status_icon, status_color, status_text) = if vault.is_unreachable() {
        (icons::CIRCLE_XMARK, Colors::GRAY, "Unreachable")
    } else if sealed {
        (icons::LOCK, Colors::RED, "Sealed")
    } else {
        (icons::LOCK_OPEN, Colors::GREEN, "Unsealed")
    };

    egui::Frame::default()
        .fill(Colors::bg_elevated(mode))
        .corner_radius(8.0)
        .stroke(egui::Stroke::new(1.0_f32, Colors::border(mode)))
        .inner_margin(16.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());

            // Header row
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(env_name.to_uppercase())
                        .size(14.0)
                        .strong()
                        .color(Colors::text_primary(mode)),
                );
                ui.add_space(8.0);
                ui.label(icons::icon(status_icon, 14.0, status_color));
                ui.label(
                    egui::RichText::new(status_text)
                        .size(13.0)
                        .color(status_color),
                );
                if sealed {
                    if let Some(since) = vault.sealed_since {
                        ui.label(
                            egui::RichText::new(format!(
                                "since {}",
                                crate::ui::widgets::time_ago(&since)
                            ))
                            .size(11.0)
                            .color(Colors::text_muted(mode)),
                        );
                    }
                }
                ui.add_space(8.0);
                let auto = ctx.cfg.as_ref().is_some_and(|c| c.auto_unseal);
                status_pill(
                    ui,
                    if auto {
                        "auto-unseal on"
                    } else {
                        "auto-unseal off"
                    },
                    if auto { Colors::GREEN } else { Colors::GRAY },
                );
                status_pill(
                    ui,
                    &format!("keys: {}", vault.key_source.label()),
                    if vault.key_source.is_automatic() {
                        Colors::BLUE
                    } else {
                        Colors::GRAY
                    },
                );

                let url = vault.url.clone();
                if !url.is_empty() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = format!("{} Open UI", icons::ARROW_UP_RIGHT_FROM_SQUARE);
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(&label).size(11.0).color(Colors::ACCENT),
                                )
                                .fill(egui::Color32::TRANSPARENT)
                                .stroke(egui::Stroke::new(1.0_f32, Colors::ACCENT))
                                .corner_radius(4.0),
                            )
                            .clicked()
                        {
                            let _ = open::that(&url);
                        }
                    });
                }
            });

            // Sub-header
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                if let Some(version) = vault.version() {
                    ui.label(
                        egui::RichText::new(format!("v{}", version))
                            .size(11.0)
                            .color(Colors::text_muted(mode)),
                    );
                }
                if !vault.pods.is_empty() {
                    let unsealed = vault.pods.iter().filter(|p| p.sealed == Some(false)).count();
                    let total = vault.pods.len();
                    let color = if unsealed == total {
                        Colors::text_muted(mode)
                    } else {
                        Colors::ORANGE
                    };
                    ui.label(
                        egui::RichText::new(format!("{}/{} pods unsealed", unsealed, total))
                            .size(11.0)
                            .color(color),
                    );
                }
                if let Some(last) = vault.last_poll {
                    ui.label(
                        egui::RichText::new(format!(
                            "Last checked: {}",
                            crate::ui::widgets::time_ago(&last)
                        ))
                        .size(11.0)
                        .color(Colors::text_muted(mode)),
                    );
                }
            });

            // HTTP errors only matter without pod data: a sealed vault cannot
            // serve TLS to the ingress, so HTTP failing is expected then.
            if let Some(ref err) = vault.error {
                if vault.pods.is_empty() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(icons::icon(icons::TRIANGLE_EXCLAMATION, 12.0, Colors::ORANGE));
                        ui.label(egui::RichText::new(err).size(11.0).color(Colors::ORANGE));
                    });
                }
            }

            ui.add_space(8.0);

            match vault.capability {
                VaultCapability::Full => {
                    if let Some(ref pod_err) = vault.pod_error {
                        ui.horizontal(|ui| {
                            ui.label(icons::icon(
                                icons::TRIANGLE_EXCLAMATION,
                                12.0,
                                Colors::ORANGE,
                            ));
                            ui.label(
                                egui::RichText::new(format!("Pod discovery failed: {}", pod_err))
                                    .size(11.0)
                                    .color(Colors::ORANGE),
                            );
                        });
                    } else if vault.pods.is_empty() {
                        ui.label(
                            egui::RichText::new("No vault pods found")
                                .size(11.0)
                                .color(Colors::text_muted(mode)),
                        );
                    } else {
                        nav_request = show_pod_table(ui, env_name, vault, ctx, mode);
                    }
                    ui.add_space(12.0);
                    show_unseal_section(
                        ui, env_name, vault, ctx, mode, vault_page, state, config, runtime,
                    );
                }
                VaultCapability::HttpOnly => {
                    ui.horizontal(|ui| {
                        ui.label(icons::icon(
                            icons::CIRCLE_INFO,
                            12.0,
                            Colors::text_muted(mode),
                        ));
                        ui.label(
                            egui::RichText::new(
                                "Per-pod status and unseal unavailable: no vault kubeconfig configured.",
                            )
                            .size(11.0)
                            .color(Colors::text_muted(mode)),
                        );
                    });
                }
                VaultCapability::NotConfigured => {}
            }
        });

    nav_request
}

fn show_pod_table(
    ui: &mut egui::Ui,
    env_name: &str,
    vault: &VaultState,
    ctx: &EnvContext,
    mode: ThemeMode,
) -> Option<super::sidebar::View> {
    let mut nav_request = None;
    let table_id = format!("vault_pods_{}", env_name);

    egui::Grid::new(&table_id)
        .num_columns(5)
        .spacing([16.0, 6.0])
        .striped(true)
        .show(ui, |ui| {
            let h = Colors::text_muted(mode);
            ui.label(egui::RichText::new("POD").size(10.0).strong().color(h));
            ui.label(egui::RichText::new("STATUS").size(10.0).strong().color(h));
            ui.label(egui::RichText::new("RESTARTS").size(10.0).strong().color(h));
            ui.label(egui::RichText::new("AGE").size(10.0).strong().color(h));
            ui.label(egui::RichText::new("UNSEAL").size(10.0).strong().color(h));
            ui.end_row();

            for pod in &vault.pods {
                let pod_label = ui.add(
                    egui::Label::new(
                        egui::RichText::new(&pod.name)
                            .size(12.0)
                            .color(Colors::ACCENT)
                            .monospace(),
                    )
                    .sense(egui::Sense::click()),
                );
                if pod_label.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if pod_label.clicked() {
                    nav_request = Some(super::sidebar::View::AppDetail("vault".to_string()));
                }

                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    match pod.sealed {
                        Some(false) => {
                            ui.label(icons::icon(icons::LOCK_OPEN, 11.0, Colors::GREEN));
                            ui.label(
                                egui::RichText::new("Unsealed")
                                    .size(12.0)
                                    .color(Colors::GREEN),
                            );
                        }
                        Some(true) => {
                            ui.label(icons::icon(icons::LOCK, 11.0, Colors::RED));
                            let progress = pod.unseal_progress.unwrap_or(0);
                            let threshold = pod.unseal_threshold.unwrap_or(0);
                            let text = if threshold > 0 {
                                format!("Sealed ({}/{})", progress, threshold)
                            } else {
                                "Sealed".to_string()
                            };
                            ui.label(egui::RichText::new(text).size(12.0).color(Colors::RED));
                        }
                        None => {
                            ui.label(icons::icon(icons::CIRCLE_QUESTION, 11.0, Colors::GRAY));
                            ui.label(
                                egui::RichText::new("Unknown")
                                    .size(12.0)
                                    .color(Colors::text_muted(mode)),
                            );
                        }
                    }
                });

                let rc = if pod.restarts > 0 {
                    Colors::ORANGE
                } else {
                    Colors::text_muted(mode)
                };
                ui.label(
                    egui::RichText::new(pod.restarts.to_string())
                        .size(12.0)
                        .color(rc),
                );

                let age = pod
                    .created
                    .map(|c| format_age(chrono::Utc::now() - c))
                    .unwrap_or_else(|| "-".to_string());
                ui.label(
                    egui::RichText::new(&age)
                        .size(12.0)
                        .color(Colors::text_muted(mode)),
                );

                // Progress of the current or last unseal run for this pod
                let progress = ctx
                    .run
                    .as_ref()
                    .and_then(|r| r.pods.iter().find(|p| p.pod == pod.name));
                match progress {
                    Some(p) if p.error.is_some() => {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.label(icons::icon(icons::TRIANGLE_EXCLAMATION, 10.0, Colors::RED));
                            ui.label(
                                egui::RichText::new(p.error.as_deref().unwrap_or(""))
                                    .size(10.0)
                                    .color(Colors::RED),
                            );
                        });
                    }
                    Some(p) if p.is_unsealed() => {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.label(icons::icon(icons::CIRCLE_CHECK, 10.0, Colors::GREEN));
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}/{} keys",
                                    p.submitted, p.threshold
                                ))
                                .size(10.0)
                                .color(Colors::GREEN),
                            );
                        });
                    }
                    Some(p) => {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            ui.label(icons::icon(icons::KEY, 10.0, Colors::ACCENT));
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}/{} keys",
                                    p.submitted, p.threshold
                                ))
                                .size(10.0)
                                .color(Colors::ACCENT),
                            );
                        });
                    }
                    None => {
                        ui.label("");
                    }
                }

                ui.end_row();
            }
        });

    nav_request
}

/// Threshold for manual entry: the sealed pods' own report first, then the
/// HTTP seal status, then vault's default.
fn manual_threshold(vault: &VaultState) -> u32 {
    let from_pods = vault
        .pods
        .iter()
        .filter(|p| p.sealed == Some(true))
        .filter_map(|p| p.unseal_threshold)
        .max();
    vault_actions::select_threshold(from_pods, vault.http_threshold())
}

#[allow(clippy::too_many_arguments)]
fn show_unseal_section(
    ui: &mut egui::Ui,
    env_name: &str,
    vault: &VaultState,
    ctx: &EnvContext,
    mode: ThemeMode,
    vault_page: &mut VaultPage,
    state: &AppState,
    config: &Option<Config>,
    runtime: &tokio::runtime::Runtime,
) {
    let sealed_count = vault.sealed_pod_count();

    // Current or last run, kept until dismissed
    if let Some(run) = &ctx.run {
        egui::Frame::default()
            .fill(Colors::bg_base(mode))
            .corner_radius(6.0)
            .stroke(egui::Stroke::new(1.0_f32, Colors::border(mode)))
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                vault_banner::show_run(ui, run, mode);
                if run.keys_remembered {
                    ui.label(
                        egui::RichText::new("Keys remembered in the OS keyring for this environment.")
                            .size(11.0)
                            .color(Colors::text_secondary(mode)),
                    );
                }
                if !run.is_running() {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if run.succeeded()
                            && !run.keys_remembered
                            && vault_page.remember_offers.contains_key(env_name)
                        {
                            let label = format!("{} Remember keys on this machine", icons::KEY);
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(label).size(11.0).color(Colors::ACCENT),
                                    )
                                    .fill(egui::Color32::TRANSPARENT)
                                    .stroke(egui::Stroke::new(1.0_f32, Colors::ACCENT))
                                    .corner_radius(4.0),
                                )
                                .on_hover_text(
                                    "Stores the keys in the OS keyring and turns on remember_keys \
                                     for this environment until the app restarts. Persist it in Settings.",
                                )
                                .clicked()
                            {
                                if let Some(keys) = vault_page.remember_offers.remove(env_name) {
                                    match unseal_keys::remember_keys_in_keyring(env_name, &keys) {
                                        Ok(()) => {
                                            state.set_vault_remember_override(env_name, true);
                                            state.push_vault_event(format!(
                                                "{}: keys remembered in the keyring",
                                                env_name
                                            ));
                                        }
                                        Err(e) => state.push_vault_event(format!(
                                            "{}: could not store keys in the keyring: {}",
                                            env_name, e
                                        )),
                                    }
                                }
                            }
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Dismiss")
                                        .size(11.0)
                                        .color(Colors::text_secondary(mode)),
                                )
                                .fill(egui::Color32::TRANSPARENT)
                                .corner_radius(4.0),
                            )
                            .clicked()
                        {
                            state.dismiss_vault_run(env_name);
                            vault_page.remember_offers.remove(env_name);
                        }
                    });
                }
            });
        if run.is_running() {
            return;
        }
        ui.add_space(8.0);
    }

    if sealed_count == 0 {
        vault_page.unseal_panels.remove(env_name);
        return;
    }
    if vault.kubeconfig_path.is_none() {
        return;
    }

    // Primary: unseal from the configured key source
    if vault.key_source.is_automatic() {
        let label = format!(
            "{} Unseal now (keys: {})",
            icons::LOCK_OPEN,
            vault.key_source.label()
        );
        if ui
            .add(
                egui::Button::new(
                    egui::RichText::new(label)
                        .size(12.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                )
                .fill(Colors::GREEN)
                .corner_radius(4.0),
            )
            .clicked()
        {
            vault_banner::start_unseal(state, config, env_name, runtime);
        }
        ui.add_space(8.0);
    }

    // Manual entry
    if !vault_page.unseal_panels.contains_key(env_name) {
        let label = if vault.key_source.is_automatic() {
            format!("{} Enter keys manually", icons::KEY)
        } else if sealed_count == 1 {
            format!("{} Unseal 1 sealed pod", icons::KEY)
        } else {
            format!("{} Unseal {} sealed pods", icons::KEY, sealed_count)
        };
        if ui
            .add(
                egui::Button::new(egui::RichText::new(&label).size(12.0).color(Colors::ACCENT))
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::new(1.0_f32, Colors::ACCENT))
                    .corner_radius(4.0),
            )
            .clicked()
        {
            vault_page.open_manual(env_name);
        }
        return;
    }

    show_manual_panel(
        ui, env_name, vault, ctx, mode, vault_page, state, config, runtime,
    );
}

#[allow(clippy::too_many_arguments)]
fn show_manual_panel(
    ui: &mut egui::Ui,
    env_name: &str,
    vault: &VaultState,
    ctx: &EnvContext,
    mode: ThemeMode,
    vault_page: &mut VaultPage,
    state: &AppState,
    config: &Option<Config>,
    runtime: &tokio::runtime::Runtime,
) {
    let threshold = manual_threshold(vault);
    let sealed_count = vault.sealed_pod_count();
    let mut submit_key = false;
    let mut cancel = false;
    let mut start_unseal = false;

    egui::Frame::default()
        .fill(Colors::bg_base(mode))
        .corner_radius(6.0)
        .stroke(egui::Stroke::new(1.0_f32, Colors::border(mode)))
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let panel = vault_page.unseal_panels.get_mut(env_name).unwrap();
            let keys_collected = panel.keys.len() as u32;

            ui.horizontal(|ui| {
                ui.label(icons::icon(icons::KEY, 14.0, Colors::ACCENT));
                ui.label(
                    egui::RichText::new(format!(
                        "Unseal {}: {} pod{}",
                        env_name,
                        sealed_count,
                        if sealed_count == 1 { "" } else { "s" }
                    ))
                    .size(13.0)
                    .strong()
                    .color(Colors::text_primary(mode)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("Threshold: {}", threshold))
                            .size(11.0)
                            .color(Colors::text_muted(mode)),
                    );
                });
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                for i in 0..threshold {
                    let color = if i < keys_collected {
                        Colors::GREEN
                    } else {
                        Colors::text_muted(mode).linear_multiply(0.3)
                    };
                    ui.label(icons::icon(icons::CIRCLE, 8.0, color));
                }
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!(
                        "{}/{} keys entered",
                        keys_collected, threshold
                    ))
                    .size(11.0)
                    .color(Colors::text_secondary(mode)),
                );
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let input_id = egui::Id::new("unseal_key_input").with(env_name);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut *panel.key_input)
                        .id(input_id)
                        .password(true)
                        .desired_width(360.0)
                        .hint_text(format!(
                            "Enter unseal key {} of {}",
                            panel.keys.len() + 1,
                            threshold
                        ))
                        .font(egui::TextStyle::Monospace),
                );
                if panel.focus_input {
                    resp.request_focus();
                    panel.focus_input = false;
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let has_input = !panel.key_input.trim().is_empty();
                let add_clicked = ui
                    .add_enabled(
                        has_input,
                        egui::Button::new(
                            egui::RichText::new(format!("{} Add Key", icons::KEY))
                                .size(11.0)
                                .color(Colors::bg_base(mode)),
                        )
                        .fill(if has_input {
                            Colors::ACCENT
                        } else {
                            Colors::bg_hover(mode)
                        })
                        .corner_radius(4.0),
                    )
                    .clicked();
                if (add_clicked || enter) && has_input {
                    submit_key = true;
                }
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let has_keys = !panel.keys.is_empty();
                let all_keys = keys_collected >= threshold;
                let btn_label = if all_keys {
                    format!(
                        "{} Unseal {} pod{}",
                        icons::LOCK_OPEN,
                        sealed_count,
                        if sealed_count == 1 { "" } else { "s" }
                    )
                } else if has_keys {
                    format!(
                        "{} Submit {} key{}",
                        icons::KEY,
                        keys_collected,
                        if keys_collected == 1 { "" } else { "s" }
                    )
                } else {
                    format!("{} Submit keys", icons::KEY)
                };
                let btn_color = if all_keys {
                    Colors::GREEN
                } else if has_keys {
                    Colors::ACCENT
                } else {
                    Colors::bg_hover(mode)
                };
                let hover = if all_keys {
                    "All keys entered: will fully unseal".to_string()
                } else if has_keys {
                    format!(
                        "Partial unseal: {}/{} keys. Others can contribute the rest.",
                        keys_collected, threshold
                    )
                } else {
                    String::new()
                };
                if ui
                    .add_enabled(
                        has_keys,
                        egui::Button::new(egui::RichText::new(&btn_label).size(12.0).color(
                            if has_keys {
                                Colors::bg_base(mode)
                            } else {
                                Colors::text_muted(mode)
                            },
                        ))
                        .fill(btn_color)
                        .corner_radius(4.0),
                    )
                    .on_hover_text(hover)
                    .clicked()
                {
                    start_unseal = true;
                }
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Cancel")
                                .size(11.0)
                                .color(Colors::text_secondary(mode)),
                        )
                        .fill(egui::Color32::TRANSPARENT)
                        .corner_radius(4.0),
                    )
                    .clicked()
                {
                    cancel = true;
                }
            });

            if ctx.remember_enabled() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "remember_keys is on: keys are stored in the OS keyring after a successful unseal.",
                    )
                    .size(10.0)
                    .color(Colors::text_muted(mode)),
                );
            }
        });

    if submit_key {
        let panel = vault_page.unseal_panels.get_mut(env_name).unwrap();
        let key = std::mem::take(&mut *panel.key_input).trim().to_string();
        panel.keys.push(key);
        panel.focus_input = true;
    }

    if start_unseal {
        begin_manual_unseal(env_name, vault, ctx, vault_page, state, config, runtime);
    }

    if cancel {
        vault_page.unseal_panels.remove(env_name);
    }
}

/// Take the collected keys and start a manual run. Keys move into the async
/// task; a copy is kept only when the user may still choose to remember them.
fn begin_manual_unseal(
    env_name: &str,
    vault: &VaultState,
    ctx: &EnvContext,
    vault_page: &mut VaultPage,
    state: &AppState,
    config: &Option<Config>,
    runtime: &tokio::runtime::Runtime,
) {
    let Some(kubeconfig) = vault.kubeconfig_path.clone() else {
        return;
    };
    let Some(mut panel) = vault_page.unseal_panels.remove(env_name) else {
        return;
    };
    let keys: UnsealKeys = std::mem::take(&mut panel.keys);
    let targets = vault.sealed_targets();
    if targets.is_empty() || keys.is_empty() {
        return;
    }
    let remember = ctx.remember_enabled();
    if !remember {
        vault_page
            .remember_offers
            .insert(env_name.to_string(), Zeroizing::new(keys.to_vec()));
    }
    let started = vault_actions::spawn_unseal(
        runtime.handle(),
        state.clone(),
        env_name.to_string(),
        kubeconfig,
        targets,
        vault_actions::select_threshold(None, vault.http_threshold()),
        KeyInput::Keys(keys, KeySource::Manual),
        UnsealTrigger::Manual,
        remember,
        config.as_ref().map(|c| c.notifications.clone()),
    );
    if !started {
        vault_page.remember_offers.remove(env_name);
        state.push_vault_event(format!("{}: an unseal run is already going", env_name));
    }
}

fn format_age(duration: chrono::Duration) -> String {
    let secs = duration.num_seconds();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pollers::vault::VaultPodInfo;

    fn pod(sealed: Option<bool>, threshold: Option<u32>) -> VaultPodInfo {
        VaultPodInfo {
            name: "vault-0".to_string(),
            ready: false,
            sealed,
            unseal_progress: None,
            unseal_threshold: threshold,
            phase: "Running".to_string(),
            restarts: 0,
            created: None,
        }
    }

    #[test]
    fn manual_threshold_comes_from_sealed_pods_then_http_then_default() {
        let mut vs = VaultState {
            pods: vec![pod(Some(true), Some(2)), pod(Some(false), Some(5))],
            ..Default::default()
        };
        assert_eq!(manual_threshold(&vs), 2);
        vs.pods = vec![pod(Some(true), None)];
        assert_eq!(manual_threshold(&vs), 3);
    }
}
