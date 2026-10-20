//! Pod log viewer panel with environment/pod selection, search, level filter, and tail mode.

use crate::config::Config;
use crate::pollers::logs::{LogLevel, LogLine, LogStreamHandle, LogStreamRequest};
use crate::state::AppState;
use crate::ui::theme::{self, Colors, ThemeMode};
use eframe::egui;
use std::collections::HashMap;

/// Context passed to the log viewer's show method.
pub struct LogViewerContext<'a> {
    pub state: &'a AppState,
    pub service_name: &'a str,
    pub config: &'a Option<Config>,
    pub runtime: &'a tokio::runtime::Runtime,
    pub mode: ThemeMode,
    pub env_order: &'a [&'a str],
    pub active_envs: &'a [&'a str],
}

/// Maximum number of log lines to keep in the buffer.
const MAX_LOG_LINES: usize = 10_000;

/// Default number of tail lines to fetch.
const DEFAULT_TAIL_LINES: i64 = 500;

/// Sentinel value for "All pods" selection.
const ALL_PODS: &str = "__all__";

/// State for a log viewer instance (one per service detail view).
pub struct LogViewer {
    /// All log lines in the buffer.
    lines: Vec<LogLine>,
    /// Active log stream handles (one per pod, or single for one pod).
    streams: Vec<LogStreamHandle>,
    /// Currently selected environment.
    pub selected_env: Option<String>,
    /// Currently selected pod (ALL_PODS for all, or a specific pod name).
    pub selected_pod: Option<String>,
    /// Search/filter text.
    search_text: String,
    /// Whether tail mode is active (auto-scroll to bottom).
    tail_mode: bool,
    /// Level filter: which levels to show.
    level_filter: HashMap<LogLevel, bool>,
    /// Whether we're streaming (follow mode) or one-shot.
    following: bool,
    /// Status message for the UI.
    status: String,
    /// Whether the viewer is initialized (first pod auto-selected).
    initialized: bool,
}

impl Default for LogViewer {
    fn default() -> Self {
        let mut level_filter = HashMap::new();
        level_filter.insert(LogLevel::Trace, true);
        level_filter.insert(LogLevel::Debug, true);
        level_filter.insert(LogLevel::Info, true);
        level_filter.insert(LogLevel::Warn, true);
        level_filter.insert(LogLevel::Error, true);
        level_filter.insert(LogLevel::Fatal, true);

        Self {
            lines: Vec::new(),
            streams: Vec::new(),
            selected_env: None,
            selected_pod: None,
            search_text: String::new(),
            tail_mode: true,
            level_filter,
            following: true,
            status: "Select a pod to view logs".to_string(),
            initialized: false,
        }
    }
}

impl LogViewer {
    /// Drain pending log lines from all stream channels into the buffer.
    pub fn poll_lines(&mut self) {
        if self.streams.is_empty() {
            return;
        }
        let mut count = 0;
        for handle in &mut self.streams {
            while let Ok(mut line) = handle.rx.try_recv() {
                if line.pod_name.is_none() {
                    line.pod_name = Some(handle.pod_name.clone());
                }
                self.lines.push(line);
                count += 1;
                if count >= 500 {
                    break;
                }
            }
        }
        // Sort by timestamp so interleaved pod logs are in order
        if count > 0 && self.streams.len() > 1 {
            self.lines.sort_by_key(|a| a.timestamp);
        }
        // Trim buffer
        if self.lines.len() > MAX_LOG_LINES {
            let drain = self.lines.len() - MAX_LOG_LINES;
            self.lines.drain(..drain);
        }
        if count > 0 {
            self.status = format!(
                "{} lines{}",
                self.lines.len(),
                if self.following { " (streaming)" } else { "" }
            );
        }
    }

