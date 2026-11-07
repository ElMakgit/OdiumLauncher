//! Themeable Windows visual language for egui.
//!
//! The launcher ships five looks: the classic Windows 98 grey, Aero glass from
//! Windows 7, flat Metro from Windows 8.1 and 10, and the rounded Mica-style
//! Windows 11 surface. All colours live in one [`Palette`] per theme, while the
//! widget helpers below branch on the palette/bevel style so every control is
//! drawn consistently instead of relying on egui defaults.

use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use egui::{
    Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, Response, Sense,
    Stroke, TextStyle, Vec2,
};
pub use pml_config::Theme;

const UI_FONT_SIZE: f32 = 12.0;

/// Builds a translucent Aero colour. [`Color32`] stores premultiplied alpha,
/// while design values are written the intuitive unmultiplied way, so the
/// channels are converted here. Kept `const` so the palette stays a `const`.
const fn aero(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    let alpha = a as u32;
    Color32::from_rgba_premultiplied(
        ((r as u32 * alpha + 127) / 255) as u8,
        ((g as u32 * alpha + 127) / 255) as u8,
        ((b as u32 * alpha + 127) / 255) as u8,
        a,
    )
}

/// How a theme renders raised/sunken edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BevelStyle {
    /// Two-tone 3D bevel of Windows 95/98.
    Classic,
    /// One-pixel flat border of Metro/Windows 10.
    Flat,
    /// One-pixel rounded border of Aero/Windows 11.
    Soft,
}

/// Structural layout family, not just a recolour. It changes how group boxes,
/// title bars and the primary action are composed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutStyle {
    /// Windows 98: grey dialog with etched group boxes.
    Classic,
    /// Windows 7: white rounded cards on a pale blue Aero surface.
    Aero,
    /// Windows 8.1: white Metro surface with solid accent section headers.
    Metro,
    /// Windows 10: white flat cards with an accent stripe on a grey surface.
    Cards,
    /// Windows 11: rounded white cards with titles above them on a Mica surface.
    Fluent,
}

/// Every colour and metric a theme needs. Kept `Copy`-free because it lives in
/// `static` storage; widgets borrow the active palette instead of copying it.
pub struct Palette {
    pub face: Color32,
    pub face_hover: Color32,
    pub face_pressed: Color32,
    pub panel: Color32,
    pub card: Color32,
    pub card_border: Color32,
    pub card_rounding: f32,
    pub card_title: Color32,
    pub card_title_bar: Color32,
    pub input: Color32,
    pub primary: Color32,
    pub primary_hover: Color32,
    pub primary_pressed: Color32,
    pub primary_text: Color32,
    pub layout: LayoutStyle,
    pub button: Color32,
    pub button_hover: Color32,
    pub button_pressed: Color32,
    pub button_border: Color32,
    pub button_rounding: f32,
    pub button_gloss: bool,
    pub bevel_light: Color32,
    pub bevel_inner_light: Color32,
    pub bevel_dark: Color32,
    pub bevel_inner_dark: Color32,
    pub title_top: Color32,
    pub title_bottom: Color32,
    pub title_text: Color32,
    pub title_rounding: f32,
    pub title_height: f32,
    pub title_border: Color32,
    pub text: Color32,
    pub text_disabled: Color32,
    pub selection: Color32,
    pub selection_text: Color32,
    pub row_hover: Color32,
    pub well: Color32,
    pub track: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_pressed: Color32,
    pub progress: Color32,
    pub border: Color32,
    pub error: Color32,
    pub muted: Color32,
    pub rounding: f32,
    pub bevel: BevelStyle,
    pub slider_ticks: bool,
    pub checkbox_rounding: f32,
}

const WIN98: Palette = Palette {
    face: Color32::from_rgb(0xC0, 0xC0, 0xC0),
    face_hover: Color32::from_rgb(0xD2, 0xD2, 0xD2),
    face_pressed: Color32::from_rgb(0xA8, 0xA8, 0xA8),
    panel: Color32::from_rgb(0xC0, 0xC0, 0xC0),
    card: Color32::from_rgb(0xC0, 0xC0, 0xC0),
    card_border: Color32::from_rgb(0x80, 0x80, 0x80),
    card_rounding: 0.0,
    card_title: Color32::from_rgb(0x00, 0x00, 0x00),
    card_title_bar: Color32::TRANSPARENT,
    input: Color32::WHITE,
    primary: Color32::from_rgb(0xC0, 0xC0, 0xC0),
    primary_hover: Color32::from_rgb(0xD2, 0xD2, 0xD2),
    primary_pressed: Color32::from_rgb(0xA8, 0xA8, 0xA8),
    primary_text: Color32::from_rgb(0x00, 0x00, 0x00),
    layout: LayoutStyle::Classic,
    button: Color32::from_rgb(0xC0, 0xC0, 0xC0),
    button_hover: Color32::from_rgb(0xD2, 0xD2, 0xD2),
    button_pressed: Color32::from_rgb(0xA8, 0xA8, 0xA8),
    button_border: Color32::from_rgb(0x80, 0x80, 0x80),
    button_rounding: 0.0,
    button_gloss: false,
    bevel_light: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    bevel_inner_light: Color32::from_rgb(0xDF, 0xDF, 0xDF),
    bevel_dark: Color32::from_rgb(0x80, 0x80, 0x80),
    bevel_inner_dark: Color32::from_rgb(0x00, 0x00, 0x00),
    title_top: Color32::from_rgb(0x00, 0x00, 0x80),
    title_bottom: Color32::from_rgb(0x00, 0x00, 0x80),
    title_text: Color32::WHITE,
    title_rounding: 0.0,
    title_height: 22.0,
    title_border: Color32::from_rgb(0x00, 0x00, 0x00),
    text: Color32::from_rgb(0x00, 0x00, 0x00),
    text_disabled: Color32::from_rgb(0x80, 0x80, 0x80),
    selection: Color32::from_rgb(0x00, 0x00, 0x80),
    selection_text: Color32::WHITE,
    row_hover: Color32::from_rgb(0xE8, 0xE8, 0xE8),
    well: Color32::WHITE,
    track: Color32::from_rgb(0xC0, 0xC0, 0xC0),
    accent: Color32::from_rgb(0x00, 0x00, 0x80),
    accent_hover: Color32::from_rgb(0x00, 0x00, 0xA0),
    accent_pressed: Color32::from_rgb(0x00, 0x00, 0x60),
    progress: Color32::from_rgb(0x00, 0x00, 0x80),
    border: Color32::from_rgb(0x80, 0x80, 0x80),
    error: Color32::from_rgb(0x80, 0x00, 0x00),
    muted: Color32::from_rgb(0x60, 0x60, 0x60),
    rounding: 0.0,
    bevel: BevelStyle::Classic,
    slider_ticks: true,
    checkbox_rounding: 0.0,
};

const WIN7: Palette = Palette {
    // Aero glass: every large surface is translucent so the DWM acrylic
    // backdrop below the framebuffer shows through. Cards keep a higher alpha
    // than the window tint, otherwise dark text over a dark desktop wallpaper
    // would become unreadable.
    face: aero(0xF0, 0xF0, 0xF0, 0xB4),
    face_hover: aero(0xEA, 0xF3, 0xFB, 0xD2),
    face_pressed: aero(0xD5, 0xE8, 0xF7, 0xE0),
    panel: aero(0xEA, 0xF3, 0xFA, 0x99),
    card: aero(0xFF, 0xFF, 0xFF, 0xD9),
    card_border: Color32::from_rgb(0xB7, 0xCF, 0xE3),
    card_rounding: 4.0,
    card_title: Color32::from_rgb(0x14, 0x39, 0x5C),
    card_title_bar: Color32::TRANSPARENT,
    input: aero(0xFF, 0xFF, 0xFF, 0xEB),
    primary: Color32::from_rgb(0x3C, 0x7F, 0xB1),
    primary_hover: Color32::from_rgb(0x55, 0xA4, 0xDC),
    primary_pressed: Color32::from_rgb(0x2C, 0x6A, 0x99),
    primary_text: Color32::WHITE,
    layout: LayoutStyle::Aero,
    button: aero(0xF4, 0xF9, 0xFD, 0xC4),
    button_hover: aero(0xEA, 0xF6, 0xFD, 0xDC),
    button_pressed: aero(0xD2, 0xE9, 0xF9, 0xE6),
    button_border: Color32::from_rgb(0x8C, 0xA0, 0xB3),
    button_rounding: 3.0,
    button_gloss: true,
    bevel_light: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    bevel_inner_light: Color32::from_rgb(0xE8, 0xF1, 0xF8),
    bevel_dark: Color32::from_rgb(0x9A, 0xA9, 0xB8),
    bevel_inner_dark: Color32::from_rgb(0xC7, 0xD5, 0xE2),
    title_top: aero(0xF6, 0xFB, 0xFF, 0xCC),
    title_bottom: aero(0xC9, 0xE7, 0xFA, 0x99),
    title_text: Color32::from_rgb(0x14, 0x39, 0x5C),
    title_rounding: 6.0,
    title_height: 26.0,
    title_border: Color32::from_rgb(0x8C, 0xA0, 0xB3),
    text: Color32::from_rgb(0x10, 0x10, 0x10),
    text_disabled: Color32::from_rgb(0x9A, 0x9A, 0x9A),
    selection: Color32::from_rgb(0x33, 0x99, 0xFF),
    selection_text: Color32::WHITE,
    row_hover: aero(0xEA, 0xF3, 0xFB, 0xB4),
    well: aero(0xFF, 0xFF, 0xFF, 0xEB),
    track: aero(0xC9, 0xD8, 0xE4, 0xD2),
    accent: Color32::from_rgb(0x3C, 0x7F, 0xB1),
    accent_hover: Color32::from_rgb(0x55, 0xA4, 0xDC),
    accent_pressed: Color32::from_rgb(0x2C, 0x6A, 0x99),
    progress: Color32::from_rgb(0x06, 0xB0, 0x25),
    border: Color32::from_rgb(0xA0, 0xA0, 0xA0),
    error: Color32::from_rgb(0xA8, 0x00, 0x00),
    muted: Color32::from_rgb(0x6D, 0x6D, 0x6D),
    rounding: 4.0,
    bevel: BevelStyle::Soft,
    slider_ticks: false,
    checkbox_rounding: 2.0,
};

