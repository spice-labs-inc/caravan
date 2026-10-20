//! Resource usage panel (CPU / Memory) for the service detail Resources tab.
//!
//! Shows per-pod usage bars with current values vs limits/requests,
//! plus area charts of CPU and memory over time.
//! Polls metrics only while the tab is active; dropping `ResourceViewer` stops polling.

use crate::config::Config;
use crate::models::deployment::{PodInfo, PodMetrics};
use crate::pollers::metrics::{MetricsHandle, MetricsStore};
use crate::state::AppState;
use crate::ui::theme::{self, Colors, ThemeMode};
use chrono::{DateTime, Utc};
use eframe::egui;
use std::collections::HashMap;

/// Maximum number of history points to keep per pod (at 10s intervals, 360 = 1 hour).
const MAX_HISTORY: usize = 360;

/// A single time-series data point.
#[derive(Clone)]
struct MetricPoint {
    timestamp: DateTime<Utc>,
    cpu_millicores: f64,
    memory_bytes: u64,
}

/// State for a resource viewer instance (one per service detail view).
pub struct ResourceViewer {
    /// Active metrics polling handle. Drop to stop.
    handle: Option<MetricsHandle>,
    /// Status message
    status: String,
    /// Whether the user explicitly pressed Stop (prevents auto-restart).
    user_stopped: bool,
    /// Time-series history: (env, pod_name) → vec of data points.
    history: HashMap<(String, String), Vec<MetricPoint>>,
}

impl Default for ResourceViewer {
    fn default() -> Self {
        Self {
            handle: None,
            status: "Idle".to_string(),
            user_stopped: false,
            history: HashMap::new(),
        }
    }
}

impl ResourceViewer {
    /// Start polling metrics. Idempotent — won't restart if already running.
    pub fn ensure_polling(&mut self, runtime: &tokio::runtime::Runtime, config: &Option<Config>) {
        if self.handle.is_some() {
            return;
        }
        let Some(config) = config else {
            self.status = "No config available".to_string();
            return;
        };
        self.handle = Some(crate::pollers::metrics::start_metrics_polling(
            runtime, config,
        ));
        self.status = "Polling metrics…".to_string();
        self.user_stopped = false;
    }

    /// Stop polling metrics (called when navigating away).
    pub fn stop(&mut self) {
        if self.handle.is_some() {
            self.handle = None;
            self.status = "Stopped".to_string();
        }
    }

    /// Whether currently polling.
    pub fn is_polling(&self) -> bool {
        self.handle.is_some()
    }

    /// Get the metrics store (if polling).
    fn store(&self) -> Option<&MetricsStore> {
        self.handle.as_ref().map(|h| &h.store)
    }

    /// Ingest latest metrics snapshot into time-series history.
    fn update_history(&mut self, snapshot: &HashMap<String, Vec<PodMetrics>>) {
        let now = Utc::now();
        for (env_name, pod_metrics) in snapshot {
            for pm in pod_metrics {
                let key = (env_name.clone(), pm.name.clone());
                let series = self.history.entry(key).or_default();

                // Only add if timestamp is newer than last point
                let dominated = series
                    .last()
                    .is_some_and(|last| (now - last.timestamp).num_seconds() < 5);
                if dominated {
                    continue;
                }

                series.push(MetricPoint {
                    timestamp: now,
                    cpu_millicores: pm.cpu_millicores,
                    memory_bytes: pm.memory_bytes,
                });

                if series.len() > MAX_HISTORY {
                    series.drain(..series.len() - MAX_HISTORY);
                }
            }
        }
    }

