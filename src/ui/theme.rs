use eframe::egui;

// ── Theme Mode ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    /// Detect system preference, fallback to dark
    pub fn from_system() -> Self {
        match dark_light::detect() {
            dark_light::Mode::Light => ThemeMode::Light,
            dark_light::Mode::Dark | dark_light::Mode::Default => ThemeMode::Dark,
        }
    }
}

impl Default for ThemeMode {
    fn default() -> Self {
        Self::from_system()
    }
}

// ── Color Palette (Spotify-inspired) ───────────────────────────────────────
//
// Spotify uses layered backgrounds with almost no borders:
//   Base:     #121212
//   Surface:  #181818
//   Elevated: #282828
//   Hover:    #2a2a2a
//   Highlight:#333333
//
// Accent:    #1DB954 (green)
// Text:      #FFFFFF / #B3B3B3 / #6A6A6A

pub struct Colors;

impl Colors {
    // ── Backgrounds ──
    pub fn bg_base(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(18, 18, 18), // #121212
            ThemeMode::Light => egui::Color32::from_rgb(242, 242, 242),
        }
    }
    pub fn bg_surface(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(24, 24, 24), // #181818
            ThemeMode::Light => egui::Color32::from_rgb(255, 255, 255),
        }
    }
    pub fn bg_elevated(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(40, 40, 40), // #282828
            ThemeMode::Light => egui::Color32::from_rgb(248, 248, 248),
        }
    }
    pub fn bg_hover(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(42, 42, 42), // #2a2a2a
            ThemeMode::Light => egui::Color32::from_rgb(238, 238, 238),
        }
    }
    pub fn bg_highlight(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(51, 51, 51), // #333333
            ThemeMode::Light => egui::Color32::from_rgb(230, 230, 230),
        }
    }
    pub fn bg_sidebar(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(0, 0, 0), // pure black sidebar
            ThemeMode::Light => egui::Color32::from_rgb(250, 250, 250),
        }
    }

    // ── Text ──
    pub fn text_primary(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(255, 255, 255),
            ThemeMode::Light => egui::Color32::from_rgb(20, 20, 20),
        }
    }
    pub fn text_secondary(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(179, 179, 179), // #B3B3B3
            ThemeMode::Light => egui::Color32::from_rgb(80, 80, 80),
        }
    }
    pub fn text_muted(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(106, 106, 106), // #6A6A6A
            ThemeMode::Light => egui::Color32::from_rgb(140, 140, 140),
        }
    }

    // ── Accent ──
    pub fn accent(_m: ThemeMode) -> egui::Color32 {
        egui::Color32::from_rgb(29, 185, 84) // Spotify green #1DB954
    }

    // ── Borders (very subtle, Spotify barely uses them) ──
    pub fn border(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(40, 40, 40),
            ThemeMode::Light => egui::Color32::from_rgb(220, 220, 220),
        }
    }

    // ── Status Colors ──
    pub const GREEN: egui::Color32 = egui::Color32::from_rgb(29, 185, 84); // Spotify green #1DB954
    pub const YELLOW: egui::Color32 = egui::Color32::from_rgb(255, 199, 0);
    pub const RED: egui::Color32 = egui::Color32::from_rgb(232, 65, 65);
    pub const BLUE: egui::Color32 = egui::Color32::from_rgb(58, 140, 235);
    pub const ORANGE: egui::Color32 = egui::Color32::from_rgb(255, 143, 0);
    pub const PURPLE: egui::Color32 = egui::Color32::from_rgb(180, 130, 255);
    pub const GRAY: egui::Color32 = egui::Color32::from_rgb(120, 120, 120);

    // ── Status tinted backgrounds ──
    #[allow(dead_code)]
    pub fn status_bg(color: egui::Color32, m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => {
                egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 25)
            }
            ThemeMode::Light => {
                egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 20)
            }
        }
    }

    // ── Environment Colors (distinct from status red/yellow/green) ──
    pub fn env_third(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(100, 180, 255), // Sky blue
            ThemeMode::Light => egui::Color32::from_rgb(50, 130, 220),
        }
    }
    pub fn env_staging(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(180, 130, 255), // Purple
            ThemeMode::Light => egui::Color32::from_rgb(130, 80, 220),
        }
    }
    pub fn env_prod(m: ThemeMode) -> egui::Color32 {
        match m {
            ThemeMode::Dark => egui::Color32::from_rgb(255, 150, 80), // Warm orange
            ThemeMode::Light => egui::Color32::from_rgb(210, 110, 40),
        }
    }

    // ── Accent ──
    pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(29, 185, 84); // Spotify green
}