    /// Start streaming logs for a specific pod with known namespace.
    pub fn start_stream_for_pod(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        config: &Config,
        env_name: &str,
        namespace: &str,
        pod_name: &str,
        container: Option<String>,
    ) {
        // Vault pods use the vault kubeconfig, not the apps kubeconfig
        let env_config = if namespace == "vault" {
            config.vault.get(env_name).and_then(|vc| {
                vc.kubeconfig
                    .as_ref()
                    .map(|kc| crate::config::EnvironmentConfig {
                        kubeconfig: kc.clone(),
                        ..crate::config::EnvironmentConfig::new(env_name)
                    })
            })
        } else {
            config.environments.get(env_name).cloned()
        };
        let Some(env_config) = env_config else {
            self.status = format!("No config for environment: {}", env_name);
            return;
        };

        // Stop existing streams
        self.streams.clear();
        self.lines.clear();

        self.selected_env = Some(env_name.to_string());
        self.selected_pod = Some(pod_name.to_string());

        // Vault pods need explicit container name (multi-container pod)
        let container = if namespace == "vault" && container.is_none() {
            Some("vault".to_string())
        } else {
            container
        };

        let request = LogStreamRequest {
            env_name: env_name.to_string(),
            env_config: env_config.clone(),
            namespace: namespace.to_string(),
            pod_name: pod_name.to_string(),
            container,
            tail_lines: DEFAULT_TAIL_LINES,
            follow: self.following,
        };

        self.status = format!("Connecting to {}/{}...", env_name, pod_name);

        let handle = crate::pollers::logs::start_log_stream(runtime, request);
        self.streams.push(handle);
    }

    /// Start streaming logs for all pods of a service in an environment.
    pub fn start_stream_all_pods(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        config: &Config,
        env_name: &str,
        pods: &[(String, String)], // (pod_name, namespace)
    ) {
        // Check if these are vault pods (namespace == "vault")
        let is_vault = pods.first().is_some_and(|(_, ns)| ns == "vault");
        let env_config = if is_vault {
            config.vault.get(env_name).and_then(|vc| {
                vc.kubeconfig
                    .as_ref()
                    .map(|kc| crate::config::EnvironmentConfig {
                        kubeconfig: kc.clone(),
                        ..crate::config::EnvironmentConfig::new(env_name)
                    })
            })
        } else {
            config.environments.get(env_name).cloned()
        };
        let Some(env_config) = env_config else {
            self.status = format!("No config for environment: {}", env_name);
            return;
        };

        self.streams.clear();
        self.lines.clear();

        self.selected_env = Some(env_name.to_string());
        self.selected_pod = Some(ALL_PODS.to_string());

        self.status = format!("Connecting to {} pods in {}...", pods.len(), env_name);

        for (pod_name, namespace) in pods {
            // Vault pods need explicit container name (multi-container pod)
            let container = if namespace == "vault" {
                Some("vault".to_string())
            } else {
                None
            };
            let request = LogStreamRequest {
                env_name: env_name.to_string(),
                env_config: env_config.clone(),
                namespace: namespace.to_string(),
                pod_name: pod_name.to_string(),
                container,
                tail_lines: DEFAULT_TAIL_LINES,
                follow: self.following,
            };
            let handle = crate::pollers::logs::start_log_stream(runtime, request);
            self.streams.push(handle);
        }
    }

    /// Stop all streams.
    pub fn stop_stream(&mut self) {
        self.streams.clear();
        self.following = false;
        self.status = format!("{} lines (stopped)", self.lines.len());
    }