    /// Render the resources panel.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        state: &AppState,
        service_name: &str,
        active_envs: &[&str],
        config: &Option<Config>,
        runtime: &tokio::runtime::Runtime,
        mode: ThemeMode,
    ) {
        // ── Controls bar ───────────────────────────────────────────
        ui.horizontal(|ui| {
            if self.is_polling() {
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
                    self.stop();
                    self.user_stopped = true;
                }
            } else if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(format!("{} Start", super::icons::PLAY))
                            .color(Colors::GREEN)
                            .size(12.0),
                    )
                    .fill(Colors::bg_elevated(mode)),
                )
                .clicked()
            {
                self.ensure_polling(runtime, config);
            }

            ui.separator();

            // Status
            let dot_color = if self.is_polling() {
                Colors::GREEN
            } else {
                Colors::text_muted(mode)
            };
            let dot = if self.is_polling() {
                super::icons::CIRCLE
            } else {
                super::icons::CIRCLE_DOT
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(super::icons::icon(dot, 9.0, dot_color));
                ui.label(
                    egui::RichText::new(&self.status)
                        .color(dot_color)
                        .size(11.0),
                );
            });

            if !self.history.is_empty() {
                let total_points: usize = self.history.values().map(|v| v.len()).sum();
                ui.label(
                    egui::RichText::new(format!("({} data points)", total_points))
                        .size(10.0)
                        .color(Colors::text_muted(mode)),
                );
            }
        });

        ui.add_space(8.0);

        // Auto-start when the tab becomes visible (not if user manually stopped)
        if !self.is_polling() && !self.user_stopped {
            self.ensure_polling(runtime, config);
        }

        // ── Ingest new metrics into history ────────────────────────
        // Use try_read() to avoid blocking the UI thread when the poller is writing
        let store_snapshot: Option<HashMap<String, Vec<PodMetrics>>> = self
            .store()
            .and_then(|s| s.try_read().ok().map(|g| g.clone()));

        if let Some(ref snapshot) = store_snapshot {
            self.update_history(snapshot);
        }

        // ── Per-environment resource display ───────────────────────
        for env_name in crate::ui::filters::env_order() {
            let env_name = env_name.as_str();
            if !active_envs.contains(&env_name) {
                continue;
            }

            // For vault, build PodInfo from the vault poller state (apps kubeconfig can't see vault pods)
            // For other services, get pods from the K8s environment state
            let owned_pods: Vec<PodInfo> = if service_name == "vault" {
                state.read(|s| {
                    s.vault_states
                        .get(env_name)
                        .map(|v| {
                            v.pods
                                .iter()
                                .map(|vp| PodInfo {
                                    name: vp.name.clone(),
                                    namespace: "vault".to_string(),
                                    phase: vp.phase.clone(),
                                    status: if vp.sealed == Some(true) {
                                        "Sealed".to_string()
                                    } else {
                                        "Running".to_string()
                                    },
                                    restarts: vp.restarts,
                                    created: vp.created,
                                    started_at: vp.created,
                                    ready: vp.sealed == Some(false),
                                    ready_count: "1/1".to_string(),
                                    images: vec![],
                                    node_name: None,
                                    reason: None,
                                    cpu_request_millicores: None,
                                    cpu_limit_millicores: None,
                                    memory_request_bytes: None,
                                    memory_limit_bytes: None,
                                    containers: vec![],
                                    volume_images: vec![],
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
            } else {
                let env_state = state.get_environment(env_name);
                let Some(ref env) = env_state else {
                    continue;
                };

                let hr = env.helm_releases.iter().find(|hr| hr.name == service_name);
                let Some(hr) = hr else { continue };

                let pod_namespace = if hr.is_fleet_instance() {
                    &hr.name
                } else {
                    &hr.namespace
                };

                env.pods
                    .iter()
                    .filter(|p| p.namespace == *pod_namespace)
                    .cloned()
                    .collect()
            };

            if owned_pods.is_empty() {
                continue;
            }
            let pods: Vec<&PodInfo> = owned_pods.iter().collect();

            // Get current metrics for these pods
            let env_metrics = store_snapshot.as_ref().and_then(|s| s.get(env_name));

            theme::section_header(ui, mode, &theme::env_label_full(env_name));

            // ── Area charts (aggregated across pods) ───────────────
            let pod_names: Vec<String> = pods.iter().map(|p| p.name.clone()).collect();

            {
                let aggregated = aggregate_history(&self.history, env_name, &pod_names);

                let cpu_limit: Option<f64> = pods
                    .iter()
                    .filter_map(|p| p.cpu_limit_millicores.or(p.cpu_request_millicores))
                    .reduce(|a, b| a + b);

                let mem_limit: Option<u64> = pods
                    .iter()
                    .filter_map(|p| p.memory_limit_bytes.or(p.memory_request_bytes))
                    .reduce(|a, b| a + b);

                theme::card(ui, mode, |ui| {
                    show_area_chart(
                        ui,
                        mode,
                        "CPU (cores)",
                        &aggregated,
                        |p| p.cpu_millicores / 1000.0,
                        cpu_limit.map(|l| l / 1000.0),
                        Colors::BLUE,
                        format!("cpu_chart_{}", env_name),
                    );

                    ui.add_space(8.0);

                    show_area_chart(
                        ui,
                        mode,
                        "Memory",
                        &aggregated,
                        |p| p.memory_bytes as f64 / (1024.0 * 1024.0),
                        mem_limit.map(|l| l as f64 / (1024.0 * 1024.0)),
                        Colors::PURPLE,
                        format!("mem_chart_{}", env_name),
                    );
                });
                ui.add_space(4.0);
            }

            // ── Per-pod current usage bars ─────────────────────────
            theme::card(ui, mode, |ui| {
                for pod in &pods {
                    let metrics = env_metrics.and_then(|m| m.iter().find(|pm| pm.name == pod.name));

                    ui.horizontal(|ui| {
                        let short = short_pod_name(&pod.name);
                        ui.label(
                            egui::RichText::new(&short)
                                .size(12.0)
                                .strong()
                                .color(Colors::text_primary(mode)),
                        );

                        let status_color = if pod.is_problematic() {
                            Colors::RED
                        } else if pod.phase == "Running" && pod.ready {
                            Colors::GREEN
                        } else {
                            Colors::YELLOW
                        };
                        ui.label(super::icons::icon(super::icons::CIRCLE, 8.0, status_color));
                    });

                    ui.add_space(4.0);

                    match metrics {
                        Some(m) => {
                            let cpu_limit = pod.cpu_limit_millicores.or(pod.cpu_request_millicores);
                            show_usage_bar(
                                ui,
                                mode,
                                "CPU",
                                m.cpu_millicores,
                                cpu_limit,
                                &format_cpu(m.cpu_millicores),
                                cpu_limit.map(format_cpu),
                                Colors::BLUE,
                            );

                            ui.add_space(2.0);

                            let mem_limit = pod
                                .memory_limit_bytes
                                .or(pod.memory_request_bytes)
                                .map(|v| v as f64);
                            show_usage_bar(
                                ui,
                                mode,
                                "MEM",
                                m.memory_bytes as f64,
                                mem_limit,
                                &format_memory(m.memory_bytes),
                                mem_limit.map(|l| format_memory(l as u64)),
                                Colors::PURPLE,
                            );
                        }
                        None => {
                            ui.label(
                                egui::RichText::new("  Waiting for metrics…")
                                    .size(11.0)
                                    .color(Colors::text_muted(mode))
                                    .italics(),
                            );
                        }
                    }

                    ui.add_space(8.0);
                }

                // Update status with metric count
                if let Some(env_m) = env_metrics {
                    let matched = pods
                        .iter()
                        .filter(|p| env_m.iter().any(|m| m.name == p.name))
                        .count();
                    if matched > 0 && self.is_polling() {
                        self.status = format!("Polling ({} pods)", matched);
                    }
                }
            });

            ui.add_space(8.0);
        }
    }
}

/// Aggregate time-series history across multiple pods into a single series.
/// Aligns by timestamp (within 5s tolerance) and sums values.
fn aggregate_history(
    history: &HashMap<(String, String), Vec<MetricPoint>>,
    env_name: &str,
    pod_names: &[String],
) -> Vec<MetricPoint> {
    // Collect all series for these pods
    let mut all_series: Vec<&Vec<MetricPoint>> = Vec::new();
    for pn in pod_names {
        if let Some(series) = history.get(&(env_name.to_string(), pn.clone())) {
            if !series.is_empty() {
                all_series.push(series);
            }
        }
    }

    if all_series.is_empty() {
        return Vec::new();
    }

    if all_series.len() == 1 {
        return all_series[0].clone();
    }

    // Use the first series' timestamps as the reference, sum values from other series
    // that have a matching timestamp (within 5s).
    let reference = all_series[0];
    let mut aggregated = Vec::with_capacity(reference.len());

    for point in reference {
        let mut cpu = point.cpu_millicores;
        let mut mem = point.memory_bytes;

        for other_series in &all_series[1..] {
            // Find closest point within 5 seconds
            if let Some(other) = other_series.iter().min_by_key(|op| {
                (op.timestamp - point.timestamp)
                    .num_seconds()
                    .unsigned_abs()
            }) {
                if (other.timestamp - point.timestamp)
                    .num_seconds()
                    .unsigned_abs()
                    <= 5
                {
                    cpu += other.cpu_millicores;
                    mem += other.memory_bytes;
                }
            }
        }

        aggregated.push(MetricPoint {
            timestamp: point.timestamp,
            cpu_millicores: cpu,
            memory_bytes: mem,
        });
    }

    aggregated
}

/// Render an area chart using egui_plot.
#[allow(clippy::too_many_arguments)]
fn show_area_chart(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    label: &str,
    data: &[MetricPoint],
    value_fn: fn(&MetricPoint) -> f64,
    limit: Option<f64>,
    accent: egui::Color32,
    id: String,
) {
    let is_cpu = label.contains("CPU");

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .size(12.0)
                .strong()
                .color(Colors::text_primary(mode)),
        );
        if let Some(last) = data.last() {
            let val = value_fn(last);
            let text = if is_cpu {
                format_cpu(val * 1000.0)
            } else {
                format_memory((val * 1024.0 * 1024.0) as u64)
            };
            ui.label(
                egui::RichText::new(format!("current: {}", text))
                    .size(11.0)
                    .color(Colors::text_muted(mode)),
            );
        }
        if let Some(lim) = limit {
            let lim_text = if is_cpu {
                format_cpu(lim * 1000.0)
            } else {
                format_memory((lim * 1024.0 * 1024.0) as u64)
            };
            ui.label(
                egui::RichText::new(format!("-- limit: {}", lim_text))
                    .size(11.0)
                    .color(Colors::RED.linear_multiply(0.6)),
            );
        }
    });

    ui.add_space(2.0);

    // Convert timestamps to seconds-ago for x-axis
    let now = Utc::now();
    let points: Vec<[f64; 2]> = data
        .iter()
        .map(|p| {
            let secs_ago = (now - p.timestamp).num_seconds() as f64;
            let x = -secs_ago; // negative so time goes left-to-right
            let y = value_fn(p);
            [x, y]
        })
        .collect();

    // Determine the fixed time window based on data span.
    // Steps up: 1m → 10m → 1h → 6h (only steps up, never back down)
    let data_span = if data.len() >= 2 {
        let oldest = data.first().unwrap();
        let newest = data.last().unwrap();
        (newest.timestamp - oldest.timestamp).num_seconds() as f64
    } else {
        0.0
    };
    let window_secs = if data_span < 55.0 {
        60.0 // 1 minute
    } else if data_span < 540.0 {
        600.0 // 10 minutes
    } else if data_span < 3300.0 {
        3600.0 // 1 hour
    } else {
        21600.0 // 6 hours
    };

    let line = egui_plot::Line::new(egui_plot::PlotPoints::new(points))
        .color(accent)
        .width(1.5_f32)
        .fill(0.0_f32)
        .name(label);

    let chart_height = 120.0;

    // Compute y range from data for a stable axis
    let y_max = if data.is_empty() {
        if let Some(lim) = limit {
            lim * 1.1
        } else {
            1.0
        }
    } else {
        let data_max = data.iter().map(&value_fn).fold(0.0_f64, f64::max);
        let upper = if let Some(lim) = limit {
            data_max.max(lim) * 1.1
        } else {
            data_max * 1.2
        };
        if upper < 1e-9 {
            1.0
        } else {
            upper
        }
    };

    let bounds = egui_plot::PlotBounds::from_min_max([-window_secs, 0.0], [0.0, y_max]);

    let mut plot = egui_plot::Plot::new(&id)
        .height(chart_height)
        .show_axes([true, true])
        .show_grid(true)
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .auto_bounds(egui::Vec2b::new(false, false))
        .x_axis_formatter(|mark, _range| {
            let secs = mark.value.abs();
            if secs < 0.5 {
                "now".to_string()
            } else if secs < 60.0 {
                // Only show multiples of 10s
                let rounded = (secs / 10.0).round() * 10.0;
                if (secs - rounded).abs() < 1.0 {
                    format!("{:.0}s", rounded)
                } else {
                    String::new()
                }
            } else if secs < 3600.0 {
                // Only show whole minutes
                let mins = secs / 60.0;
                if (mins - mins.round()).abs() < 0.1 {
                    format!("{:.0}m", mins.round())
                } else {
                    String::new()
                }
            } else {
                // Only show whole hours
                let hours = secs / 3600.0;
                if (hours - hours.round()).abs() < 0.05 {
                    format!("{:.0}h", hours.round())
                } else {
                    String::new()
                }
            }
        })
        .y_axis_formatter(move |mark, _range| {
            // Skip the zero label — it collides with the first x-axis label
            // in the bottom-left corner.
            if mark.value.abs() < 1e-12 {
                return String::new();
            }
            if is_cpu {
                format_cpu(mark.value * 1000.0)
            } else {
                format_memory((mark.value * 1024.0 * 1024.0) as u64)
            }
        })
        // Disable built-in coordinate tooltip — we show our own below
        .show_background(true)
        .show_x(false)
        .show_y(false);

    plot = plot
        .set_margin_fraction(egui::Vec2::new(0.0, 0.05))
        .clamp_grid(true);

    let plot_response = plot.show(ui, |plot_ui| {
        plot_ui.set_plot_bounds(bounds);
        plot_ui.line(line);

        if let Some(lim) = limit {
            let limit_line = egui_plot::Line::new(egui_plot::PlotPoints::new(vec![
                [-window_secs, lim],
                [0.0, lim],
            ]))
            .color(Colors::RED.linear_multiply(0.6))
            .width(1.0_f32)
            .style(egui_plot::LineStyle::dashed_dense())
            .name("limit");
            plot_ui.line(limit_line);
        }
    });

    // Custom tooltip that positions itself to stay within the window
    if let Some(hover_pos) = plot_response.response.hover_pos() {
        // Convert screen position to plot coordinates
        let plot_rect = plot_response.response.rect;
        if plot_rect.contains(hover_pos) {
            // Map hover x from screen to plot x (seconds ago)
            let x_frac = ((hover_pos.x - plot_rect.left()) / plot_rect.width()) as f64;
            let plot_x = -window_secs + x_frac * window_secs;

            // Find nearest data point
            let snap_points: Vec<(f64, f64, &DateTime<Utc>)> = data
                .iter()
                .map(|p| {
                    let secs_ago = (now - p.timestamp).num_seconds() as f64;
                    (-secs_ago, value_fn(p), &p.timestamp)
                })
                .collect();

            if let Some(&(_, y, ts)) = snap_points
                .iter()
                .min_by_key(|(x, _, _)| ((x - plot_x).abs() * 1000.0) as i64)
            {
                let local_ts = ts.with_timezone(&chrono::Local);
                let time_str = local_ts.format("%Y-%m-%d %H:%M:%S %Z").to_string();
                let ago = crate::ui::widgets::time_ago(ts);
                let val_str = if is_cpu {
                    format_cpu(y * 1000.0)
                } else {
                    format_memory((y * 1024.0 * 1024.0) as u64)
                };

                let tooltip_text = format!("{} ({})\n{}", time_str, ago, val_str);

                // Estimate tooltip size
                let tooltip_width = 280.0_f32;
                let screen_rect = ui.ctx().screen_rect();

                // Position: prefer right of cursor, but flip left if near right edge
                let tooltip_x = if hover_pos.x + tooltip_width + 16.0 > screen_rect.right() {
                    hover_pos.x - tooltip_width - 8.0
                } else {
                    hover_pos.x + 16.0
                };
                let tooltip_pos = egui::pos2(tooltip_x, hover_pos.y - 20.0);

                egui::containers::show_tooltip_at(
                    ui.ctx(),
                    ui.layer_id(),
                    egui::Id::new(&id).with("tooltip"),
                    tooltip_pos,
                    |ui| {
                        ui.label(
                            egui::RichText::new(tooltip_text)
                                .monospace()
                                .size(11.0)
                                .color(Colors::text_primary(mode)),
                        );
                    },
                );
            }
        }
    }
}

/// Draw a horizontal usage bar with label and values.
#[allow(clippy::too_many_arguments)]
fn show_usage_bar(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    label: &str,
    current: f64,
    limit: Option<f64>,
    current_text: &str,
    limit_text: Option<String>,
    accent: egui::Color32,
) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .size(10.0)
                .strong()
                .color(Colors::text_muted(mode))
                .monospace(),
        );

        ui.add_space(4.0);

        let bar_width = 200.0_f32;
        let bar_height = 14.0_f32;

        let (rect, _response) =
            ui.allocate_exact_size(egui::vec2(bar_width, bar_height), egui::Sense::hover());

        let painter = ui.painter_at(rect);

        // Background
        painter.rect_filled(rect, egui::CornerRadius::same(3), Colors::bg_elevated(mode));

        // Fill
        let fraction = if let Some(lim) = limit {
            if lim > 0.0 {
                (current / lim).clamp(0.0, 1.0) as f32
            } else {
                0.0
            }
        } else {
            0.5_f32
        };

        let fill_color = if fraction > 0.9 {
            Colors::RED
        } else if fraction > 0.7 {
            Colors::ORANGE
        } else {
            accent
        };

        if fraction > 0.0 {
            let fill_rect =
                egui::Rect::from_min_size(rect.min, egui::vec2(bar_width * fraction, bar_height));
            painter.rect_filled(fill_rect, egui::CornerRadius::same(3), fill_color);
        }

        // Text overlay
        let pct_text = if limit.is_some() {
            format!("{:.0}%", fraction * 100.0)
        } else {
            String::new()
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            &pct_text,
            egui::FontId::monospace(9.0),
            Colors::text_primary(mode),
        );

        ui.add_space(6.0);

        let value_text = match limit_text {
            Some(lt) => format!("{} / {}", current_text, lt),
            None => current_text.to_string(),
        };
        ui.label(
            egui::RichText::new(value_text)
                .size(11.0)
                .monospace()
                .color(Colors::text_secondary(mode)),
        );
    });
}

/// Format CPU millicores as decimal cores for display.
fn format_cpu(millicores: f64) -> String {
    let cores = millicores / 1000.0;
    if cores >= 10.0 {
        format!("{:.1}", cores)
    } else {
        format!("{:.3}", cores)
    }
}

/// Format memory bytes for display.
fn format_memory(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} Gi", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.0} Mi", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} Ki", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// Abbreviate a pod name (remove the replicaset hash for readability).
fn short_pod_name(name: &str) -> String {
    let parts: Vec<&str> = name.rsplitn(3, '-').collect();
    if parts.len() == 3 {
        format!("{}-{}", parts[2], parts[0])
    } else {
        name.to_string()
    }
}