const WIN81: Palette = Palette {
    face: Color32::WHITE,
    face_hover: Color32::from_rgb(0xE5, 0xF1, 0xFB),
    face_pressed: Color32::from_rgb(0xCC, 0xE4, 0xF7),
    panel: Color32::WHITE,
    card: Color32::WHITE,
    card_border: Color32::from_rgb(0xD2, 0xD2, 0xD2),
    card_rounding: 0.0,
    card_title: Color32::WHITE,
    card_title_bar: Color32::from_rgb(0x26, 0x72, 0xEC),
    input: Color32::WHITE,
    primary: Color32::from_rgb(0x26, 0x72, 0xEC),
    primary_hover: Color32::from_rgb(0x3A, 0x86, 0xF0),
    primary_pressed: Color32::from_rgb(0x1E, 0x5B, 0xB8),
    primary_text: Color32::WHITE,
    layout: LayoutStyle::Metro,
    button: Color32::from_rgb(0xE1, 0xE1, 0xE1),
    button_hover: Color32::from_rgb(0xE5, 0xF1, 0xFB),
    button_pressed: Color32::from_rgb(0xCC, 0xE4, 0xF7),
    button_border: Color32::from_rgb(0xAC, 0xAC, 0xAC),
    button_rounding: 0.0,
    button_gloss: false,
    bevel_light: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    bevel_inner_light: Color32::from_rgb(0xDF, 0xDF, 0xDF),
    bevel_dark: Color32::from_rgb(0xAC, 0xAC, 0xAC),
    bevel_inner_dark: Color32::from_rgb(0x70, 0x70, 0x70),
    title_top: Color32::from_rgb(0x26, 0x72, 0xEC),
    title_bottom: Color32::from_rgb(0x26, 0x72, 0xEC),
    title_text: Color32::WHITE,
    title_rounding: 0.0,
    title_height: 24.0,
    title_border: Color32::from_rgb(0x1E, 0x5B, 0xB8),
    text: Color32::from_rgb(0x00, 0x00, 0x00),
    text_disabled: Color32::from_rgb(0x7A, 0x7A, 0x7A),
    selection: Color32::from_rgb(0x26, 0x72, 0xEC),
    selection_text: Color32::WHITE,
    row_hover: Color32::from_rgb(0xE5, 0xF1, 0xFB),
    well: Color32::WHITE,
    track: Color32::from_rgb(0xD6, 0xD6, 0xD6),
    accent: Color32::from_rgb(0x26, 0x72, 0xEC),
    accent_hover: Color32::from_rgb(0x3A, 0x86, 0xF0),
    accent_pressed: Color32::from_rgb(0x1E, 0x5B, 0xB8),
    progress: Color32::from_rgb(0x26, 0x72, 0xEC),
    border: Color32::from_rgb(0xAC, 0xAC, 0xAC),
    error: Color32::from_rgb(0xC4, 0x2B, 0x1C),
    muted: Color32::from_rgb(0x6B, 0x6B, 0x6B),
    rounding: 0.0,
    bevel: BevelStyle::Flat,
    slider_ticks: false,
    checkbox_rounding: 0.0,
};

const WIN10: Palette = Palette {
    face: Color32::from_rgb(0xF3, 0xF3, 0xF3),
    face_hover: Color32::from_rgb(0xE9, 0xE9, 0xE9),
    face_pressed: Color32::from_rgb(0xDE, 0xDE, 0xDE),
    panel: Color32::from_rgb(0xF0, 0xF0, 0xF0),
    card: Color32::WHITE,
    card_border: Color32::from_rgb(0xDA, 0xDA, 0xDA),
    card_rounding: 0.0,
    card_title: Color32::from_rgb(0x1A, 0x1A, 0x1A),
    card_title_bar: Color32::TRANSPARENT,
    input: Color32::WHITE,
    primary: Color32::from_rgb(0x00, 0x78, 0xD7),
    primary_hover: Color32::from_rgb(0x10, 0x6E, 0xBE),
    primary_pressed: Color32::from_rgb(0x00, 0x5A, 0x9E),
    primary_text: Color32::WHITE,
    layout: LayoutStyle::Cards,
    button: Color32::from_rgb(0xE1, 0xE1, 0xE1),
    button_hover: Color32::from_rgb(0xE5, 0xF1, 0xFB),
    button_pressed: Color32::from_rgb(0xCC, 0xE4, 0xF7),
    button_border: Color32::from_rgb(0xAD, 0xAD, 0xAD),
    button_rounding: 0.0,
    button_gloss: false,
    bevel_light: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    bevel_inner_light: Color32::from_rgb(0xE0, 0xE0, 0xE0),
    bevel_dark: Color32::from_rgb(0xAD, 0xAD, 0xAD),
    bevel_inner_dark: Color32::from_rgb(0x70, 0x70, 0x70),
    title_top: Color32::from_rgb(0x00, 0x78, 0xD7),
    title_bottom: Color32::from_rgb(0x00, 0x78, 0xD7),
    title_text: Color32::WHITE,
    title_rounding: 0.0,
    title_height: 26.0,
    title_border: Color32::from_rgb(0x00, 0x5A, 0x9E),
    text: Color32::from_rgb(0x00, 0x00, 0x00),
    text_disabled: Color32::from_rgb(0x80, 0x80, 0x80),
    selection: Color32::from_rgb(0x00, 0x78, 0xD7),
    selection_text: Color32::WHITE,
    row_hover: Color32::from_rgb(0xE5, 0xF1, 0xFB),
    well: Color32::WHITE,
    track: Color32::from_rgb(0xD6, 0xD6, 0xD6),
    accent: Color32::from_rgb(0x00, 0x78, 0xD7),
    accent_hover: Color32::from_rgb(0x10, 0x6E, 0xBE),
    accent_pressed: Color32::from_rgb(0x00, 0x5A, 0x9E),
    progress: Color32::from_rgb(0x00, 0x78, 0xD7),
    border: Color32::from_rgb(0xC8, 0xC8, 0xC8),
    error: Color32::from_rgb(0xC4, 0x2B, 0x1C),
    muted: Color32::from_rgb(0x6B, 0x6B, 0x6B),
    rounding: 0.0,
    bevel: BevelStyle::Flat,
    slider_ticks: false,
    checkbox_rounding: 0.0,
};