/// The configured colour of an environment, lightened in dark mode so it
/// reads on a dark surface; a fallback by ladder position when unset.
pub fn env_color(env: &str, mode: ThemeMode) -> egui::Color32 {
    let ladder = crate::models::environments::current();
    let base = match ladder.color_rgb(env) {
        Some((r, g, b)) => egui::Color32::from_rgb(r, g, b),
        None => match ladder.rank(env) % 3 {
            0 => Colors::env_prod(ThemeMode::Light),
            1 => Colors::env_staging(ThemeMode::Light),
            _ => Colors::env_third(ThemeMode::Light),
        },
    };
    match mode {
        ThemeMode::Light => base,
        ThemeMode::Dark => lighten(base, 0.25),
    }
}

fn lighten(c: egui::Color32, amount: f32) -> egui::Color32 {
    let f = |v: u8| (v as f32 + (255.0 - v as f32) * amount).round() as u8;
    egui::Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// The short label: the configured label, or the id.
pub fn env_label(env: &str) -> String {
    crate::models::environments::current().label(env)
}

pub fn env_label_full(env: &str) -> String {
    env_label(env)
}

// ── Theme Application ──────────────────────────────────────────────────────

pub fn apply_theme(ctx: &egui::Context, mode: ThemeMode) {
    let mut style = (*ctx.style()).clone();

    let mut visuals = match mode {
        ThemeMode::Dark => egui::Visuals::dark(),
        ThemeMode::Light => egui::Visuals::light(),
    };

    visuals.panel_fill = Colors::bg_base(mode);
    visuals.window_fill = Colors::bg_elevated(mode);
    visuals.extreme_bg_color = Colors::bg_surface(mode);
    visuals.faint_bg_color = Colors::bg_surface(mode);
    visuals.code_bg_color = Colors::bg_elevated(mode);

    // Almost no borders — Spotify style. Differentiate with background shading.
    visuals.widgets.noninteractive.bg_fill = Colors::bg_surface(mode);
    visuals.widgets.noninteractive.fg_stroke =
        egui::Stroke::new(1.0_f32, Colors::text_primary(mode));
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.inactive.bg_fill = Colors::bg_elevated(mode);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, Colors::text_secondary(mode));
    visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.hovered.bg_fill = Colors::bg_highlight(mode);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, Colors::text_primary(mode));
    visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.active.bg_fill = Colors::ACCENT;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::WHITE);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(20); // pill shape when active

    visuals.widgets.open.bg_fill = Colors::bg_hover(mode);
    visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0_f32, Colors::text_primary(mode));
    visuals.widgets.open.corner_radius = egui::CornerRadius::same(8);

    visuals.selection.bg_fill = Colors::ACCENT.linear_multiply(0.25);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, Colors::ACCENT);

    visuals.striped = false; // Spotify doesn't stripe — uses hover instead
    visuals.window_corner_radius = egui::CornerRadius::same(12);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: egui::Color32::from_black_alpha(if mode == ThemeMode::Dark { 120 } else { 30 }),
    };

    style.visuals = visuals;

    // Typography — clean, generous sizes
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(26.0));
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, egui::FontId::monospace(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));

    // Generous spacing
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(16.0, 8.0);
    style.spacing.indent = 20.0;
    style.spacing.window_margin = egui::Margin::same(16);

    // Labels should NOT show a text cursor — use default arrow
    style.interaction.selectable_labels = false;

    ctx.set_style(style);
    ctx.set_pixels_per_point(1.25);
}

// ── Reusable UI Components ─────────────────────────────────────────────────

