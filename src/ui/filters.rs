use crate::models::deployment::DeploymentStatus;
use crate::state::AppState;
use crate::ui::theme::{self, Colors, ThemeMode};
use chrono::{DateTime, Utc};
use eframe::egui;
use std::collections::HashSet;

/// The configured environment ladder, display order.
pub fn env_order() -> Vec<String> {
    crate::models::environments::current().order()
}

/// Time period for filtering problems and other time-sensitive data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimePeriod {
    Hours1,
    Hours6,
    Hours12,
    Hours24,
    Days3,
    Days7,
    All,
}

impl TimePeriod {
    pub fn label(&self) -> &'static str {
        match self {
            TimePeriod::Hours1 => "1 hour",
            TimePeriod::Hours6 => "6 hours",
            TimePeriod::Hours12 => "12 hours",
            TimePeriod::Hours24 => "24 hours",
            TimePeriod::Days3 => "3 days",
            TimePeriod::Days7 => "7 days",
            TimePeriod::All => "All time",
        }
    }

    /// Returns the cutoff time for this period (anything before this is excluded)
    pub fn cutoff(&self) -> Option<DateTime<Utc>> {
        let duration = match self {
            TimePeriod::Hours1 => chrono::Duration::hours(1),
            TimePeriod::Hours6 => chrono::Duration::hours(6),
            TimePeriod::Hours12 => chrono::Duration::hours(12),
            TimePeriod::Hours24 => chrono::Duration::hours(24),
            TimePeriod::Days3 => chrono::Duration::days(3),
            TimePeriod::Days7 => chrono::Duration::days(7),
            TimePeriod::All => return None,
        };
        Some(Utc::now() - duration)
    }

    /// All available time periods in order
    pub const ALL: &'static [TimePeriod] = &[
        TimePeriod::Hours1,
        TimePeriod::Hours6,
        TimePeriod::Hours12,
        TimePeriod::Hours24,
        TimePeriod::Days3,
        TimePeriod::Days7,
        TimePeriod::All,
    ];
}

/// Global filter state shared across all views
pub struct GlobalFilters {
    /// Which envs are visible
    pub visible_envs: HashSet<String>,
    /// Time period for filtering problems
    pub time_period: TimePeriod,
}

impl Default for GlobalFilters {
    fn default() -> Self {
        Self {
            visible_envs: env_order().into_iter().collect(),
            time_period: TimePeriod::Hours12,
        }
    }
}

impl GlobalFilters {
    pub fn active_envs(&self) -> Vec<&str> {
        let envs = crate::models::environments::current();
        let mut active: Vec<&str> = self.visible_envs.iter().map(String::as_str).collect();
        active.sort_by_key(|e| envs.rank(e));
        active
    }

    /// Load saved filter state from cache
    pub fn load_from_cache(&mut self, cache: &crate::cache::Cache) {
        if let Ok(Some(period_str)) = cache.get_meta("filter_time_period") {
            self.time_period = TimePeriod::from_str(&period_str);
        }
        if let Ok(Some(envs_str)) = cache.get_meta("filter_visible_envs") {
            let envs: HashSet<String> = envs_str.split(',').map(|s| s.to_string()).collect();
            if !envs.is_empty() {
                self.visible_envs = envs;
            }
        }
    }

    /// Save current filter state to cache
    pub fn save_to_cache(&self, cache: &crate::cache::Cache) {
        let _ = cache.set_meta("filter_time_period", self.time_period.as_str());
        let envs_str: String = self
            .visible_envs
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        let _ = cache.set_meta("filter_visible_envs", &envs_str);
    }
}

impl TimePeriod {
    pub fn as_str(self) -> &'static str {
        match self {
            TimePeriod::Hours1 => "1h",
            TimePeriod::Hours6 => "6h",
            TimePeriod::Hours12 => "12h",
            TimePeriod::Hours24 => "24h",
            TimePeriod::Days3 => "3d",
            TimePeriod::Days7 => "7d",
            TimePeriod::All => "all",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "1h" => TimePeriod::Hours1,
            "6h" => TimePeriod::Hours6,
            "12h" => TimePeriod::Hours12,
            "24h" => TimePeriod::Hours24,
            "3d" => TimePeriod::Days3,
            "7d" => TimePeriod::Days7,
            "all" => TimePeriod::All,
            _ => TimePeriod::Hours12,
        }
    }
}