const WIN11: Palette = Palette {
    face: Color32::from_rgb(0xF3, 0xF3, 0xF3),
    face_hover: Color32::from_rgb(0xEB, 0xEB, 0xEB),
    face_pressed: Color32::from_rgb(0xE0, 0xE0, 0xE0),
    panel: Color32::from_rgb(0xF1, 0xF5, 0xF9),
    card: Color32::WHITE,
    card_border: Color32::from_rgb(0xE1, 0xE6, 0xEB),
    card_rounding: 8.0,
    card_title: Color32::from_rgb(0x1A, 0x1A, 0x1A),
    card_title_bar: Color32::TRANSPARENT,
    input: Color32::WHITE,
    primary: Color32::from_rgb(0x00, 0x67, 0xC0),
    primary_hover: Color32::from_rgb(0x00, 0x78, 0xD4),
    primary_pressed: Color32::from_rgb(0x00, 0x5A, 0x9E),
    primary_text: Color32::WHITE,
    layout: LayoutStyle::Fluent,
    button: Color32::from_rgb(0xFD, 0xFD, 0xFD),
    button_hover: Color32::from_rgb(0xF5, 0xF5, 0xF5),
    button_pressed: Color32::from_rgb(0xE8, 0xE8, 0xE8),
    button_border: Color32::from_rgb(0xD1, 0xD1, 0xD1),
    button_rounding: 4.0,
    button_gloss: false,
    bevel_light: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    bevel_inner_light: Color32::from_rgb(0xE6, 0xE6, 0xE6),
    bevel_dark: Color32::from_rgb(0xD1, 0xD1, 0xD1),
    bevel_inner_dark: Color32::from_rgb(0xA0, 0xA0, 0xA0),
    title_top: Color32::from_rgb(0xF8, 0xFA, 0xFC),
    title_bottom: Color32::from_rgb(0xED, 0xF2, 0xF7),
    title_text: Color32::from_rgb(0x1A, 0x1A, 0x1A),
    title_rounding: 8.0,
    title_height: 30.0,
    title_border: Color32::from_rgb(0xDF, 0xE5, 0xEB),
    text: Color32::from_rgb(0x1A, 0x1A, 0x1A),
    text_disabled: Color32::from_rgb(0x9A, 0x9A, 0x9A),
    selection: Color32::from_rgb(0x00, 0x67, 0xC0),
    selection_text: Color32::WHITE,
    row_hover: Color32::from_rgb(0xEB, 0xEB, 0xEB),
    well: Color32::WHITE,
    track: Color32::from_rgb(0xD6, 0xD6, 0xD6),
    accent: Color32::from_rgb(0x00, 0x67, 0xC0),
    accent_hover: Color32::from_rgb(0x00, 0x78, 0xD4),
    accent_pressed: Color32::from_rgb(0x00, 0x5A, 0x9E),
    progress: Color32::from_rgb(0x00, 0x67, 0xC0),
    border: Color32::from_rgb(0xE0, 0xE0, 0xE0),
    error: Color32::from_rgb(0xC4, 0x2B, 0x1C),
    muted: Color32::from_rgb(0x6B, 0x6B, 0x6B),
    rounding: 8.0,
    bevel: BevelStyle::Soft,
    slider_ticks: false,
    checkbox_rounding: 3.0,
};

static CURRENT_THEME: AtomicU8 = AtomicU8::new(0);

/// Display scale (physical pixels per logical point) as reported by egui.
/// Custom-painted text uses it to snap glyph sizes to whole pixels, which is
/// what keeps labels sharp on 4K monitors running at 125-175% scaling.
static DPI_SCALE: AtomicU32 = AtomicU32::new(0);

fn theme_code(theme: Theme) -> u8 {
    match theme {
        Theme::Win98 => 0,
        Theme::Win7 => 1,
        Theme::Win81 => 2,
        Theme::Win10 => 3,
        Theme::Win11 => 4,
    }
}

fn theme_from_code(code: u8) -> Theme {
    match code {
        1 => Theme::Win7,
        2 => Theme::Win81,
        3 => Theme::Win10,
        4 => Theme::Win11,
        _ => Theme::Win98,
    }
}

pub fn current_theme() -> Theme {
    theme_from_code(CURRENT_THEME.load(Ordering::Relaxed))
}

/// Rounds a point size so the glyphs rasterise at a whole number of physical
/// pixels. At 150% a 12.5 pt font covers 18.75 px, which the rasteriser spreads
/// over 19 px and the extra sample softens every stem; 19/1.5 = 12.67 pt keeps
/// the glyph grid aligned and looks crisp instead.
pub fn snap_font_size(size: f32, pixels_per_point: f32) -> f32 {
    if !(pixels_per_point.is_finite() && pixels_per_point > 0.0) {
        return size;
    }
    ((size * pixels_per_point).round().max(1.0)) / pixels_per_point
}

/// Display scale captured by the last [`apply`]/[`sync_scale`] call.
pub fn dpi_scale() -> f32 {
    let bits = DPI_SCALE.load(Ordering::Relaxed);
    if bits == 0 {
        1.0
    } else {
        f32::from_bits(bits)
    }
}

/// Proportional font whose size is snapped to the physical pixel grid.
pub fn font(size: f32) -> FontId {
    FontId::proportional(snap_font_size(size, dpi_scale()))
}

/// Monospace font whose size is snapped to the physical pixel grid.
pub fn mono_font(size: f32) -> FontId {
    FontId::monospace(snap_font_size(size, dpi_scale()))
}

pub fn set_theme(theme: Theme) {
    CURRENT_THEME.store(theme_code(theme), Ordering::Relaxed);
}

pub fn palette(theme: Theme) -> &'static Palette {
    match theme {
        Theme::Win98 => &WIN98,
        Theme::Win7 => &WIN7,
        Theme::Win81 => &WIN81,
        Theme::Win10 => &WIN10,
        Theme::Win11 => &WIN11,
    }
}

pub fn palette_now() -> &'static Palette {
    palette(current_theme())
}

/// Height reserved by the main window's custom title strip.
pub fn title_bar_height() -> f32 {
    palette_now().title_height + 2.0
}

pub fn face() -> Color32 {
    palette_now().face
}

pub fn panel_color() -> Color32 {
    palette_now().panel
}

pub fn row_rounding() -> f32 {
    palette_now().card_rounding.clamp(0.0, 6.0)
}

pub fn well_rounding() -> f32 {
    let p = palette_now();
    if p.bevel == BevelStyle::Soft {
        p.rounding.min(6.0)
    } else {
        0.0
    }
}

pub fn text_color() -> Color32 {
    palette_now().text
}

pub fn well_color() -> Color32 {
    palette_now().well
}

pub fn selection_color() -> Color32 {
    palette_now().selection
}

pub fn selection_text_color() -> Color32 {
    palette_now().selection_text
}

pub fn row_hover_color() -> Color32 {
    palette_now().row_hover
}

pub fn error_color() -> Color32 {
    palette_now().error
}

pub fn muted_color() -> Color32 {
    palette_now().muted
}

pub fn border_color() -> Color32 {
    palette_now().border
}

/// Applies the global egui style for the chosen theme. Call once at startup
/// and again whenever the player picks another theme.
pub fn apply(ctx: &egui::Context, theme: Theme) {
    set_theme(theme);
    install_system_fonts(ctx, theme);
    apply_style(ctx, theme);
}

/// Rebuilds the style when the window moved to a monitor with a different
/// DPI scale. Font sizes are snapped to physical pixels, so they have to be
/// recomputed for the new scale; without this a 4K laptop screen and a 1080p
/// external display would share sizes tuned for only one of them.
pub fn sync_scale(ctx: &egui::Context) {
    let ppp = ctx.pixels_per_point();
    if !(ppp.is_finite() && ppp > 0.0) {
        return;
    }
    let previous = dpi_scale();
    if (previous - ppp).abs() > 0.01 {
        apply_style(ctx, current_theme());
        ctx.request_repaint();
    }
}

