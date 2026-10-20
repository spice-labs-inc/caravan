//! FontAwesome 6 Free icon constants for use with egui.
//!
//! Font files: assets/fonts/fa-solid-900.otf, fa-regular-400.otf
//! License: SIL OFL 1.1 (https://fontawesome.com/license/free)
//!
//! Usage:
//!   ui.label(icons::icon(icons::CIRCLE_CHECK, 16.0, color));
//!   ui.label(icons::icon_text(icons::ROCKET, 14.0, color, " Deploy"));

use eframe::egui;

/// The font family name registered for FontAwesome Solid
pub const FA_SOLID: &str = "fa-solid";
/// The font family name registered for FontAwesome Regular
pub const FA_REGULAR: &str = "fa-regular";

// ── Solid Icons (fa-solid-900) ─────────────────────────────────────────────

// Status
pub const CIRCLE_CHECK: char = '\u{f058}'; // ✓ healthy / success
pub const CIRCLE_XMARK: char = '\u{f057}'; // ✗ failed
pub const TRIANGLE_EXCLAMATION: char = '\u{f071}'; // ⚠ warning / degraded
pub const CIRCLE_EXCLAMATION: char = '\u{f06a}'; // ! error
pub const CIRCLE_INFO: char = '\u{f05a}'; // ℹ info
pub const CIRCLE_QUESTION: char = '\u{f059}'; // ? unknown
pub const CIRCLE_PAUSE: char = '\u{f28b}'; // ⏸ suspended
pub const SPINNER: char = '\u{f110}'; // ↻ progressing
pub const ARROWS_ROTATE: char = '\u{f021}'; // ↻ refresh/sync

// Navigation / Sidebar
pub const CHART_LINE: char = '\u{f201}'; // 📊 Overview
pub const ROCKET: char = '\u{f135}'; // 🚀 Apps
pub const SEEDLING: char = '\u{f4d8}'; // Projects
pub const HELMET_SAFETY: char = '\u{f807}'; // 🏗 Infrastructure
pub const CUBE: char = '\u{f1b2}'; // 📦 Libraries
pub const CODE_BRANCH: char = '\u{f126}'; // 🔀 Pull Requests
pub const GEAR: char = '\u{f013}'; // ⚙ Settings
pub const LOCK: char = '\u{f023}'; // 🔐 Vault
pub const LOCK_OPEN: char = '\u{f3c1}'; // 🔓 Vault unsealed
pub const KEY: char = '\u{f084}'; // 🔑 Unseal key
pub const CAMEL: char = '\u{f8a6}'; // 🐪 Logo (FA6 Pro only — fallback below)

// Actions
pub const ARROW_RIGHT: char = '\u{f061}'; // →
pub const ARROW_UP: char = '\u{f062}'; // ↑ ahead of prod
pub const ARROW_DOWN: char = '\u{f063}'; // ↓ behind upstream env
pub const CHEVRON_LEFT: char = '\u{f053}'; // ◀ back
pub const CHEVRON_RIGHT: char = '\u{f054}'; // ▶ forward
pub const CHEVRON_DOWN: char = '\u{f078}'; // ▼ expanded
pub const CIRCLE_DOT: char = '\u{f192}'; // ● active indicator
pub const MINUS: char = '\u{f068}'; // — not deployed
pub const LINK: char = '\u{f0c1}'; // link
pub const EYE: char = '\u{f06e}'; // visibility
pub const EYE_SLASH: char = '\u{f070}'; // hidden

// External links
pub const ARROW_UP_RIGHT_FROM_SQUARE: char = '\u{f08e}'; // external link

// Media / Controls
pub const PLAY: char = '\u{f04b}'; // ▶ play/start
pub const STOP: char = '\u{f04d}'; // ⏹ stop
pub const SUN: char = '\u{f185}'; // ☀ light mode
pub const MOON: char = '\u{f186}'; // ☽ dark mode
pub const CIRCLE: char = '\u{f111}'; // ● filled circle / dot

// Objects
pub const SERVER: char = '\u{f233}'; // server/pod
pub const TAG: char = '\u{f02b}'; // release tag
pub const CODE_COMMIT: char = '\u{f386}'; // commit
pub const CLOCK: char = '\u{f017}'; // age/time
pub const MAGNIFYING_GLASS: char = '\u{f002}'; // search
pub const COMMENT: char = '\u{f075}'; // comment / review
pub const COPY: char = '\u{f0c5}'; // copy to clipboard
pub const CLIPBOARD_LIST: char = '\u{f46d}'; // issues / task list

// ── Helper Functions ───────────────────────────────────────────────────────

/// Create a RichText with a FontAwesome Solid icon
pub fn icon(codepoint: char, size: f32, color: egui::Color32) -> egui::RichText {
    egui::RichText::new(codepoint.to_string())
        .family(egui::FontFamily::Name(FA_SOLID.into()))
        .size(size)
        .color(color)
}

/// Create a RichText with a FontAwesome Regular icon
pub fn icon_regular(codepoint: char, size: f32, color: egui::Color32) -> egui::RichText {
    egui::RichText::new(codepoint.to_string())
        .family(egui::FontFamily::Name(FA_REGULAR.into()))
        .size(size)
        .color(color)
}

/// Render an icon followed by text, both in the same label.
/// The icon uses FA Solid; the text uses the default proportional font.
/// For mixed-font rendering, call icon() and text separately in a horizontal layout.
pub fn icon_label(
    ui: &mut egui::Ui,
    codepoint: char,
    icon_size: f32,
    icon_color: egui::Color32,
    text: &str,
    text_size: f32,
    text_color: egui::Color32,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(icon(codepoint, icon_size, icon_color));
        ui.label(egui::RichText::new(text).size(text_size).color(text_color));
    });
}

/// Register FontAwesome fonts with egui. Call once during setup.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // FontAwesome Solid
    fonts.font_data.insert(
        FA_SOLID.to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/fa-solid-900.otf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Name(FA_SOLID.into()))
        .or_default()
        .push(FA_SOLID.to_owned());

    // FontAwesome Regular
    fonts.font_data.insert(
        FA_REGULAR.to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/fa-regular-400.otf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Name(FA_REGULAR.into()))
        .or_default()
        .push(FA_REGULAR.to_owned());

    // Also add FA Solid as a fallback for Proportional so FA codepoints
    // work even when no explicit font family is set (graceful degradation).
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push(FA_SOLID.to_owned());

    ctx.set_fonts(fonts);
}
