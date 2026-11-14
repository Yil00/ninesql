//! Light / dark themes and font settings (F-WS-03).
//!
//! Themes are gpui-component's default light and dark themes. Fonts and sizes from
//! `[appearance]` are applied on top and re-applied after every theme switch. Sizes are logical
//! pixels: GPUI multiplies them by the window scale factor (Wayland `wp_fractional_scale_v1`,
//! X11 `Xft.dpi` or `GPUI_X11_SCALE_FACTOR`), so fractional scaling needs no code here.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, Pixels, px};

use crate::settings::{Appearance, FONT_SIZE_RANGE, ThemePreference};

/// Step of the font size actions, in pixels.
const FONT_STEP: f32 = 1.0;

/// Font sizes from the settings and the current zoom offset.
#[derive(Debug, Clone, Default)]
pub struct AppearanceState {
    /// The `[appearance]` section in effect.
    pub settings: Appearance,
    /// Theme default sizes, captured before any override (UI, editor).
    defaults: Option<(Pixels, Pixels)>,
    /// Offset added by the font size actions, in pixels.
    pub zoom: f32,
}

impl Global for AppearanceState {}

fn clamp_size(size: f32) -> f32 {
    size.clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end())
}

/// The theme mode a preference resolves to, given the desktop appearance.
#[must_use]
pub fn resolve_mode(pref: ThemePreference, system_dark: bool) -> ThemeMode {
    match pref {
        ThemePreference::Light => ThemeMode::Light,
        ThemePreference::Dark => ThemeMode::Dark,
        ThemePreference::System if system_dark => ThemeMode::Dark,
        ThemePreference::System => ThemeMode::Light,
    }
}

/// Applies `[appearance]` at startup. `gpui_kit::init` must have run.
pub fn init(appearance: Appearance, cx: &mut App) {
    let system_dark = ThemeMode::from(cx.window_appearance()).is_dark();
    let mode = resolve_mode(appearance.theme, system_dark);
    cx.set_global(AppearanceState {
        settings: appearance,
        defaults: None,
        zoom: 0.0,
    });
    set_mode(mode, cx);
}

/// Switches to `mode` and re-applies the font settings and the contrast fixes.
pub fn set_mode(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    enforce_contrast(cx);
    apply_fonts(cx);
}

/// Minimum contrast of body text (WCAG 2.x AA).
pub const MIN_TEXT_CONTRAST: f32 = 4.5;

/// Moves the lightness of `fg` away from the backgrounds until it reaches `min` contrast with
/// every one of them (or black / white is reached). Hue and saturation are kept.
#[must_use]
pub fn ensure_contrast(fg: Hsla, backgrounds: &[Hsla], min: f32) -> Hsla {
    let worst = |c: Hsla| {
        backgrounds
            .iter()
            .map(|bg| contrast_ratio(c, *bg))
            .fold(f32::INFINITY, f32::min)
    };
    if worst(fg) >= min {
        return fg;
    }
    let mean_bg = backgrounds
        .iter()
        .map(|bg| relative_luminance(*bg))
        .sum::<f32>()
        / backgrounds.len().max(1) as f32;
    let step = if mean_bg > 0.18 { -0.01 } else { 0.01 };
    let mut c = fg;
    while worst(c) < min && (0.0..=1.0).contains(&(c.l + step)) {
        c.l += step;
    }
    c
}

/// Fixes the default themes where secondary text falls under AA contrast on the surfaces it is
/// drawn on (gpui-component's light `muted_foreground` on the status bar is 4.46:1).
fn enforce_contrast(cx: &mut App) {
    Theme::update(cx, |theme| {
        let surfaces = [
            theme.background,
            *theme.tokens.status_bar,
            theme.sidebar,
            theme.secondary,
        ];
        theme.muted_foreground =
            ensure_contrast(theme.muted_foreground, &surfaces, MIN_TEXT_CONTRAST);
        theme.sidebar_foreground = ensure_contrast(
            theme.sidebar_foreground,
            &[theme.sidebar],
            MIN_TEXT_CONTRAST,
        );
        theme.foreground =
            ensure_contrast(theme.foreground, &[theme.background], MIN_TEXT_CONTRAST);
    });
}

/// Switches between light and dark.
pub fn toggle_mode(cx: &mut App) {
    let next = if Theme::global(cx).mode.is_dark() {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    set_mode(next, cx);
}

/// The current theme mode.
#[must_use]
pub fn current_mode(cx: &App) -> ThemeMode {
    Theme::global(cx).mode
}

/// Changes the zoom offset by `steps` font steps (negative = smaller); `None` resets it.
pub fn zoom(steps: Option<f32>, cx: &mut App) {
    let state = cx.default_global::<AppearanceState>();
    state.zoom = match steps {
        Some(steps) => (state.zoom + steps * FONT_STEP).clamp(-8.0, 16.0),
        None => 0.0,
    };
    apply_fonts(cx);
}

/// Writes fonts and sizes from [`AppearanceState`] into the theme.
fn apply_fonts(cx: &mut App) {
    let theme = Theme::global(cx);
    let theme_defaults = (theme.font_size, theme.mono_font_size);
    let state = cx.default_global::<AppearanceState>();
    let (ui_default, mono_default) = *state.defaults.get_or_insert(theme_defaults);
    let s = state.settings.clone();
    let zoom = state.zoom;
    let ui = clamp_size(s.ui_font_size.unwrap_or(f32::from(ui_default)) + zoom);
    let mono = clamp_size(s.mono_font_size.unwrap_or(f32::from(mono_default)) + zoom);
    Theme::update(cx, |theme| {
        if let Some(family) = &s.ui_font_family {
            theme.font_family = family.clone().into();
        }
        if let Some(family) = &s.mono_font_family {
            theme.mono_font_family = family.clone().into();
        }
        theme.font_size = px(ui);
        theme.mono_font_size = px(mono);
    });
}

/// WCAG 2.x relative luminance of a color (alpha ignored).
#[must_use]
pub fn relative_luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    let channel = |c: f32| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

/// WCAG 2.x contrast ratio between two colors, from 1 to 21.
#[must_use]
pub fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::{contrast_ratio, ensure_contrast, resolve_mode};
    use crate::settings::ThemePreference;
    use gpui_kit::component::ThemeMode;
    use gpui_kit::{black, white};

    #[test]
    fn preference_resolution() {
        assert_eq!(resolve_mode(ThemePreference::Dark, false), ThemeMode::Dark);
        assert_eq!(resolve_mode(ThemePreference::Light, true), ThemeMode::Light);
        assert_eq!(resolve_mode(ThemePreference::System, true), ThemeMode::Dark);
        assert_eq!(
            resolve_mode(ThemePreference::System, false),
            ThemeMode::Light
        );
    }

    #[test]
    fn ensure_contrast_darkens_or_lightens() {
        let grey = gpui_kit::hsla(0.6, 0.1, 0.55, 1.0);
        let on_white = ensure_contrast(grey, &[white()], 4.5);
        assert!(contrast_ratio(on_white, white()) >= 4.5);
        assert!(on_white.l < grey.l);
        let on_black = ensure_contrast(grey, &[black()], 7.0);
        assert!(contrast_ratio(on_black, black()) >= 7.0);
        assert!(on_black.l > grey.l);
        assert_eq!(ensure_contrast(black(), &[white()], 4.5), black());
    }

    #[test]
    fn contrast_of_black_on_white_is_21() {
        assert!((contrast_ratio(black(), white()) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(white(), white()) - 1.0).abs() < 0.001);
    }
}