    /// Render the log viewer panel.
    pub fn show(&mut self, ui: &mut egui::Ui, ctx: &LogViewerContext<'_>) {
        let state = ctx.state;
        let service_name = ctx.service_name;
        let config = ctx.config;
        let runtime = ctx.runtime;
        let mode = ctx.mode;
        let env_order = ctx.env_order;
        let active_envs = ctx.active_envs;
        // Poll for new lines
        self.poll_lines();

        // ── Controls bar ───────────────────────────────────────────────
        ui.horizontal(|ui| {
            // Environment selector
            let env_label = self
                .selected_env
                .as_deref()
                .map(theme::env_label)
                .unwrap_or_else(|| "Environment".to_string());
            egui::ComboBox::from_id_salt("log_env_select")
                .selected_text(env_label)
                .show_ui(ui, |ui| {
                    for env_name in env_order {
                        if !active_envs.contains(env_name) {
                            continue;
                        }
                        let label = &theme::env_label(env_name);
                        let selected = self.selected_env.as_deref() == Some(*env_name);
                        if ui.selectable_label(selected, label).clicked() {
                            self.selected_env = Some(env_name.to_string());
                            self.selected_pod = None;
                            self.streams.clear();
                            self.lines.clear();
                        }
                    }
                });

            // Pod selector (populated from state)
            if let Some(ref env_name) = self.selected_env.clone() {
                let pods = get_pods_for_service(state, env_name, service_name);

                let pod_label = match self.selected_pod.as_deref() {
                    Some(ALL_PODS) => format!("All ({} pods)", pods.len()),
                    Some(name) => short_pod_name(name),
                    None => "Select pod".to_string(),
                };

                egui::ComboBox::from_id_salt("log_pod_select")
                    .selected_text(pod_label)
                    .show_ui(ui, |ui| {
                        // "All" option
                        let all_selected = self.selected_pod.as_deref() == Some(ALL_PODS);
                        if ui
                            .selectable_label(all_selected, format!("All ({} pods)", pods.len()))
                            .clicked()
                            && !all_selected
                        {
                            if let Some(ref config) = config {
                                self.start_stream_all_pods(runtime, config, env_name, &pods);
                            }
                        }

                        ui.separator();

                        for (pod_name, namespace) in &pods {
                            let short = short_pod_name(pod_name);
                            let selected = self.selected_pod.as_deref() == Some(pod_name.as_str());
                            if ui.selectable_label(selected, &short).clicked()
                                && self.selected_pod.as_deref() != Some(pod_name.as_str())
                            {
                                if let Some(ref config) = config {
                                    self.start_stream_for_pod(
                                        runtime, config, env_name, namespace, pod_name, None,
                                    );
                                }
                            }
                        }
                    });

                // Auto-select all pods if not initialized
                if !self.initialized && self.selected_pod.is_none() && !pods.is_empty() {
                    self.initialized = true;
                    if let Some(ref config) = config {
                        self.start_stream_all_pods(runtime, config, env_name, &pods);
                    }
                }
            } else if !self.initialized {
                // Auto-select first available env
                for env_name in env_order {
                    if active_envs.contains(env_name) {
                        let pods = get_pods_for_service(state, env_name, service_name);
                        if !pods.is_empty() {
                            self.selected_env = Some(env_name.to_string());
                            break;
                        }
                    }
                }
            }

            ui.separator();

            // Follow/stop toggle
            if !self.streams.is_empty() {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(format!("{} Stop", super::icons::STOP))
                                .color(Colors::RED)
                                .size(12.0),
                        )
                        .fill(Colors::bg_elevated(mode)),
                    )
                    .clicked()
                {
                    self.stop_stream();
                }
            } else if self.selected_pod.is_some()
                && ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(format!("{} Stream", super::icons::PLAY))
                                .color(Colors::GREEN)
                                .size(12.0),
                        )
                        .fill(Colors::bg_elevated(mode)),
                    )
                    .clicked()
            {
                self.following = true;
                if let (Some(ref env_name), Some(ref pod_name)) =
                    (self.selected_env.clone(), self.selected_pod.clone())
                {
                    let pods = get_pods_for_service(state, env_name, service_name);
                    if pod_name == ALL_PODS {
                        if let Some(ref config) = config {
                            self.start_stream_all_pods(runtime, config, env_name, &pods);
                        }
                    } else if let Some((_, namespace)) = pods.iter().find(|(n, _)| n == pod_name) {
                        if let Some(ref config) = config {
                            self.start_stream_for_pod(
                                runtime, config, env_name, namespace, pod_name, None,
                            );
                        }
                    }
                }
            }

            // OpenObserve link for the selected pod, or the whole service
            let oo_url_set = config
                .as_ref()
                .is_some_and(|c| !c.openobserve.url.trim().is_empty());
            if let (Some(ref env_name), true) = (&self.selected_env, oo_url_set) {
                let (oo_url, oo_org) = config
                    .as_ref()
                    .map(|c| (c.openobserve.url.clone(), c.openobserve.org.clone()))
                    .unwrap_or_else(|| {
                        let d = crate::config::OpenObserveConfig::default();
                        (d.url, d.org)
                    });
                let pod = self.selected_pod.as_deref().filter(|p| *p != ALL_PODS);
                let (from_us, to_us) = self.link_time_range();
                let url = openobserve_logs_url(
                    &oo_url,
                    &oo_org,
                    env_name,
                    cluster_for_service(service_name),
                    service_name,
                    pod,
                    from_us,
                    to_us,
                );
                let label = match pod {
                    Some(p) => format!(
                        "{} OpenObserve: {}",
                        super::icons::ARROW_UP_RIGHT_FROM_SQUARE,
                        short_pod_name(p)
                    ),
                    None => format!("{} OpenObserve", super::icons::ARROW_UP_RIGHT_FROM_SQUARE),
                };
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new(label).size(12.0))
                            .fill(Colors::bg_elevated(mode)),
                    )
                    .on_hover_text("Open this stream in OpenObserve")
                    .clicked()
                {
                    let _ = open::that(&url);
                }
            }

            // Tail mode toggle
            let tail_color = if self.tail_mode {
                Colors::GREEN
            } else {
                Colors::text_muted(mode)
            };
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("⏬ Auto-scroll")
                            .color(tail_color)
                            .size(12.0),
                    )
                    .fill(Colors::bg_elevated(mode)),
                )
                .clicked()
            {
                self.tail_mode = !self.tail_mode;
            }

            // Clear button
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Clear")
                            .color(Colors::text_secondary(mode))
                            .size(12.0),
                    )
                    .fill(Colors::bg_elevated(mode)),
                )
                .clicked()
            {
                self.lines.clear();
            }

            ui.separator();

            // Level filter pills
            level_pill(ui, mode, &mut self.level_filter, LogLevel::Error, "ERR");
            level_pill(ui, mode, &mut self.level_filter, LogLevel::Warn, "WARN");
            level_pill(ui, mode, &mut self.level_filter, LogLevel::Info, "INFO");
            level_pill(ui, mode, &mut self.level_filter, LogLevel::Debug, "DBG");

            ui.separator();

            // Search
            ui.add(
                egui::TextEdit::singleline(&mut self.search_text)
                    .hint_text("Search logs...")
                    .desired_width(200.0)
                    .font(egui::FontId::monospace(12.0)),
            );
        });

        ui.add_space(4.0);

        // ── Status bar ─────────────────────────────────────────────────
        ui.horizontal(|ui| {
            let status_color = if !self.streams.is_empty() && self.following {
                Colors::GREEN
            } else {
                Colors::text_muted(mode)
            };
            let dot = if !self.streams.is_empty() && self.following {
                super::icons::CIRCLE
            } else {
                super::icons::CIRCLE_DOT
            };
            ui.label(super::icons::icon(dot, 9.0, status_color));
            ui.label(
                egui::RichText::new(&self.status)
                    .color(status_color)
                    .size(11.0),
            );
        });

        ui.add_space(4.0);

        // ── Log lines ──────────────────────────────────────────────────
        let filtered_lines = self.filtered_lines();
        let search_lower = self.search_text.to_lowercase();

        // Reverse so newest lines appear at the top
        let reversed: Vec<_> = filtered_lines.into_iter().rev().collect();

        egui::Frame::default()
            .fill(Colors::bg_base(mode))
            .corner_radius(8.0)
            .inner_margin(8.0)
            .show(ui, |ui| {
                let row_height = 16.0;
                let total_rows = reversed.len();
                let multi_pod = self.selected_pod.as_deref() == Some(ALL_PODS);

                // available_height() is finite here because AppDetail is NOT
                // wrapped in an outer ScrollArea (app.rs handles this).
                let log_height = ui.available_height().clamp(300.0, 2000.0);

                // Column count depends on multi_pod — separate table ids so
                // egui_extras doesn't reuse stale column state.
                ui.push_id(("log_table", multi_pod), |ui| {
                    use egui_extras::{Column, TableBuilder};
                    let mut table = TableBuilder::new(ui)
                        .auto_shrink([false, false])
                        .max_scroll_height(log_height)
                        .vscroll(true)
                        .column(Column::exact(COL_AGO))
                        .column(Column::exact(COL_TIMESTAMP))
                        .column(Column::exact(COL_LEVEL));
                    if multi_pod {
                        table = table.column(Column::exact(COL_POD));
                    }
                    table = table.column(Column::remainder().clip(true));

                    let hdr = |ui: &mut egui::Ui, t: &str| {
                        ui.label(
                            egui::RichText::new(t)
                                .size(10.0)
                                .strong()
                                .color(Colors::text_muted(mode)),
                        );
                    };
                    table
                        .header(18.0, |mut header| {
                            header.col(|ui| hdr(ui, "AGO"));
                            header.col(|ui| hdr(ui, "TIME"));
                            header.col(|ui| hdr(ui, "LEVEL"));
                            if multi_pod {
                                header.col(|ui| hdr(ui, "POD"));
                            }
                            header.col(|ui| hdr(ui, "MESSAGE"));
                        })
                        .body(|body| {
                            body.rows(row_height, total_rows, |mut row| {
                                let line = reversed[row.index()];
                                render_log_row(&mut row, line, mode, &search_lower, multi_pod);
                            });
                        });
                });
            });
    }

    /// Get lines filtered by level and search text.
    fn filtered_lines(&self) -> Vec<&LogLine> {
        let search_lower = self.search_text.to_lowercase();
        self.lines
            .iter()
            .filter(|line| {
                // Level filter
                if let Some(level) = &line.level {
                    if !self.level_filter.get(level).copied().unwrap_or(true) {
                        return false;
                    }
                }
                // Search filter
                if !search_lower.is_empty() && !line.text.to_lowercase().contains(&search_lower) {
                    return false;
                }
                true
            })
            .collect()
    }
}