/// Render the environment filter pill bar.
pub fn show_env_filter_bar(
    ui: &mut egui::Ui,
    state: &AppState,
    filters: &mut GlobalFilters,
    mode: ThemeMode,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;

        // Time period dropdown
        ui.label(
            egui::RichText::new('\u{f017}'.to_string())
                .family(egui::FontFamily::Name("fa-solid".into()))
                .size(13.0)
                .color(Colors::text_muted(mode)),
        );
        egui::ComboBox::from_id_salt("time_period")
            .selected_text(
                egui::RichText::new(filters.time_period.label())
                    .size(12.0)
                    .color(Colors::text_primary(mode)),
            )
            .width(85.0)
            .show_ui(ui, |ui| {
                for &period in TimePeriod::ALL {
                    ui.selectable_value(&mut filters.time_period, period, period.label());
                }
            });

        ui.add_space(12.0);
        ui.label(
            egui::RichText::new("│")
                .size(14.0)
                .color(Colors::text_muted(mode)),
        );
        ui.add_space(4.0);

        // "All" pill
        let all_active = filters.visible_envs.len() == env_order().len();
        if theme::pill_button(ui, mode, "All", all_active) {
            filters.visible_envs = env_order().into_iter().collect();
        }

        for env_name in env_order() {
            let env_name = env_name.as_str();
            let active = filters.visible_envs.contains(env_name);
            let env_state = state.get_environment(env_name);
            let connected = env_state.as_ref().is_some_and(|e| e.connected);
            let has_problems = env_state.as_ref().is_some_and(|e| {
                e.helm_releases
                    .iter()
                    .any(|hr| hr.status != DeploymentStatus::Healthy)
                    || e.pods.iter().any(|p| p.is_problematic())
            });

            // Build label with status dot
            let dot = if connected {
                super::icons::CIRCLE.to_string()
            } else {
                super::icons::CIRCLE_DOT.to_string()
            };
            let dot_color = if !connected {
                Colors::text_muted(mode)
            } else if has_problems {
                Colors::YELLOW
            } else {
                Colors::GREEN
            };

            let label_text = &theme::env_label_full(env_name);

            // Custom pill with colored dot
            let bg = if active {
                Colors::text_primary(mode)
            } else {
                Colors::bg_elevated(mode)
            };
            let fg = if active {
                Colors::bg_base(mode)
            } else {
                Colors::text_primary(mode)
            };

            let response = ui.add(
                egui::Button::new({
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        &format!("{} ", dot),
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::new(
                                10.0,
                                egui::FontFamily::Name(super::icons::FA_SOLID.into()),
                            ),
                            color: if active { fg } else { dot_color },
                            ..Default::default()
                        },
                    );
                    job.append(
                        label_text,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(13.0),
                            color: fg,
                            ..Default::default()
                        },
                    );
                    job
                })
                .fill(bg)
                .corner_radius(20.0)
                .min_size(egui::vec2(0.0, 32.0)),
            );

            if response.clicked() {
                let modifier = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
                if modifier {
                    // Ctrl/Cmd+click: toggle this env on/off (but keep at least one)
                    if active && filters.visible_envs.len() > 1 {
                        filters.visible_envs.remove(env_name);
                    } else if !active {
                        filters.visible_envs.insert(env_name.to_string());
                    }
                } else {
                    // Plain click: select only this env (or re-select all if already solo)
                    if filters.visible_envs.len() == 1 && active {
                        // Already solo on this env → go back to all
                        filters.visible_envs = env_order().into_iter().collect();
                    } else {
                        // Select only this one
                        filters.visible_envs.clear();
                        filters.visible_envs.insert(env_name.to_string());
                    }
                }
            }
        }
    });
}
