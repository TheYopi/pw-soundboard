//! Look and feel: colors from the desktop's qt5ct/qt6ct color scheme (as written by desktop
//! shells such as Quickshell themes), live-reloaded when the file changes, plus the system fonts
//! and a few drawing helpers for the angular "notched" shapes.

use egui::epaint::{Mesh, Shadow};
use egui::{Color32, CornerRadius, FontFamily, FontId, Pos2, Rect, Shape, Stroke, TextStyle, pos2, vec2};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// Semantic colors used by the whole UI.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub dark_mode: bool,
    /// Main background (grid).
    pub bg: Color32,
    /// Bars and side panel, inputs.
    pub bg_deep: Color32,
    /// Cards, buttons.
    pub surface: Color32,
    pub surface_hi: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub accent2: Color32,
    pub danger: Color32,
    pub success: Color32,
    pub warn: Color32,
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn luminance(c: Color32) -> f32 {
    (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) / 255.0
}

pub fn alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0) as u8)
}

impl Palette {
    /// Derive the UI colors from the few Qt roles that every scheme sets sensibly
    /// (window, text, highlight, …). Roles like `dark`/`midlight` vary too much between schemes.
    fn derive(window: Color32, text: Color32, accent: Color32, accent2: Color32, muted: Option<Color32>) -> Self {
        let dark_mode = luminance(window) < 0.5;
        let shade = if dark_mode { Color32::BLACK } else { Color32::WHITE };
        // An accent too close to the background (some schemes use a dim highlight) gets lifted.
        let accent = if (luminance(accent) - luminance(window)).abs() < 0.25 { mix(accent, text, 0.45) } else { accent };
        let on_accent = if luminance(accent) > 0.55 { mix(window, Color32::BLACK, 0.5) } else { Color32::WHITE };
        Palette {
            dark_mode,
            bg: window,
            bg_deep: mix(window, shade, if dark_mode { 0.35 } else { 0.5 }),
            surface: mix(window, text, 0.05),
            surface_hi: mix(window, text, 0.10),
            border: mix(window, text, 0.15),
            border_strong: mix(window, text, 0.32),
            text,
            text_dim: mix(text, window, 0.28),
            text_muted: muted.filter(|m| *m != text).unwrap_or_else(|| mix(text, window, 0.5)),
            accent,
            on_accent,
            accent2,
            danger: Color32::from_rgb(255, 77, 109),
            success: Color32::from_rgb(61, 220, 151),
            warn: Color32::from_rgb(255, 184, 77),
        }
    }

    pub fn builtin() -> Self {
        Self::derive(
            Color32::from_rgb(0x0B, 0x10, 0x18),
            Color32::from_rgb(0xE3, 0xEA, 0xF2),
            Color32::from_rgb(0x4D, 0xC3, 0xFF),
            Color32::from_rgb(0x9B, 0x7B, 0xFF),
            None,
        )
    }

    /// Parse a qt5ct/qt6ct `[ColorScheme]` file. Colors are QPalette roles in enum order:
    /// WindowText, Button, Light, Midlight, Dark, Mid, Text, BrightText, ButtonText, Base, Window,
    /// Shadow, Highlight, HighlightedText, Link, LinkVisited, AlternateBase, NoRole, ToolTipBase,
    /// ToolTipText, PlaceholderText, (Qt 6.6+) Accent.
    pub fn from_qtct(text: &str) -> Option<Self> {
        let colors = |key: &str| -> Option<Vec<Color32>> {
            let line = text.lines().find(|l| l.trim_start().starts_with(key))?;
            let (_, list) = line.split_once('=')?;
            list.split(',').map(|c| parse_hex(c.trim())).collect()
        };
        let active = colors("active_colors")?;
        if active.len() < 17 {
            return None;
        }
        let window = active[10];
        let text = active[0];
        let accent = active.get(21).copied().unwrap_or(active[12]);
        let muted = active.get(20).copied();
        Some(Self::derive(window, text, accent, active[15], muted))
    }
}

fn parse_hex(s: &str) -> Option<Color32> {
    let h = s.strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok()?;
    match h.len() {
        6 => Some(Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)),
        // Qt writes #AARRGGBB
        8 => Some(Color32::from_rgba_unmultiplied((v >> 16) as u8, (v >> 8) as u8, v as u8, (v >> 24) as u8)),
        _ => None,
    }
}