fn apply_style(ctx: &egui::Context, theme: Theme) {
    let ppp = ctx.pixels_per_point();
    DPI_SCALE.store(ppp.to_bits(), Ordering::Relaxed);

    let p = palette(theme);
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, font(15.0)),
        (TextStyle::Body, font(UI_FONT_SIZE)),
        (TextStyle::Monospace, mono_font(11.0)),
        (TextStyle::Button, font(UI_FONT_SIZE)),
        (TextStyle::Small, font(10.0)),
    ]
    .into();

    style.spacing.item_spacing = Vec2::new(4.0, 4.0);
    style.spacing.button_padding = if p.bevel == BevelStyle::Classic {
        Vec2::new(8.0, 3.0)
    } else {
        Vec2::new(10.0, 4.0)
    };
    style.spacing.interact_size = Vec2::new(
        72.0,
        if p.bevel == BevelStyle::Classic {
            22.0
        } else {
            24.0
        },
    );
    style.spacing.indent = 14.0;
    style.spacing.slider_width = 120.0;
    style.spacing.combo_width = 220.0;

    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = p.face;
    visuals.window_fill = if p.layout == LayoutStyle::Classic {
        p.face
    } else {
        p.card
    };
    visuals.extreme_bg_color = p.input;
    visuals.faint_bg_color = p.face_hover;
    visuals.code_bg_color = p.input;
    visuals.override_text_color = Some(p.text);
    visuals.window_rounding = egui::Rounding::same(p.rounding);
    visuals.menu_rounding = egui::Rounding::same(p.rounding.min(6.0));
    visuals.window_stroke = Stroke::new(1.0_f32, p.border);
    visuals.window_shadow = window_shadow(theme);
    visuals.popup_shadow = window_shadow(theme);
    visuals.selection.bg_fill = p.selection;
    visuals.selection.stroke = Stroke::new(1.0_f32, p.selection_text);
    visuals.hyperlink_color = p.accent;

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = p.face;
    widgets.noninteractive.weak_bg_fill = p.face;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    widgets.noninteractive.rounding = egui::Rounding::same(p.rounding);

    widgets.inactive.bg_fill = p.button;
    widgets.inactive.weak_bg_fill = p.button;
    widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.button_border);
    widgets.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    widgets.inactive.rounding = egui::Rounding::same(p.button_rounding);

    widgets.hovered.bg_fill = p.button_hover;
    widgets.hovered.weak_bg_fill = p.button_hover;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent);
    widgets.hovered.fg_stroke = Stroke::new(1.0_f32, p.text);
    widgets.hovered.rounding = egui::Rounding::same(p.button_rounding);

    widgets.active.bg_fill = p.button_pressed;
    widgets.active.weak_bg_fill = p.button_pressed;
    widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent_pressed);
    widgets.active.fg_stroke = Stroke::new(1.0_f32, p.text);
    widgets.active.rounding = egui::Rounding::same(p.button_rounding);

    widgets.open.bg_fill = p.button_hover;
    widgets.open.weak_bg_fill = p.button_hover;
    widgets.open.bg_stroke = Stroke::new(1.0_f32, p.button_border);
    widgets.open.fg_stroke = Stroke::new(1.0_f32, p.text);
    widgets.open.rounding = egui::Rounding::same(p.button_rounding);

    style.visuals = visuals;
    ctx.set_style(style);
}

fn window_shadow(theme: Theme) -> egui::epaint::Shadow {
    match theme {
        Theme::Win98 => egui::epaint::Shadow::NONE,
        Theme::Win7 => egui::epaint::Shadow {
            offset: Vec2::new(0.0, 3.0),
            blur: 10.0,
            spread: 0.0,
            color: Color32::from_black_alpha(45),
        },
        Theme::Win81 | Theme::Win10 => egui::epaint::Shadow {
            offset: Vec2::new(0.0, 2.0),
            blur: 6.0,
            spread: 0.0,
            color: Color32::from_black_alpha(40),
        },
        Theme::Win11 => egui::epaint::Shadow {
            offset: Vec2::new(0.0, 6.0),
            blur: 18.0,
            spread: 0.0,
            color: Color32::from_black_alpha(50),
        },
    }
}

fn install_system_fonts(ctx: &egui::Context, theme: Theme) {
    let mut fonts = FontDefinitions::default();

    // Windows 98 keeps Microsoft Sans Serif for its bitmap-font silhouette.
    // Modern themes use Segoe UI; Windows 11 prefers Segoe UI Variable when the
    // font is present, falling back to the classic Segoe UI otherwise.
    let ui_path = match theme {
        Theme::Win98 => "C:\\Windows\\Fonts\\micross.ttf",
        Theme::Win11 if Path::new("C:\\Windows\\Fonts\\SegUIVar.ttf").exists() => {
            "C:\\Windows\\Fonts\\SegUIVar.ttf"
        }
        _ => "C:\\Windows\\Fonts\\segoeui.ttf",
    };
    let ui_name = match theme {
        Theme::Win98 => "win98-ui",
        _ => "theme-ui",
    };
    if let Some(bytes) = read_font(ui_path) {
        fonts
            .font_data
            .insert(ui_name.to_owned(), FontData::from_owned(bytes));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, ui_name.to_owned());
    }

    if let Some(bytes) = read_font("C:\\Windows\\Fonts\\consola.ttf") {
        fonts
            .font_data
            .insert("theme-mono".to_owned(), FontData::from_owned(bytes));
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, "theme-mono".to_owned());
    }

    ctx.set_fonts(fonts);
}

fn read_font(path: &str) -> Option<Vec<u8>> {
    match std::fs::read(Path::new(path)) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            tracing::debug!(path, %error, "system font unavailable, using egui fallback");
            None
        }
    }
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let channel = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(
        channel(a.r(), b.r()),
        channel(a.g(), b.g()),
        channel(a.b(), b.b()),
    )
}

fn lighten(color: Color32, amount: f32) -> Color32 {
    mix(color, Color32::WHITE, amount)
}

/// Vertical gradient drawn as thin strips. `rounding` is only applied to the
/// first and last strip, which keeps rounded title bars smooth.
fn gradient_rect(
    painter: &egui::Painter,
    rect: Rect,
    top: Color32,
    bottom: Color32,
    rounding: egui::Rounding,
) {
    const STEPS: usize = 14;
    for index in 0..STEPS {
        let t0 = index as f32 / STEPS as f32;
        let t1 = (index + 1) as f32 / STEPS as f32;
        let strip = Rect::from_min_max(
            Pos2::new(rect.left(), rect.top() + rect.height() * t0),
            Pos2::new(rect.right(), rect.top() + rect.height() * t1),
        );
        let mut corner = egui::Rounding::ZERO;
        if index == 0 {
            corner.nw = rounding.nw;
            corner.ne = rounding.ne;
        }
        if index + 1 == STEPS {
            corner.sw = rounding.sw;
            corner.se = rounding.se;
        }
        painter.rect_filled(strip, corner, mix(top, bottom, (t0 + t1) * 0.5));
    }
}

/// Draws a classic 2-pixel raised/sunken bevel, a flat Metro border, or a soft
/// rounded border depending on the active theme.
pub fn paint_bevel(painter: &egui::Painter, rect: Rect, raised: bool) {
    let p = palette_now();
    match p.bevel {
        BevelStyle::Classic => {
            let (top_left_outer, bottom_right_outer, top_left_inner, bottom_right_inner) = if raised
            {
                (
                    p.bevel_light,
                    p.bevel_dark,
                    p.bevel_inner_light,
                    p.bevel_inner_dark,
                )
            } else {
                (
                    p.bevel_dark,
                    p.bevel_light,
                    p.bevel_inner_dark,
                    p.bevel_inner_light,
                )
            };

            let r = rect;
            painter.line_segment(
                [r.left_top(), r.right_top()],
                Stroke::new(1.0_f32, top_left_outer),
            );
            painter.line_segment(
                [r.left_top(), r.left_bottom()],
                Stroke::new(1.0_f32, top_left_outer),
            );
            painter.line_segment(
                [r.left_bottom(), r.right_bottom()],
                Stroke::new(1.0_f32, bottom_right_outer),
            );
            painter.line_segment(
                [r.right_top(), r.right_bottom()],
                Stroke::new(1.0_f32, bottom_right_outer),
            );

            let inner = r.shrink(1.0);
            painter.line_segment(
                [inner.left_top(), inner.right_top()],
                Stroke::new(1.0_f32, top_left_inner),
            );
            painter.line_segment(
                [inner.left_top(), inner.left_bottom()],
                Stroke::new(1.0_f32, top_left_inner),
            );
            painter.line_segment(
                [inner.left_bottom(), inner.right_bottom()],
                Stroke::new(1.0_f32, bottom_right_inner),
            );
            painter.line_segment(
                [inner.right_top(), inner.right_bottom()],
                Stroke::new(1.0_f32, bottom_right_inner),
            );
        }
        BevelStyle::Flat => {
            let color = if raised { p.button_border } else { p.border };
            painter.rect_stroke(rect, egui::Rounding::ZERO, Stroke::new(1.0_f32, color));
        }
        BevelStyle::Soft => {
            let rounding = if raised {
                p.button_rounding
            } else {
                p.rounding.min(6.0)
            };
            let color = if raised { p.button_border } else { p.border };
            painter.rect_stroke(
                rect,
                egui::Rounding::same(rounding),
                Stroke::new(1.0_f32, color),
            );
        }
    }
}

/// A filled, beveled container. `raised = false` gives a sunken well suitable
/// for logs, list boxes and text areas.
pub fn bevel_frame<R>(
    ui: &mut egui::Ui,
    raised: bool,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let p = palette_now();
    let fill = if raised { p.face } else { p.well };
    let frame = egui::Frame::none()
        .fill(fill)
        .inner_margin(egui::Margin::same(5.0));
    let inner = frame.show(ui, add_contents);
    paint_bevel(ui.painter(), inner.response.rect, raised);
    inner
}