/// Fixed column widths for log line layout.
const COL_AGO: f32 = 65.0;
const COL_TIMESTAMP: f32 = 195.0;
const COL_LEVEL: f32 = 50.0;
const COL_POD: f32 = 110.0;

/// Render a single log line as one table row.
/// The message column clips to its cell; hover shows the full text.
fn render_log_row(
    row: &mut egui_extras::TableRow<'_, '_>,
    line: &LogLine,
    mode: ThemeMode,
    search_lower: &str,
    show_pod_name: bool,
) {
    // Relative time
    row.col(|ui| {
        if let Some(ref ts) = line.timestamp {
            ui.label(
                egui::RichText::new(crate::ui::widgets::time_ago(ts))
                    .monospace()
                    .size(11.0)
                    .color(Colors::BLUE),
            );
        }
    });

    // Timestamp
    row.col(|ui| {
        if let Some(ref ts) = line.timestamp {
            let local_ts = ts.with_timezone(&chrono::Local);
            let time_str = local_ts.format("%Y-%m-%d %H:%M:%S %Z").to_string();
            ui.label(
                egui::RichText::new(time_str)
                    .monospace()
                    .size(11.0)
                    .color(Colors::text_muted(mode)),
            );
        }
    });

    // Level
    row.col(|ui| {
        if let Some(ref level) = line.level {
            let (label, color) = level_style(level, mode);
            ui.label(
                egui::RichText::new(label)
                    .monospace()
                    .size(11.0)
                    .color(color)
                    .strong(),
            );
        }
    });

    // Pod
    if show_pod_name {
        row.col(|ui| {
            if let Some(ref pod) = line.pod_name {
                ui.label(
                    egui::RichText::new(short_pod_name(pod))
                        .monospace()
                        .size(10.0)
                        .color(Colors::PURPLE),
                )
                .on_hover_text(pod);
            }
        });
    }

    // Message (clipped to the cell; full text on hover)
    let display_text = format_log_text(&line.text);
    let (_, msg_resp) = row.col(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let text_color = line
            .level
            .as_ref()
            .map(|l| level_text_color(l, mode))
            .unwrap_or_else(|| Colors::text_primary(mode));

        let mut highlighted = false;
        if !search_lower.is_empty() {
            let lower = display_text.to_lowercase();
            if let Some(start) = lower.find(search_lower) {
                highlighted = true;
                let before = &display_text[..start];
                let matched = &display_text[start..start + search_lower.len()];
                let after = &display_text[start + search_lower.len()..];
                ui.label(
                    egui::RichText::new(before)
                        .monospace()
                        .size(11.0)
                        .color(text_color),
                );
                ui.label(
                    egui::RichText::new(matched)
                        .monospace()
                        .size(11.0)
                        .color(Colors::bg_base(mode))
                        .background_color(Colors::YELLOW),
                );
                ui.label(
                    egui::RichText::new(after)
                        .monospace()
                        .size(11.0)
                        .color(text_color),
                );
            }
        }
        if !highlighted {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&display_text)
                        .monospace()
                        .size(11.0)
                        .color(text_color),
                )
                .truncate(),
            );
        }
    });

    let msg_resp = msg_resp.on_hover_text(&display_text);

    // Right-click context menu to copy log line
    msg_resp.context_menu(|ui| {
        if ui.button("Copy log line").clicked() {
            ui.ctx().copy_text(line.text.clone());
            ui.close_menu();
        }
        if ui.button("Copy formatted").clicked() {
            let mut parts = Vec::new();
            if let Some(ref ts) = line.timestamp {
                parts.push(crate::ui::widgets::time_ago(ts));
                let local_ts = ts.with_timezone(&chrono::Local);
                parts.push(local_ts.format("%Y-%m-%d %H:%M:%S %Z").to_string());
            }
            if let Some(ref level) = line.level {
                let (label, _) = level_style(level, mode);
                parts.push(label.to_string());
            }
            if let Some(ref pod) = line.pod_name {
                parts.push(format!("[{}]", short_pod_name(pod)));
            }
            parts.push(format_log_text(&line.text));
            ui.ctx().copy_text(parts.join(" "));
            ui.close_menu();
        }
    });
}