// ------------------------------------------------------------------------------------------------
// Scheme files
// ------------------------------------------------------------------------------------------------

fn scheme_dirs() -> Vec<PathBuf> {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    vec![base.join("qt6ct/colors"), base.join("qt5ct/colors")]
}

/// Names of the available color schemes (file stems), qt6ct first.
pub fn list_schemes() -> Vec<String> {
    let mut names = Vec::new();
    for dir in scheme_dirs() {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut found: Vec<String> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? != "conf" {
                    return None;
                }
                Some(p.file_stem()?.to_string_lossy().into_owned())
            })
            .collect();
        found.sort();
        for n in found {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    names
}

/// `""` follows the desktop (the most recently written scheme, which is the one a theme switcher
/// just generated), `"builtin"` uses the app's own colors, anything else is a scheme name.
fn resolve(source: &str) -> Option<PathBuf> {
    match source {
        "builtin" => None,
        "" => scheme_dirs().into_iter().find_map(|dir| {
            std::fs::read_dir(dir)
                .ok()?
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "conf"))
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH))
                .map(|e| e.path())
        }),
        name => scheme_dirs()
            .into_iter()
            .map(|d| d.join(format!("{name}.conf")))
            .find(|p| p.exists()),
    }
}

/// Tracks the chosen scheme and reloads it when the file (or the newest file) changes.
pub struct ThemeWatcher {
    source: String,
    loaded: Option<(PathBuf, SystemTime)>,
    last_check: Instant,
    pub palette: Palette,
    /// Name of the scheme in use, for the settings panel.
    pub current: Option<String>,
}

impl ThemeWatcher {
    pub fn new(source: &str) -> Self {
        let mut w = Self {
            source: source.to_string(),
            loaded: None,
            last_check: Instant::now(),
            palette: Palette::builtin(),
            current: None,
        };
        w.reload();
        w
    }

    pub fn set_source(&mut self, source: &str) {
        self.source = source.to_string();
        self.loaded = None;
        self.reload();
    }

    fn reload(&mut self) -> bool {
        let path = resolve(&self.source);
        let stamp = path.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        let key = path.clone().zip(stamp);
        if key.is_some() && key == self.loaded {
            return false;
        }
        let palette = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| Palette::from_qtct(&t))
            .unwrap_or_else(Palette::builtin);
        self.current = path.as_ref().and_then(|p| Some(p.file_stem()?.to_string_lossy().into_owned()));
        self.loaded = key;
        let changed = palette != self.palette;
        self.palette = palette;
        changed
    }

    /// Check (at most once a second) whether the scheme changed. Returns true if it did.
    pub fn poll(&mut self) -> bool {
        if self.last_check.elapsed() < Duration::from_secs(1) {
            return false;
        }
        self.last_check = Instant::now();
        self.reload()
    }
}

// ------------------------------------------------------------------------------------------------
// Fonts and egui style
// ------------------------------------------------------------------------------------------------

pub const BOLD: &str = "bold";

fn fc_match(pattern: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    // egui can't load font collections.
    if path.is_empty() || path.ends_with(".ttc") || path.ends_with(".pcf.gz") {
        return None;
    }
    std::fs::read(path).ok()
}

/// Use the desktop's sans-serif and monospace fonts, keeping egui's fonts as fallback (they
/// carry the symbols used for icons).
pub fn install_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions};
    use std::sync::Arc;
    fn add(fonts: &mut FontDefinitions, name: &str, data: Option<Vec<u8>>, family: FontFamily) {
        if let Some(bytes) = data {
            fonts.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
            fonts.families.entry(family).or_default().insert(0, name.into());
        }
    }
    let mut fonts = FontDefinitions::default();
    add(&mut fonts, "system-sans", fc_match("sans-serif:weight=regular"), FontFamily::Proportional);
    add(&mut fonts, "system-mono", fc_match("monospace:weight=regular"), FontFamily::Monospace);
    // A bold family for headings; falls back to the regular fonts.
    let fallback = fonts.families[&FontFamily::Proportional].clone();
    fonts.families.insert(FontFamily::Name(BOLD.into()), fallback);
    add(&mut fonts, "system-sans-bold", fc_match("sans-serif:weight=bold"), FontFamily::Name(BOLD.into()));
    ctx.set_fonts(fonts);
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