/// Section container. Every layout family composes it differently:
/// etched 98 box, Aero card, Metro accent header, Windows 10 accent card or
/// a rounded Windows 11 card with the title above it.
pub fn group_box<R>(
    ui: &mut egui::Ui,
    title: &str,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let theme = current_theme();
    let p = palette(theme);

    if p.layout == LayoutStyle::Cards {
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(title)
                .strong()
                .size(12.0)
                .color(p.card_title),
        );
        ui.add_space(3.0);
        return ui
            .scope(|ui| {
                let card = egui::Frame::none()
                    .fill(p.card)
                    .inner_margin(egui::Margin {
                        left: 14.0,
                        right: 12.0,
                        top: 9.0,
                        bottom: 9.0,
                    })
                    .show(ui, add_contents);
                let rect = card.response.rect;
                let painter = ui.painter();
                painter.rect_stroke(
                    rect,
                    egui::Rounding::ZERO,
                    Stroke::new(1.0_f32, p.card_border),
                );
                painter.rect_filled(
                    Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())),
                    egui::Rounding::ZERO,
                    p.accent,
                );
                card
            })
            .inner;
    }

    if p.layout == LayoutStyle::Fluent {
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(title)
                .strong()
                .size(12.5)
                .color(p.card_title),
        );
        ui.add_space(2.0);
        let frame = egui::Frame::none()
            .fill(p.card)
            .rounding(egui::Rounding::same(p.card_rounding))
            .stroke(Stroke::new(1.0_f32, p.card_border))
            .inner_margin(egui::Margin {
                left: 12.0,
                right: 12.0,
                top: 8.0,
                bottom: 8.0,
            });
        return frame.show(ui, add_contents);
    }

    let (left, right, top, bottom) = match p.layout {
        LayoutStyle::Aero => (10.0, 10.0, 12.0, 10.0),
        LayoutStyle::Metro => (10.0, 10.0, 28.0, 10.0),
        _ => (8.0, 8.0, 10.0, 8.0),
    };
    let inner = ui
        .scope(|ui| {
            ui.add_space(3.0);
            let frame = egui::Frame::none().fill(p.card).inner_margin(egui::Margin {
                left,
                right,
                top,
                bottom,
            });
            frame.show(ui, add_contents)
        })
        .inner;

    let rect = inner.response.rect;
    let painter = ui.painter();
    match p.layout {
        LayoutStyle::Classic => {
            painter.line_segment(
                [rect.left_top(), rect.right_top()],
                Stroke::new(1.0_f32, p.bevel_dark),
            );
            painter.line_segment(
                [rect.left_top(), rect.left_bottom()],
                Stroke::new(1.0_f32, p.bevel_dark),
            );
            painter.line_segment(
                [rect.left_bottom(), rect.right_bottom()],
                Stroke::new(1.0_f32, p.bevel_light),
            );
            painter.line_segment(
                [rect.right_top(), rect.right_bottom()],
                Stroke::new(1.0_f32, p.bevel_light),
            );
        }
        LayoutStyle::Aero => {
            painter.rect_stroke(
                rect,
                egui::Rounding::same(p.card_rounding),
                Stroke::new(1.0_f32, p.card_border),
            );
        }
        LayoutStyle::Metro => {
            painter.rect_stroke(
                rect,
                egui::Rounding::ZERO,
                Stroke::new(1.0_f32, p.card_border),
            );
            let header = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 24.0));
            painter.rect_filled(header, egui::Rounding::ZERO, p.card_title_bar);
            painter.text(
                header.left_center() + Vec2::new(10.0, 0.0),
                Align2::LEFT_CENTER,
                title,
                font(12.5),
                p.card_title,
            );
        }
        _ => {}
    }

    if matches!(p.layout, LayoutStyle::Classic | LayoutStyle::Aero) {
        let title_pos = rect.left_top() + Vec2::new(10.0, -7.0);
        let title_galley = painter.layout_no_wrap(title.to_owned(), font(11.5), p.card_title);
        let title_rect = Rect::from_min_size(
            title_pos - Vec2::new(2.0, 0.0),
            title_galley.size() + Vec2::new(4.0, 1.0),
        );
        painter.rect_filled(title_rect, egui::Rounding::ZERO, p.panel);
        painter.galley(title_pos, title_galley, p.card_title);
    }
    inner
}

/// Theme-aware caption strip used for the main window and internal panels.
pub fn title_bar(ui: &mut egui::Ui, title: &str) {
    title_bar_with_icon(ui, title, None);
}

/// Caption strip that shows the launcher mascot instead of the generic "O"
/// badge. The icon is optional so internal panels and previews keep working
/// without a texture.
pub fn title_bar_with_icon(ui: &mut egui::Ui, title: &str, texture: Option<&egui::TextureHandle>) {
    let theme = current_theme();
    let p = palette(theme);
    let width = ui.available_width().max(32.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, p.title_height), Sense::hover());
    let painter = ui.painter();

    match theme {
        Theme::Win98 => {
            painter.rect_filled(rect, egui::Rounding::ZERO, p.title_bottom);
            painter.line_segment(
                [rect.left_top(), rect.right_top()],
                Stroke::new(1.0_f32, p.bevel_light),
            );
            painter.line_segment(
                [rect.left_top(), rect.left_bottom()],
                Stroke::new(1.0_f32, p.bevel_light),
            );
            painter.line_segment(
                [rect.left_bottom(), rect.right_bottom()],
                Stroke::new(1.0_f32, p.bevel_dark),
            );
            painter.line_segment(
                [rect.right_top(), rect.right_bottom()],
                Stroke::new(1.0_f32, p.bevel_dark),
            );
            let icon_rect = Rect::from_center_size(
                Pos2::new(rect.left() + 12.0, rect.center().y),
                Vec2::splat(16.0),
            );
            let icon_drawn = paint_title_icon(painter, icon_rect, texture);
            painter.text(
                rect.left_center() + Vec2::new(if icon_drawn { 24.0 } else { 6.0 }, 0.0),
                Align2::LEFT_CENTER,
                title,
                font(12.0),
                p.title_text,
            );
        }
        Theme::Win7 => {
            let rounding = egui::Rounding {
                nw: p.title_rounding,
                ne: p.title_rounding,
                sw: 0.0,
                se: 0.0,
            };
            gradient_rect(painter, rect, p.title_top, p.title_bottom, rounding);
            painter.line_segment(
                [
                    rect.left_top() + Vec2::new(1.0, 1.0),
                    rect.right_top() + Vec2::new(-1.0, 1.0),
                ],
                Stroke::new(1.0_f32, Color32::from_white_alpha(190)),
            );
            painter.rect_stroke(rect, rounding, Stroke::new(1.0_f32, p.title_border));
            let icon_rect = Rect::from_center_size(
                Pos2::new(rect.left() + 19.0, rect.center().y),
                Vec2::splat(20.0),
            );
            let icon_drawn = paint_title_icon(painter, icon_rect, texture);
            let pos = rect.left_center() + Vec2::new(if icon_drawn { 34.0 } else { 8.0 }, 0.0);
            painter.text(
                pos + Vec2::new(0.7, 0.7),
                Align2::LEFT_CENTER,
                title,
                font(12.5),
                Color32::from_white_alpha(180),
            );
            painter.text(pos, Align2::LEFT_CENTER, title, font(12.5), p.title_text);
        }
        Theme::Win81 => {
            painter.rect_filled(rect, egui::Rounding::ZERO, p.title_bottom);
            painter.line_segment(
                [rect.left_top(), rect.right_top()],
                Stroke::new(1.0_f32, lighten(p.title_bottom, 0.18)),
            );
            let icon_rect = Rect::from_center_size(
                Pos2::new(rect.left() + 14.0, rect.center().y),
                Vec2::splat(16.0),
            );
            let icon_drawn = paint_title_icon(painter, icon_rect, texture);
            painter.text(
                rect.left_center() + Vec2::new(if icon_drawn { 28.0 } else { 8.0 }, 0.0),
                Align2::LEFT_CENTER,
                title,
                font(12.0),
                p.title_text,
            );
        }
        Theme::Win10 => {
            painter.rect_filled(rect, egui::Rounding::ZERO, p.title_bottom);
            painter.line_segment(
                [rect.left_top(), rect.right_top()],
                Stroke::new(1.0_f32, lighten(p.title_bottom, 0.18)),
            );
            let icon = Rect::from_center_size(
                Pos2::new(rect.left() + 16.0, rect.center().y),
                Vec2::splat(18.0),
            );
            if !paint_title_icon(painter, icon, texture) {
                painter.rect_filled(icon, egui::Rounding::same(2.0), Color32::WHITE);
                painter.text(
                    icon.center(),
                    Align2::CENTER_CENTER,
                    "O",
                    font(11.5),
                    p.title_bottom,
                );
            }
            painter.text(
                rect.left_center() + Vec2::new(32.0, 0.0),
                Align2::LEFT_CENTER,
                title,
                font(12.5),
                p.title_text,
            );
        }
        Theme::Win11 => {
            let rounding = egui::Rounding {
                nw: p.title_rounding,
                ne: p.title_rounding,
                sw: 0.0,
                se: 0.0,
            };
            gradient_rect(painter, rect, p.title_top, p.title_bottom, rounding);
            painter.line_segment(
                [
                    rect.left_bottom() - Vec2::new(0.0, 0.5),
                    rect.right_bottom() - Vec2::new(0.0, 0.5),
                ],
                Stroke::new(1.0_f32, p.title_border),
            );
            let icon = Rect::from_center_size(
                Pos2::new(rect.left() + 17.0, rect.center().y),
                Vec2::splat(18.0),
            );
            if !paint_title_icon(painter, icon, texture) {
                painter.rect_filled(icon, egui::Rounding::same(5.0), p.accent);
                painter.text(
                    icon.center(),
                    Align2::CENTER_CENTER,
                    "O",
                    font(11.5),
                    Color32::WHITE,
                );
            }
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                title,
                font(12.5),
                p.title_text,
            );
        }
    }
}