/// Extract the message and key fields from a JSON log line for display.
fn format_log_text(text: &str) -> String {
    if !text.starts_with('{') {
        return text.to_string();
    }

    let Ok(val) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.to_string();
    };

    // Extract the message field (try common names)
    let message = val
        .get("message")
        .or_else(|| val.get("msg"))
        .and_then(|v| v.as_str());

    // For Rust tracing-style: {"level":"INFO","fields":{"message":"..."}}
    let fields_message = val
        .get("fields")
        .and_then(|f| f.get("message"))
        .and_then(|v| v.as_str());

    let msg = message.or(fields_message);

    // Extract useful context fields
    let target = val.get("target").and_then(|v| v.as_str());
    let logger = val
        .get("logger_name")
        .or_else(|| val.get("logger"))
        .and_then(|v| v.as_str());
    let _thread = val
        .get("thread_name")
        .or_else(|| val.get("thread"))
        .and_then(|v| v.as_str());
    let _trace_parent = val.get("traceParent").and_then(|v| v.as_str());

    if let Some(msg) = msg {
        let mut parts = vec![msg.to_string()];

        // Add target/logger for context (shortened)
        if let Some(t) = target.or(logger) {
            let short = t.rsplit("::").next().unwrap_or(t);
            // Only add if it's informative (not just the class name repeated in the message)
            if !msg.contains(short) {
                parts.push(format!("[{}]", short));
            }
        }

        parts.join("  ")
    } else {
        // No message field found — show raw JSON compactly
        text.to_string()
    }
}