/// Corner radius used for widgets in the current style.
pub fn radius(angular: bool) -> u8 {
    if angular { 2 } else { 8 }
}

pub fn apply(ctx: &egui::Context, p: &Palette, angular: bool) {
    ctx.set_theme(if p.dark_mode { egui::Theme::Dark } else { egui::Theme::Light });
    let r = CornerRadius::same(radius(angular));
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Body, FontId::proportional(13.5)),
            (TextStyle::Button, FontId::proportional(13.5)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
            (TextStyle::Heading, bold(17.0)),
        ]
        .into();
        let s = &mut style.spacing;
        s.item_spacing = vec2(8.0, 7.0);
        s.button_padding = vec2(10.0, 5.0);
        s.interact_size.y = 26.0;
        s.slider_rail_height = 4.0;
        s.window_margin = egui::Margin::same(14);
        s.menu_margin = egui::Margin::same(6);

        let v = &mut style.visuals;
        v.dark_mode = p.dark_mode;
        v.override_text_color = None;
        v.panel_fill = p.bg_deep;
        v.window_fill = p.bg_deep;
        v.window_stroke = Stroke::new(1.0, p.border_strong);
        v.window_corner_radius = r;
        v.menu_corner_radius = r;
        v.window_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: alpha(Color32::BLACK, 0.45) };
        v.popup_shadow = Shadow { offset: [0, 4], blur: 12, spread: 0, color: alpha(Color32::BLACK, 0.35) };
        v.faint_bg_color = p.surface;
        v.extreme_bg_color = p.bg_deep;
        v.text_edit_bg_color = Some(mix(p.bg_deep, p.bg, 0.5));
        v.code_bg_color = p.surface;
        v.hyperlink_color = p.accent;
        v.warn_fg_color = p.warn;
        v.error_fg_color = p.danger;
        v.selection.bg_fill = alpha(p.accent, 0.35);
        v.selection.stroke = Stroke::new(1.0, p.accent);
        v.slider_trailing_fill = true;
        v.handle_shape = if angular {
            egui::style::HandleShape::Rect { aspect_ratio: 0.55 }
        } else {
            egui::style::HandleShape::Circle
        };
        v.text_cursor.stroke = Stroke::new(2.0, p.accent);

        let w = &mut v.widgets;
        w.noninteractive.bg_fill = p.bg_deep;
        w.noninteractive.weak_bg_fill = p.bg_deep;
        w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
        w.noninteractive.fg_stroke = Stroke::new(1.0, p.text_dim);
        w.noninteractive.corner_radius = r;

        // bg_fill is also the inside of checkboxes and slider rails, so it must stand out on cards.
        w.inactive.bg_fill = p.bg_deep;
        w.inactive.weak_bg_fill = p.surface;
        w.inactive.bg_stroke = Stroke::new(1.0, p.border_strong);
        w.inactive.fg_stroke = Stroke::new(1.0, p.text);
        w.inactive.corner_radius = r;
        w.inactive.expansion = 0.0;

        w.hovered.bg_fill = mix(p.surface_hi, p.accent, 0.25);
        w.hovered.weak_bg_fill = p.surface_hi;
        w.hovered.bg_stroke = Stroke::new(1.0, p.accent);
        w.hovered.fg_stroke = Stroke::new(1.5, p.text);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;

        w.active.bg_fill = p.accent;
        w.active.weak_bg_fill = mix(p.surface_hi, p.accent, 0.35);
        w.active.bg_stroke = Stroke::new(1.0, p.accent);
        w.active.fg_stroke = Stroke::new(1.5, p.text);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;

        w.open = w.active;
        w.open.weak_bg_fill = p.surface_hi;
    });
}

// ------------------------------------------------------------------------------------------------
// Shapes
// ------------------------------------------------------------------------------------------------

/// Outline of a rectangle with its bottom-right corner cut off (the shell's "notch").
pub fn notched(rect: Rect, notch: f32) -> Vec<Pos2> {
    let n = notch.min(rect.width() / 2.0).min(rect.height() / 2.0);
    vec![
        rect.left_top(),
        rect.right_top(),
        pos2(rect.right(), rect.bottom() - n),
        pos2(rect.right() - n, rect.bottom()),
        rect.left_bottom(),
    ]
}