/// Draws the mascot texture into `rect`. Returns `false` when no icon was
/// supplied so the caller can fall back to the letter badge.
fn paint_title_icon(
    painter: &egui::Painter,
    rect: Rect,
    icon: Option<&egui::TextureHandle>,
) -> bool {
    let Some(texture) = icon else {
        return false;
    };
    painter.image(
        texture.id(),
        rect,
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
    true
}

/// Trackbar/slider. Classic Windows 98 draws ticks and a raised thumb; modern
/// themes draw a rounded accent track with a circular thumb.
pub fn slider_u32(
    ui: &mut egui::Ui,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    width: f32,
) -> Response {
    let theme = current_theme();
    let p = palette(theme);
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(width.max(80.0), 22.0), Sense::click_and_drag());
    let min = *range.start();
    let max = (*range.end()).max(min);
    let mut changed = false;

    if (response.dragged() || response.clicked()) && max > min {
        if let Some(pointer) = ui.input(|input| input.pointer.interact_pos()) {
            let track_left = rect.left() + 8.0;
            let track_width = (rect.width() - 16.0).max(1.0);
            let fraction = ((pointer.x - track_left) / track_width).clamp(0.0, 1.0);
            let raw = min as f32 + fraction * (max - min) as f32;
            let snapped = ((raw / 64.0).round() as u32 * 64).clamp(min, max);
            if *value != snapped {
                *value = snapped;
                changed = true;
            }
        }
    }

    let fraction = if max > min {
        (*value).clamp(min, max).saturating_sub(min) as f32 / (max - min) as f32
    } else {
        0.0
    };
    let painter = ui.painter();

    if p.slider_ticks {
        painter.rect_filled(rect, egui::Rounding::ZERO, p.face);

        let channel = Rect::from_min_max(
            Pos2::new(rect.left() + 8.0, rect.center().y - 3.0),
            Pos2::new(rect.right() - 8.0, rect.center().y + 3.0),
        );
        painter.rect_filled(channel, egui::Rounding::ZERO, p.face);
        paint_bevel(painter, channel, false);

        for index in 0..=8 {
            let x = channel.left() + channel.width() * index as f32 / 8.0;
            painter.line_segment(
                [
                    Pos2::new(x, channel.bottom() + 2.0),
                    Pos2::new(x, channel.bottom() + 4.0),
                ],
                Stroke::new(1.0_f32, p.bevel_dark),
            );
        }

        let thumb_center = channel.left() + channel.width() * fraction;
        let thumb_rect = Rect::from_center_size(
            Pos2::new(
                thumb_center.clamp(rect.left() + 5.5, rect.right() - 5.5),
                rect.center().y,
            ),
            Vec2::new(11.0, 20.0),
        );
        let fill = if response.dragged() {
            p.face_pressed
        } else if response.hovered() {
            p.face_hover
        } else {
            p.face
        };
        painter.rect_filled(thumb_rect, egui::Rounding::ZERO, fill);
        paint_bevel(painter, thumb_rect, true);
    } else {
        let channel = Rect::from_center_size(
            rect.center(),
            Vec2::new((rect.width() - 16.0).max(1.0), 4.0),
        );
        let radius = channel.height() / 2.0;
        painter.rect_filled(channel, egui::Rounding::same(radius), p.track);

        let filled_width = channel.width() * fraction;
        if filled_width > 0.0 {
            let filled_width = filled_width.max(channel.height()).min(channel.width());
            let filled =
                Rect::from_min_size(channel.min, Vec2::new(filled_width, channel.height()));
            painter.rect_filled(filled, egui::Rounding::same(radius), p.accent);
        }

        let thumb_center = Pos2::new(
            (channel.left() + channel.width() * fraction)
                .clamp(rect.left() + 7.0, rect.right() - 7.0),
            rect.center().y,
        );
        let thumb_radius = if response.hovered() || response.dragged() {
            7.0
        } else {
            6.0
        };
        painter.circle_filled(thumb_center, thumb_radius + 0.5, p.well);
        painter.circle_filled(thumb_center, thumb_radius, p.button);
        painter.circle_stroke(thumb_center, thumb_radius, Stroke::new(1.5_f32, p.accent));
    }

    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    if changed {
        response.mark_changed();
    }
    response
}

/// Push button. `desired_size = None` sizes to the label.
pub fn button_sized(
    ui: &mut egui::Ui,
    text: &str,
    desired_size: Option<Vec2>,
    enabled: bool,
) -> Response {
    let theme = current_theme();
    let p = palette(theme);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font(UI_FONT_SIZE), p.text);
    let natural = Vec2::new(galley.size().x + 20.0, 24.0);
    let size = desired_size.unwrap_or(natural);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);

    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let fill = if !enabled {
        p.face
    } else if pressed {
        p.button_pressed
    } else if hovered {
        p.button_hover
    } else {
        p.button
    };

    let painter = ui.painter();
    match p.bevel {
        BevelStyle::Classic => {
            painter.rect_filled(rect, egui::Rounding::ZERO, fill);
            paint_bevel(painter, rect, !pressed);
        }
        BevelStyle::Flat | BevelStyle::Soft => {
            let rounding = egui::Rounding::same(p.button_rounding);
            if p.button_gloss && enabled && !pressed {
                gradient_rect(painter, rect, lighten(fill, 0.10), fill, rounding);
                let gloss = Rect::from_min_max(
                    rect.min + Vec2::new(1.0, 1.0),
                    Pos2::new(rect.right() - 1.0, rect.center().y),
                );
                painter.rect_filled(gloss, rounding, Color32::from_white_alpha(70));
            } else {
                painter.rect_filled(rect, rounding, fill);
            }
            let border = if !enabled {
                p.border
            } else if hovered && matches!(theme, Theme::Win10 | Theme::Win11) {
                p.accent
            } else {
                p.button_border
            };
            painter.rect_stroke(rect, rounding, Stroke::new(1.0_f32, border));
        }
    }

    let text_color = if enabled { p.text } else { p.text_disabled };
    let offset = if pressed && p.bevel != BevelStyle::Flat {
        Vec2::new(1.0, 1.0)
    } else {
        Vec2::ZERO
    };
    painter.text(
        rect.center() + offset,
        Align2::CENTER_CENTER,
        text,
        font(UI_FONT_SIZE),
        text_color,
    );

    if enabled && hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

pub fn button(ui: &mut egui::Ui, text: &str, enabled: bool) -> Response {
    button_sized(ui, text, None, enabled)
}

/// Accent-filled primary action ("Играть", "Далее", "Опубликовать"). Classic
/// Windows 98 keeps its grey push button; every modern layout gets a coloured
/// call to action so the main action is visually obvious.
pub fn primary_button_sized(
    ui: &mut egui::Ui,
    text: &str,
    desired_size: Option<Vec2>,
    enabled: bool,
) -> Response {
    let p = palette_now();
    if p.layout == LayoutStyle::Classic {
        return button_sized(ui, text, desired_size, enabled);
    }

    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font(UI_FONT_SIZE), p.primary_text);
    let natural = Vec2::new(galley.size().x + 26.0, 26.0);
    let size = desired_size.unwrap_or(natural);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);

    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let fill = if !enabled {
        p.text_disabled
    } else if pressed {
        p.primary_pressed
    } else if hovered {
        p.primary_hover
    } else {
        p.primary
    };
    let rounding = match p.layout {
        LayoutStyle::Aero => 3.0,
        LayoutStyle::Fluent => 6.0,
        _ => 0.0,
    };
    let painter = ui.painter();
    if p.layout == LayoutStyle::Aero && enabled && !pressed {
        gradient_rect(
            painter,
            rect,
            lighten(fill, 0.16),
            fill,
            egui::Rounding::same(rounding),
        );
        let gloss = Rect::from_min_max(
            rect.min + Vec2::new(1.0, 1.0),
            Pos2::new(rect.right() - 1.0, rect.center().y),
        );
        painter.rect_filled(
            gloss,
            egui::Rounding::same(rounding),
            Color32::from_white_alpha(60),
        );
    } else {
        painter.rect_filled(rect, egui::Rounding::same(rounding), fill);
    }
    painter.rect_stroke(
        rect,
        egui::Rounding::same(rounding),
        Stroke::new(1.0_f32, p.primary_pressed),
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        font(UI_FONT_SIZE),
        if enabled { p.primary_text } else { p.muted },
    );
    if enabled && hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