/// Style for a log level badge.
fn level_style(level: &LogLevel, _mode: ThemeMode) -> (&'static str, egui::Color32) {
    match level {
        LogLevel::Trace => ("TRACE", Colors::GRAY),
        LogLevel::Debug => ("DEBUG", Colors::BLUE),
        LogLevel::Info => (" INFO", Colors::GREEN),
        LogLevel::Warn => (" WARN", Colors::ORANGE),
        LogLevel::Error => ("ERROR", Colors::RED),
        LogLevel::Fatal => ("FATAL", Colors::RED),
    }
}

/// Text color for log line body based on level.
fn level_text_color(level: &LogLevel, mode: ThemeMode) -> egui::Color32 {
    match level {
        LogLevel::Error | LogLevel::Fatal => Colors::RED,
        LogLevel::Warn => Colors::ORANGE,
        LogLevel::Debug | LogLevel::Trace => Colors::text_muted(mode),
        _ => Colors::text_primary(mode),
    }
}

/// Level filter pill toggle button.
fn level_pill(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    filter: &mut HashMap<LogLevel, bool>,
    level: LogLevel,
    label: &str,
) {
    let active = filter.get(&level).copied().unwrap_or(true);
    let (_, color) = level_style(&level, mode);
    let display_color = if active { color } else { Colors::GRAY };

    if ui
        .add(
            egui::Button::new(
                egui::RichText::new(label)
                    .color(display_color)
                    .strong()
                    .size(10.0),
            )
            .fill(if active {
                color.linear_multiply(0.15)
            } else {
                Colors::bg_elevated(mode)
            })
            .corner_radius(12.0)
            .min_size(egui::vec2(0.0, 20.0)),
        )
        .clicked()
    {
        filter.insert(level, !active);
    }
}