/// A card: notched (angular) or rounded, with fill and border.
pub fn card(painter: &egui::Painter, rect: Rect, angular: bool, notch: f32, fill: Color32, stroke: Stroke) {
    if angular {
        painter.add(Shape::convex_polygon(notched(rect, notch), fill, Stroke::NONE));
        if stroke.width > 0.0 {
            painter.add(Shape::closed_line(notched(rect.shrink(stroke.width / 2.0), notch), stroke));
        }
    } else {
        painter.rect(rect, 10, fill, stroke, egui::StrokeKind::Inside);
    }
}

/// Paint the background color over the cut-off corner (for images that can't be clipped).
pub fn cut_corner(painter: &egui::Painter, rect: Rect, notch: f32, bg: Color32) {
    let n = notch.min(rect.width() / 2.0);
    painter.add(Shape::convex_polygon(
        vec![
            rect.right_bottom() + vec2(0.5, 0.5),
            pos2(rect.right() - n, rect.bottom() + 0.5),
            pos2(rect.right() + 0.5, rect.bottom() - n),
        ],
        bg,
        Stroke::NONE,
    ));
}

/// L-shaped ticks on the three un-notched corners (the shell's corner marks).
pub fn corner_ticks(painter: &egui::Painter, rect: Rect, len: f32, stroke: Stroke) {
    let r = rect.expand(3.0);
    for (corner, dx, dy) in [(r.left_bottom(), 1.0, -1.0), (r.left_top(), 1.0, 1.0), (r.right_top(), -1.0, 1.0)] {
        painter.line_segment([corner, corner + vec2(len * dx, 0.0)], stroke);
        painter.line_segment([corner, corner + vec2(0.0, len * dy)], stroke);
    }
}

/// Vertical gradient from transparent (top) to `color` (bottom).
pub fn scrim(painter: &egui::Painter, rect: Rect, color: Color32) {
    let mut mesh = Mesh::default();
    let top = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 0);
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), color);
    mesh.colored_vertex(rect.right_bottom(), color);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    painter.add(Shape::mesh(mesh));
}

/// Soft colored glow around a rectangle.
pub fn glow(painter: &egui::Painter, rect: Rect, color: Color32) {
    let shadow = Shadow { offset: [0, 0], blur: 18, spread: 1, color };
    painter.add(shadow.as_shape(rect, CornerRadius::ZERO));
}

#[cfg(test)]
mod tests {
    use super::*;

    const BW77: &str = "[ColorScheme]\nactive_colors=#DFF1FB, #0C1722, #152433, #0C1722, #02060C, #060D16, #DFF1FB, #DFF1FB, #DFF1FB, #060D16, #060D16, #02060C, #33D6FF, #02060C, #33D6FF, #8A6BFF, #0C1722, #DFF1FB, #02060C, #DFF1FB, #5D7A8C\n";

    #[test]
    fn parses_qtct_scheme() {
        let p = Palette::from_qtct(BW77).unwrap();
        assert!(p.dark_mode);
        assert_eq!(p.bg, Color32::from_rgb(0x06, 0x0D, 0x16));
        assert_eq!(p.text, Color32::from_rgb(0xDF, 0xF1, 0xFB));
        assert_eq!(p.accent, Color32::from_rgb(0x33, 0xD6, 0xFF));
        assert_eq!(p.accent2, Color32::from_rgb(0x8A, 0x6B, 0xFF));
        assert_eq!(p.text_muted, Color32::from_rgb(0x5D, 0x7A, 0x8C));
    }

    #[test]
    fn qt6_accent_role_wins_and_dim_accent_is_lifted() {
        // noctalia: highlight #004c4d is nearly the background, accent role is #00ffff
        let s = "active_colors=#f0f4f7, #0a1417, #0f1d21, #cacaca, #9f9f9f, #0f1d21, #f0f4f7, #ffffff, #f0f4f7, #0a1417, #0a1417, #03080a, #004c4d, #ccffff, #aab1b8, #00ffff, #0f1d21, #0a1417, #0f1d21, #f0f4f7, #f0f4f7, #00ffff";
        assert_eq!(Palette::from_qtct(s).unwrap().accent, Color32::from_rgb(0, 255, 255));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Palette::from_qtct("[ColorScheme]\nactive_colors=#zzz").is_none());
        assert!(Palette::from_qtct("nothing").is_none());
    }
}