pub fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> Response {
    primary_button_sized(ui, text, None, enabled)
}

/// A water droplet used as the Windows 7 primary action.
///
/// Aero is all about glossy, translucent surfaces, so the main button becomes
/// a literal drop of water: a sharp tip, a round refractive bulb, a bright rim
/// and a glossy highlight. The caption sits in the widest part of the bulb.
pub fn water_drop_button(
    ui: &mut egui::Ui,
    text: &str,
    desired_size: Option<Vec2>,
    enabled: bool,
) -> Response {
    let p = palette_now();
    let size = desired_size.unwrap_or(Vec2::new(60.0, 64.0));
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);

    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let drop_rect = Rect::from_center_size(
        rect.center() + Vec2::new(0.0, 1.0),
        Vec2::new(rect.width() * 0.86, rect.height() * 0.92),
    );
    let points = droplet_points(drop_rect);
    let painter = ui.painter();

    if hovered {
        // A soft halo reads as light refracting around the water.
        let halo = Rect::from_center_size(drop_rect.center(), drop_rect.size() * 1.12);
        painter.add(egui::Shape::convex_polygon(
            droplet_points(halo),
            Color32::from_rgba_unmultiplied(150, 215, 255, 34),
            Stroke::NONE,
        ));
    }

    let (top, bottom) = if !enabled {
        (
            Color32::from_rgb(0xC9, 0xD4, 0xDC),
            Color32::from_rgb(0xA8, 0xB6, 0xC2),
        )
    } else if pressed {
        (
            Color32::from_rgb(0x2C, 0x86, 0xC8),
            Color32::from_rgb(0x12, 0x58, 0x92),
        )
    } else if hovered {
        (
            Color32::from_rgb(0x9B, 0xE2, 0xFF),
            Color32::from_rgb(0x2A, 0x8F, 0xD8),
        )
    } else {
        (
            Color32::from_rgb(0x7F, 0xD4, 0xFF),
            Color32::from_rgb(0x1B, 0x74, 0xB8),
        )
    };

    // A triangle fan with per-vertex colours gives the bulb a smooth watery
    // gradient without shaders.
    let mut mesh = egui::epaint::Mesh::default();
    let center = drop_rect.center() + Vec2::new(0.0, drop_rect.height() * 0.12);
    mesh.colored_vertex(center, mix(top, bottom, 0.55));
    for point in &points {
        let t = ((point.y - drop_rect.top()) / drop_rect.height()).clamp(0.0, 1.0);
        mesh.colored_vertex(*point, mix(top, bottom, t));
    }
    for index in 0..points.len() {
        let a = 1 + index as u32;
        let b = 1 + ((index + 1) % points.len()) as u32;
        mesh.add_triangle(0, a, b);
    }
    painter.add(egui::Shape::mesh(mesh));

    // Bright rim along the silhouette.
    painter.add(egui::Shape::closed_line(
        points,
        Stroke::new(
            1.2_f32,
            Color32::from_rgba_unmultiplied(255, 255, 255, if enabled { 175 } else { 90 }),
        ),
    ));

    // Glossy reflection: a big soft blob and a tiny sharp sparkle.
    if enabled {
        let highlight_center =
            drop_rect.center() + Vec2::new(-drop_rect.width() * 0.17, -drop_rect.height() * 0.10);
        painter.add(egui::Shape::ellipse_filled(
            highlight_center,
            Vec2::new(drop_rect.width() * 0.17, drop_rect.height() * 0.15),
            Color32::from_white_alpha(115),
        ));
        painter.add(egui::Shape::ellipse_filled(
            highlight_center + Vec2::new(3.0, 4.0),
            Vec2::new(drop_rect.width() * 0.06, drop_rect.height() * 0.055),
            Color32::from_white_alpha(220),
        ));
    }

    let label_pos = drop_rect.center() + Vec2::new(0.0, drop_rect.height() * 0.16);
    let label_color = if enabled {
        p.primary_text
    } else {
        Color32::from_rgb(0x5A, 0x66, 0x70)
    };
    painter.text(
        label_pos + Vec2::new(0.7, 0.7),
        Align2::CENTER_CENTER,
        text,
        font(11.5),
        Color32::from_black_alpha(70),
    );
    painter.text(
        label_pos,
        Align2::CENTER_CENTER,
        text,
        font(11.5),
        label_color,
    );

    if enabled && hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// Samples the outline of a teardrop in unit space scaled into `rect`.
/// The tip is at the top centre and the round bulb fills the bottom.
fn droplet_points(rect: Rect) -> Vec<Pos2> {
    let map = |x: f32, y: f32| {
        Pos2::new(
            rect.left() + x * rect.width(),
            rect.top() + y * rect.height(),
        )
    };
    let mut points = Vec::with_capacity(48);
    // Right flank: tip -> widest point.
    sample_cubic(
        &mut points,
        map(0.5, 0.0),
        map(0.66, 0.18),
        map(1.0, 0.42),
        map(1.0, 0.66),
        10,
    );
    // Bulb: right -> bottom -> left.
    sample_cubic(
        &mut points,
        map(1.0, 0.66),
        map(1.0, 0.87),
        map(0.78, 1.0),
        map(0.5, 1.0),
        10,
    );
    sample_cubic(
        &mut points,
        map(0.5, 1.0),
        map(0.22, 1.0),
        map(0.0, 0.87),
        map(0.0, 0.66),
        10,
    );
    // Left flank: bulb -> tip.
    sample_cubic(
        &mut points,
        map(0.0, 0.66),
        map(0.0, 0.42),
        map(0.34, 0.18),
        map(0.5, 0.0),
        10,
    );
    points
}

/// Pushes `steps` points of a cubic Bézier, excluding the start point so
/// consecutive segments chain without duplicates.
fn sample_cubic(out: &mut Vec<Pos2>, p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, steps: usize) {
    for step in 1..=steps {
        let t = step as f32 / steps as f32;
        let u = 1.0 - t;
        let x =
            u * u * u * p0.x + 3.0 * u * u * t * p1.x + 3.0 * u * t * t * p2.x + t * t * t * p3.x;
        let y =
            u * u * u * p0.y + 3.0 * u * u * t * p1.y + 3.0 * u * t * t * p2.y + t * t * t * p3.y;
        out.push(Pos2::new(x, y));
    }
}

fn draw_check(painter: &egui::Painter, rect: Rect, color: Color32, width: f32) {
    let a = rect.left_bottom() + Vec2::new(rect.width() * 0.18, -rect.height() * 0.38);
    let b = rect.center_bottom() + Vec2::new(-rect.width() * 0.06, -rect.height() * 0.12);
    let c = rect.right_top() + Vec2::new(-rect.width() * 0.12, rect.height() * 0.24);
    painter.line_segment([a, b], Stroke::new(width, color));
    painter.line_segment([b, c], Stroke::new(width, color));
}

/// Draws the familiar two-stroke check mark. Used by checkboxes and the theme
/// picker so the glyph stays identical everywhere.
pub fn check_mark(painter: &egui::Painter, rect: Rect, color: Color32) {
    draw_check(painter, rect, color, 2.0);
}