/// Abbreviate a pod name (remove the replicaset hash for readability).
fn short_pod_name(name: &str) -> String {
    // Pod names like "api-5b78df97d-4ss9v" → "api-4ss9v"
    let parts: Vec<&str> = name.rsplitn(3, '-').collect();
    if parts.len() == 3 {
        format!("{}-{}", parts[2], parts[0])
    } else {
        name.to_string()
    }
}

/// Get pods for a service in an environment from AppState.
/// Returns vec of (pod_name, namespace).
fn get_pods_for_service(
    state: &AppState,
    env_name: &str,
    service_name: &str,
) -> Vec<(String, String)> {
    // Vault pods are discovered via the vault poller, not HelmReleases
    if service_name == "vault" {
        return state.read(|s| {
            s.vault_states
                .get(env_name)
                .map(|v| {
                    v.pods
                        .iter()
                        .map(|p| (p.name.clone(), "vault".to_string()))
                        .collect()
                })
                .unwrap_or_default()
        });
    }

    state.read(|s| {
        let Some(env) = s.environments.get(env_name) else {
            return Vec::new();
        };

        // Find the namespace for this service from HelmReleases.
        // Fleet instances: the HR lives in the operator namespace but pods run in
        // a namespace matching the HR name (e.g. "project-{uuid}").
        let namespace = env
            .helm_releases
            .iter()
            .find(|hr| hr.name == service_name)
            .map(|hr| {
                if hr.is_fleet_instance() {
                    hr.name.clone()
                } else {
                    hr.namespace.clone()
                }
            });

        let Some(namespace) = namespace else {
            return Vec::new();
        };

        env.pods
            .iter()
            .filter(|p| p.namespace == namespace)
            .map(|p| (p.name.clone(), p.namespace.clone()))
            .collect()
    })
}