/// Card — Spotify-style elevated surface with no border, rounded corners
pub fn card(ui: &mut egui::Ui, mode: ThemeMode, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::default()
        .fill(Colors::bg_surface(mode))
        .corner_radius(12.0)
        .inner_margin(20.0)
        .outer_margin(egui::Margin::symmetric(0, 4))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add_contents(ui);
        });
}

/// Card with a colored left accent bar
pub fn accent_card(
    ui: &mut egui::Ui,
    mode: ThemeMode,
    accent: egui::Color32,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::default()
        .fill(Colors::bg_surface(mode))
        .corner_radius(12.0)
        .inner_margin(egui::Margin {
            left: 24,
            right: 20,
            top: 16,
            bottom: 16,
        })
        .outer_margin(egui::Margin::symmetric(0, 3))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let rect = ui.min_rect();
            let bar_rect = egui::Rect::from_min_max(
                egui::pos2(rect.left() - 24.0, rect.top() - 16.0),
                egui::pos2(rect.left() - 20.0, rect.bottom() + 16.0),
            );
            ui.painter().rect_filled(
                bar_rect,
                egui::CornerRadius {
                    nw: 12,
                    sw: 12,
                    ne: 0,
                    se: 0,
                },
                accent,
            );
            add_contents(ui);
        });
}

/// Status pill — Spotify-style rounded chip
pub fn status_pill(ui: &mut egui::Ui, label: &str, color: egui::Color32) -> egui::Response {
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_string(), egui::FontId::proportional(11.0), color);
    let text_size = galley.size();
    let pill_size = egui::vec2(text_size.x + 16.0, text_size.y + 6.0);
    let (rect, response) = ui.allocate_exact_size(pill_size, egui::Sense::hover());

    let bg = color.linear_multiply(0.15);
    let radius = pill_size.y / 2.0;
    ui.painter().rect_filled(rect, radius, bg);
    ui.painter()
        .galley(rect.center() - text_size / 2.0, galley, color);

    response
}

/// Section header — muted uppercase label with subtle line
pub fn section_header(ui: &mut egui::Ui, mode: ThemeMode, title: &str) {
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(title.to_uppercase())
                .color(Colors::text_muted(mode))
                .strong()
                .size(11.0),
        );
        ui.add_space(8.0);
        let rect = ui.available_rect_before_wrap();
        let y = rect.center().y;
        ui.painter().line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(0.5_f32, Colors::border(mode)),
        );
    });
    ui.add_space(8.0);
}

/// Big metric number with subtitle
#[allow(dead_code)]
pub fn metric(ui: &mut egui::Ui, mode: ThemeMode, value: &str, label: &str, color: egui::Color32) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(value).size(36.0).color(color).strong());
        ui.label(
            egui::RichText::new(label)
                .size(11.0)
                .color(Colors::text_muted(mode)),
        );
    });
}

/// Spotify-style pill button (rounded, filled on hover)
pub fn pill_button(ui: &mut egui::Ui, mode: ThemeMode, label: &str, active: bool) -> bool {
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
        egui::Button::new(egui::RichText::new(label).color(fg).strong().size(13.0))
            .fill(bg)
            .corner_radius(20.0)
            .min_size(egui::vec2(0.0, 32.0)),
    );
    response.clicked()
}

/// Theme toggle (sun/moon)
pub fn theme_toggle(ui: &mut egui::Ui, mode: &mut ThemeMode) {
    let (icon, tooltip) = match mode {
        ThemeMode::Dark => (super::icons::SUN, "Switch to light mode"),
        ThemeMode::Light => (super::icons::MOON, "Switch to dark mode"),
    };
    if ui
        .add(
            egui::Button::new(
                egui::RichText::new(icon.to_string())
                    .family(egui::FontFamily::Name(super::icons::FA_SOLID.into()))
                    .size(16.0),
            )
            .fill(egui::Color32::TRANSPARENT)
            .corner_radius(20.0),
        )
        .on_hover_text(tooltip)
        .clicked()
    {
        *mode = match mode {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        };
    }
}