/// Checkbox with a sunken classic square or an accent-filled modern box.
pub fn checkbox(ui: &mut egui::Ui, checked: &mut bool, text: &str, enabled: bool) -> Response {
    let p = palette_now();
    let classic = p.bevel == BevelStyle::Classic;
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font(UI_FONT_SIZE), p.text);
    let box_size = if classic { 13.0 } else { 14.0 };
    let width = box_size + 5.0 + galley.size().x + 4.0;
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, 18.0), sense);

    let box_rect = Rect::from_min_size(
        rect.left_top() + Vec2::new(0.0, (18.0 - box_size) / 2.0),
        Vec2::splat(box_size),
    );
    let painter = ui.painter();

    if classic {
        painter.rect_filled(box_rect, egui::Rounding::ZERO, p.well);
        paint_bevel(painter, box_rect, false);
        if *checked {
            draw_check(
                painter,
                box_rect,
                if enabled { p.text } else { p.text_disabled },
                1.6,
            );
        }
    } else {
        let rounding = egui::Rounding::same(p.checkbox_rounding);
        let fill = if *checked {
            if enabled {
                p.accent
            } else {
                p.text_disabled
            }
        } else {
            p.well
        };
        painter.rect_filled(box_rect, rounding, fill);
        let border = if *checked {
            p.accent_pressed
        } else if enabled && response.hovered() {
            p.accent
        } else {
            p.border
        };
        painter.rect_stroke(box_rect, rounding, Stroke::new(1.0_f32, border));
        if *checked {
            draw_check(painter, box_rect, p.selection_text, 2.0);
        }
    }

    painter.text(
        box_rect.right_center() + Vec2::new(5.0, 0.0),
        Align2::LEFT_CENTER,
        text,
        font(UI_FONT_SIZE),
        if enabled { p.text } else { p.text_disabled },
    );

    if enabled {
        if response.clicked() {
            *checked = !*checked;
            response.mark_changed();
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
    response
}

/// Theme-aware progress bar: segmented navy/green bars for the classic themes,
/// a continuous rounded accent fill for Metro and Windows 11.
pub fn progress_bar(ui: &mut egui::Ui, fraction: f32, text: &str) {
    let theme = current_theme();
    let p = palette(theme);
    let width = ui.available_width().max(80.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 20.0), Sense::hover());
    let painter = ui.painter();
    let fraction = fraction.clamp(0.0, 1.0);

    match theme {
        Theme::Win98 => {
            painter.rect_filled(rect, egui::Rounding::ZERO, p.face);
            paint_bevel(painter, rect, false);

            let inner = rect.shrink(3.0);
            painter.rect_filled(inner, egui::Rounding::ZERO, p.face);
            draw_segments(painter, inner, fraction, p.progress, 28, false);
        }
        Theme::Win7 => {
            painter.rect_filled(rect, egui::Rounding::ZERO, p.face);
            paint_bevel(painter, rect, false);

            let inner = rect.shrink(3.0);
            painter.rect_filled(inner, egui::Rounding::ZERO, p.well);
            draw_segments(painter, inner, fraction, p.progress, 24, true);
        }
        Theme::Win81 | Theme::Win10 | Theme::Win11 => {
            let rounding = if theme == Theme::Win11 {
                egui::Rounding::same(3.0)
            } else {
                egui::Rounding::ZERO
            };
            painter.rect_filled(rect, rounding, p.track);
            painter.rect_stroke(rect, rounding, Stroke::new(1.0_f32, p.border));

            let inner = rect.shrink(2.0);
            let fill_width = inner.width() * fraction;
            if fill_width > 0.0 {
                let fill_width = fill_width.max(inner.height()).min(inner.width());
                let fill = Rect::from_min_size(inner.min, Vec2::new(fill_width, inner.height()));
                let fill_rounding = if theme == Theme::Win11 {
                    egui::Rounding::same(inner.height() / 2.0)
                } else {
                    egui::Rounding::ZERO
                };
                painter.rect_filled(fill, fill_rounding, p.progress);
            }
        }
    }

    if !text.is_empty() {
        let text_color =
            if matches!(theme, Theme::Win81 | Theme::Win10 | Theme::Win11) && fraction > 0.55 {
                p.selection_text
            } else {
                p.text
            };
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            font(11.0),
            text_color,
        );
    }
}

fn draw_segments(
    painter: &egui::Painter,
    inner: Rect,
    fraction: f32,
    color: Color32,
    segments: usize,
    glossy: bool,
) {
    let gap = 1.0;
    let block_w = ((inner.width() - gap * (segments as f32 - 1.0)) / segments as f32).max(0.0);
    let filled = (fraction * segments as f32).round() as usize;
    for index in 0..filled.min(segments) {
        let x = inner.left() + index as f32 * (block_w + gap);
        let block = Rect::from_min_size(
            Pos2::new(x, inner.top()),
            Vec2::new(block_w, inner.height()),
        );
        painter.rect_filled(block, egui::Rounding::ZERO, color);
        if glossy {
            let gloss = Rect::from_min_max(block.min, Pos2::new(block.right(), block.center().y));
            painter.rect_filled(gloss, egui::Rounding::ZERO, Color32::from_white_alpha(80));
        }
    }
}

/// One-pixel separator, etched in the classic theme and subtle elsewhere.
pub fn separator(ui: &mut egui::Ui) {
    let p = palette_now();
    let width = ui.available_width().max(8.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 2.0), Sense::hover());
    let painter = ui.painter();
    painter.line_segment(
        [rect.left_center(), rect.right_center()],
        Stroke::new(1.0_f32, p.border),
    );
    if p.bevel == BevelStyle::Classic {
        painter.line_segment(
            [
                rect.left_center() + Vec2::new(0.0, 1.0),
                rect.right_center() + Vec2::new(0.0, 1.0),
            ],
            Stroke::new(1.0_f32, p.bevel_light),
        );
    }
}

/// Small square caption button used by custom title bars. The close glyph gets
/// the familiar red hover on modern Windows themes.
pub fn caption_button(ui: &mut egui::Ui, text: &str) -> Response {
    let theme = current_theme();
    let p = palette(theme);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(17.0, 15.0), Sense::click());
    let hovered = response.hovered();
    let pressed = response.is_pointer_button_down_on();
    let is_close = matches!(text, "×" | "x" | "X");
    let painter = ui.painter();

    if p.bevel == BevelStyle::Classic {
        painter.rect_filled(
            rect,
            egui::Rounding::ZERO,
            if pressed {
                p.button_pressed
            } else if hovered {
                p.button_hover
            } else {
                p.button
            },
        );
        paint_bevel(painter, rect, !pressed);
        painter.text(
            rect.center()
                + if pressed {
                    Vec2::new(1.0, 1.0)
                } else {
                    Vec2::ZERO
                },
            Align2::CENTER_CENTER,
            text,
            font(11.0),
            p.text,
        );
    } else {
        let fill = if hovered && is_close {
            p.error
        } else if pressed && is_close {
            p.accent_pressed
        } else if pressed {
            p.button_pressed
        } else if hovered {
            p.button_hover
        } else {
            Color32::TRANSPARENT
        };
        painter.rect_filled(rect, egui::Rounding::same(p.button_rounding.min(4.0)), fill);
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            font(11.0),
            if hovered && is_close {
                Color32::WHITE
            } else {
                p.text
            },
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_palette_matches_windows_98() {
        let palette = palette(Theme::Win98);
        assert_eq!(palette.face.to_array(), [0xC0, 0xC0, 0xC0, 0xFF]);
        assert_eq!(palette.title_bottom.to_array(), [0x00, 0x00, 0x80, 0xFF]);
        assert_eq!(palette.bevel, BevelStyle::Classic);
    }

    #[test]
    fn every_theme_has_distinct_surfaces() {
        let mut faces = std::collections::BTreeSet::new();
        let mut titles = std::collections::BTreeSet::new();
        for theme in Theme::ALL {
            let palette = palette(theme);
            faces.insert((palette.face.r(), palette.face.g(), palette.face.b()));
            titles.insert((
                palette.title_bottom.r(),
                palette.title_bottom.g(),
                palette.title_bottom.b(),
            ));
            assert!(!theme.label().is_empty());
        }
        assert!(faces.len() >= 3, "themes should not share every colour");
        assert!(titles.len() >= 4, "title bars should be visually distinct");
    }

    #[test]
    fn modern_themes_drop_the_classic_bevel() {
        assert_eq!(palette(Theme::Win7).bevel, BevelStyle::Soft);
        assert_eq!(palette(Theme::Win81).bevel, BevelStyle::Flat);
        assert_eq!(palette(Theme::Win10).bevel, BevelStyle::Flat);
        assert_eq!(palette(Theme::Win11).bevel, BevelStyle::Soft);
        assert!(palette(Theme::Win11).rounding > 0.0);
    }

    #[test]
    fn font_sizes_snap_to_physical_pixels() {
        for ppp in [1.0_f32, 1.25, 1.5, 1.75, 2.0, 2.25] {
            for size in [10.0_f32, 11.0, 11.5, 12.0, 12.5, 15.0] {
                let snapped = snap_font_size(size, ppp);
                let physical = snapped * ppp;
                assert!(
                    (physical - physical.round()).abs() < 0.001,
                    "{size} pt at {ppp}x became {physical} px"
                );
                assert!((snapped - size).abs() <= 0.5 / ppp + f32::EPSILON);
            }
        }
        assert_eq!(snap_font_size(12.0, 1.0), 12.0);
        assert_eq!(snap_font_size(12.0, 1.5), 12.0);
        assert!((snap_font_size(12.5, 1.5) - 19.0 / 1.5).abs() < 0.001);
        assert_eq!(snap_font_size(12.0, 0.0), 12.0);
    }
}