impl LogViewer {
    /// Time range for an OpenObserve link, in microseconds: from the oldest
    /// buffered line (or the last hour) up to now.
    fn link_time_range(&self) -> (i64, i64) {
        let now = chrono::Utc::now();
        let oldest = self
            .lines
            .iter()
            .filter_map(|l| l.timestamp)
            .min()
            .unwrap_or(now - chrono::Duration::hours(1));
        (oldest.timestamp_micros(), now.timestamp_micros())
    }
}

/// The k8s cluster segment of a service's log stream: vault pods live on the
/// env's vault cluster, everything else on its apps cluster.
fn cluster_for_service(service: &str) -> &'static str {
    if service == "vault" {
        "vault"
    } else {
        "apps"
    }
}

/// OpenObserve log stream name `{environment}:{k8s_cluster_name}:{service_name}`.
/// OpenObserve turns hyphens and dots into underscores and keeps the colons.
pub fn openobserve_stream(env: &str, cluster: &str, service: &str) -> String {
    let norm = |s: &str| {
        s.chars()
            .map(|c| if c == '-' || c == '.' { '_' } else { c })
            .collect::<String>()
    };
    format!("{}:{}:{}", norm(env), norm(cluster), norm(service))
}

/// Link to the OpenObserve logs page for a stream over a microsecond time
/// range, optionally filtered to one pod. The page reads `query` as base64.
#[allow(clippy::too_many_arguments)]
pub fn openobserve_logs_url(
    base_url: &str,
    org: &str,
    env: &str,
    cluster: &str,
    service: &str,
    pod: Option<&str>,
    from_us: i64,
    to_us: i64,
) -> String {
    use base64::Engine;
    let stream = openobserve_stream(env, cluster, service);
    let mut url = format!(
        "{}/web/logs?org_identifier={}&stream_type=logs&stream={}&from={}&to={}&refresh=0&sql_mode=false",
        base_url.trim_end_matches('/'),
        urlencoding::encode(org),
        urlencoding::encode(&stream),
        from_us,
        to_us
    );
    if let Some(pod) = pod {
        let query = format!("k8s_pod_name='{}'", pod.replace('\'', ""));
        let encoded = base64::engine::general_purpose::STANDARD.encode(query);
        url.push_str("&query=");
        url.push_str(&urlencoding::encode(&encoded));
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_name_follows_the_collector_convention() {
        assert_eq!(openobserve_stream("prod", "apps", "api"), "prod:apps:api");
        assert_eq!(
            openobserve_stream("shared-services", "admin", "helm-controller"),
            "shared_services:admin:helm_controller"
        );
        assert_eq!(
            openobserve_stream("prod", "vault", "vault"),
            "prod:vault:vault"
        );
        assert_eq!(cluster_for_service("vault"), "vault");
        assert_eq!(cluster_for_service("api"), "apps");
    }

    #[test]
    fn logs_url_carries_stream_range_and_pod_filter() {
        let url = openobserve_logs_url(
            "https://oo.example/",
            "default",
            "dev",
            "apps",
            "pqc-rogues-gallery",
            Some("pqc-rogues-gallery-5b78df97d-4ss9v"),
            1_700_000_000_000_000,
            1_700_000_060_000_000,
        );
        assert!(url.starts_with("https://oo.example/web/logs?org_identifier=default"));
        assert!(url.contains("&stream=dev%3Aapps%3Apqc_rogues_gallery"));
        assert!(url.contains("&from=1700000000000000&to=1700000060000000"));
        use base64::Engine;
        let expected = base64::engine::general_purpose::STANDARD
            .encode("k8s_pod_name='pqc-rogues-gallery-5b78df97d-4ss9v'");
        assert!(url.ends_with(&format!("&query={}", urlencoding::encode(&expected))));

        let whole = openobserve_logs_url(
            "https://oo.example",
            "default",
            "prod",
            "apps",
            "api",
            None,
            1,
            2,
        );
        assert!(!whole.contains("&query="));
        assert!(whole.contains("stream=prod%3Aapps%3Aapi"));
    }
}
