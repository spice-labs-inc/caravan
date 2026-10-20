use crate::ui::theme::{Colors, ThemeMode};
use eframe::egui;
use std::collections::VecDeque;
use std::time::Instant;

const TOAST_DURATION_SECS: f32 = 3.0;
const FADE_DURATION_SECS: f32 = 0.5;
const MAX_TOASTS: usize = 5;

#[derive(Clone)]
pub struct Toast {
    pub message: String,
    pub icon: String,
    pub created: Instant,
}

impl Toast {
    fn age_secs(&self) -> f32 {
        self.created.elapsed().as_secs_f32()
    }

    fn opacity(&self) -> f32 {
        let age = self.age_secs();
        let total = TOAST_DURATION_SECS + FADE_DURATION_SECS;
        if age < 0.15 {
            // Fade in
            age / 0.15
        } else if age < TOAST_DURATION_SECS {
            1.0
        } else if age < total {
            1.0 - (age - TOAST_DURATION_SECS) / FADE_DURATION_SECS
        } else {
            0.0
        }
    }

    fn is_expired(&self) -> bool {
        self.age_secs() > TOAST_DURATION_SECS + FADE_DURATION_SECS
    }
}

#[derive(Default)]
pub struct Toaster {
    toasts: VecDeque<Toast>,
}

impl Toaster {
    pub fn push(&mut self, icon: &str, message: impl Into<String>) {
        self.toasts.push_back(Toast {
            message: message.into(),
            icon: icon.to_string(),
            created: Instant::now(),
        });
        while self.toasts.len() > MAX_TOASTS {
            self.toasts.pop_front();
        }
    }

    /// Render toasts at top-center of the screen. Call from the main update loop.
    pub fn show(&mut self, ctx: &egui::Context, mode: ThemeMode) {
        // Remove expired
        self.toasts.retain(|t| !t.is_expired());

        if self.toasts.is_empty() {
            return;
        }

        // Request repaint while toasts are animating
        ctx.request_repaint();

        let screen = ctx.screen_rect();
        let center_x = screen.center().x;

        egui::Area::new(egui::Id::new("toasts"))
            .fixed_pos(egui::pos2(center_x, screen.min.y + 8.0))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 0.0))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    for toast in &self.toasts {
                        let opacity = toast.opacity();
                        if opacity <= 0.0 {
                            continue;
                        }

                        let bg = Colors::bg_elevated(mode).gamma_multiply(opacity);
                        let text_color = Colors::text_primary(mode).gamma_multiply(opacity);
                        let icon_color = Colors::text_secondary(mode).gamma_multiply(opacity);

                        egui::Frame::NONE
                            .fill(bg)
                            .corner_radius(8.0)
                            .inner_margin(egui::Margin::symmetric(16, 8))
                            .shadow(egui::epaint::Shadow {
                                offset: [0, 2],
                                blur: 8,
                                spread: 0,
                                color: egui::Color32::from_black_alpha((40.0 * opacity) as u8),
                            })
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(&toast.icon)
                                            .size(14.0)
                                            .color(icon_color),
                                    );
                                    ui.label(
                                        egui::RichText::new(&toast.message)
                                            .size(13.0)
                                            .color(text_color),
                                    );
                                });
                            });
                    }
                });
            });
    }
}
