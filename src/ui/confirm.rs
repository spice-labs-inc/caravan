//! Tiered confirmation dialog system for write operations.
//!
//! Three tiers:
//! - **Low**: Simple "Are you sure?" tooltip on hover
//! - **Medium**: Modal dialog, type the resource name to confirm
//! - **High**: Modal dialog, type a confirmation phrase + 3-second countdown

use eframe::egui;

use crate::ui::icons;
use crate::ui::theme::{Colors, ThemeMode};

/// Confirmation tier determines the required level of user interaction.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum ConfirmTier {
    /// Click + "Are you sure?" tooltip
    Low,
    /// Modal: type the resource name to confirm
    Medium {
        /// What the user must type (e.g. "vault-2" or "staging")
        confirm_text: String,
    },
    /// Modal: type a confirmation phrase + 3-second countdown
    High {
        /// What the user must type (e.g. "unseal vault-2 prod")
        confirm_text: String,
        /// Warning message shown prominently
        warning: Option<String>,
    },
}

/// State for an active confirmation dialog.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ConfirmDialog {
    /// Unique ID for this dialog
    pub id: String,
    /// Title shown in the dialog header
    pub title: String,
    /// Description of what will happen
    pub description: String,
    /// The tier determines the UX
    pub tier: ConfirmTier,
    /// Current user input (for Medium/High tiers)
    pub input: String,
    /// When the dialog was opened (for High tier countdown)
    pub opened_at: Option<std::time::Instant>,
    /// Whether the action has been confirmed
    pub confirmed: bool,
    /// Whether the dialog was cancelled
    pub cancelled: bool,
}

impl ConfirmDialog {
    #[allow(dead_code)]
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        description: impl Into<String>,
        tier: ConfirmTier,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: description.into(),
            tier,
            input: String::new(),
            opened_at: Some(std::time::Instant::now()),
            confirmed: false,
            cancelled: false,
        }
    }

    /// Check if the confirmation criteria are met
    #[allow(dead_code)]
    pub fn can_confirm(&self) -> bool {
        match &self.tier {
            ConfirmTier::Low => true,
            ConfirmTier::Medium { confirm_text } => self.input.trim() == confirm_text.trim(),
            ConfirmTier::High { confirm_text, .. } => {
                self.input.trim() == confirm_text.trim() && self.countdown_remaining() == 0
            }
        }
    }

    /// Seconds remaining on the High tier countdown (0 if not High or expired)
    #[allow(dead_code)]
    pub fn countdown_remaining(&self) -> u64 {
        match &self.tier {
            ConfirmTier::High { .. } => {
                if let Some(opened) = self.opened_at {
                    let elapsed = opened.elapsed().as_secs();
                    3_u64.saturating_sub(elapsed)
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// Render the confirmation dialog. Returns true if confirmed, false if still open.
    /// Sets `self.cancelled` if the user dismissed it.
    #[allow(dead_code)]
    pub fn show(&mut self, ctx: &egui::Context, mode: ThemeMode) -> bool {
        if self.confirmed || self.cancelled {
            return self.confirmed;
        }

        let mut still_open = true;

        egui::Area::new(egui::Id::new(&self.id).with("confirm_overlay"))
            .fixed_pos(egui::pos2(0.0, 0.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let screen = ui.ctx().screen_rect();
                // Semi-transparent backdrop
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));

                // Click outside to cancel
                let backdrop_response = ui.allocate_rect(screen, egui::Sense::click());
                if backdrop_response.clicked() {
                    self.cancelled = true;
                    still_open = false;
                }
            });

        // Dialog window
        egui::Window::new(&self.title)
            .id(egui::Id::new(&self.id).with("confirm_dialog"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([400.0, 0.0])
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(Colors::bg_elevated(mode))
                    .stroke(egui::Stroke::new(1.0_f32, Colors::border(mode)))
                    .corner_radius(8.0)
                    .inner_margin(20.0),
            )
            .show(ctx, |ui| {
                // Description
                ui.label(
                    egui::RichText::new(&self.description)
                        .size(13.0)
                        .color(Colors::text_secondary(mode)),
                );

                ui.add_space(12.0);

                match &self.tier.clone() {
                    ConfirmTier::Low => {
                        // Simple confirm/cancel buttons
                        ui.horizontal(|ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Confirm").color(Colors::bg_base(mode)),
                                    )
                                    .fill(Colors::RED),
                                )
                                .clicked()
                            {
                                self.confirmed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                self.cancelled = true;
                            }
                        });
                    }

                    ConfirmTier::Medium { confirm_text } => {
                        // Warning / High tier extra message
                        self.show_type_to_confirm(ui, mode, confirm_text, None);
                    }

                    ConfirmTier::High {
                        confirm_text,
                        warning,
                    } => {
                        if let Some(warn) = warning {
                            ui.horizontal(|ui| {
                                ui.label(icons::icon(
                                    icons::TRIANGLE_EXCLAMATION,
                                    14.0,
                                    Colors::ORANGE,
                                ));
                                ui.label(
                                    egui::RichText::new(warn)
                                        .size(12.0)
                                        .strong()
                                        .color(Colors::ORANGE),
                                );
                            });
                            ui.add_space(8.0);
                        }
                        let countdown = self.countdown_remaining();
                        self.show_type_to_confirm(ui, mode, confirm_text, Some(countdown));
                    }
                }
            });

        if !still_open {
            self.cancelled = true;
        }

        // Request repaint during countdown
        if self.countdown_remaining() > 0 {
            ctx.request_repaint();
        }

        self.confirmed
    }

    #[allow(dead_code)]
    fn show_type_to_confirm(
        &mut self,
        ui: &mut egui::Ui,
        mode: ThemeMode,
        confirm_text: &str,
        countdown: Option<u64>,
    ) {
        ui.label(
            egui::RichText::new(format!("Type \"{}\" to confirm:", confirm_text))
                .size(12.0)
                .color(Colors::text_muted(mode)),
        );
        ui.add_space(4.0);

        let input_response = ui.add(
            egui::TextEdit::singleline(&mut self.input)
                .desired_width(ui.available_width())
                .font(egui::TextStyle::Monospace),
        );

        // Auto-focus the input
        if input_response.gained_focus() || self.input.is_empty() {
            input_response.request_focus();
        }

        ui.add_space(12.0);

        ui.horizontal(|ui| {
            let can_confirm = self.can_confirm();
            let button_text = if let Some(secs) = countdown {
                if secs > 0 {
                    format!("Confirm ({}s)", secs)
                } else {
                    "Confirm".to_string()
                }
            } else {
                "Confirm".to_string()
            };

            if ui
                .add_enabled(
                    can_confirm,
                    egui::Button::new(egui::RichText::new(&button_text).color(if can_confirm {
                        Colors::bg_base(mode)
                    } else {
                        Colors::text_muted(mode)
                    }))
                    .fill(if can_confirm {
                        Colors::RED
                    } else {
                        Colors::bg_hover(mode)
                    }),
                )
                .clicked()
            {
                self.confirmed = true;
            }

            if ui.button("Cancel").clicked() {
                self.cancelled = true;
            }
        });
    }
}
