//! Production visuals (ADR-0007 §4): the connection type color framing the editor area and the
//! connection rows, the type badge, the read-only badge and the security-warning badge, with
//! text colors chosen for WCAG AA contrast in both themes.

use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ninesql_core::{Color, Connection, ConnectionType, ConnectionTypes};

use crate::appearance::{MIN_TEXT_CONTRAST, contrast_ratio};

/// A connection type color as a GPUI color.
#[must_use]
pub fn hsla(c: Color) -> Hsla {
    Rgba {
        r: f32::from(c.r) / 255.0,
        g: f32::from(c.g) / 255.0,
        b: f32::from(c.b) / 255.0,
        a: 1.0,
    }
    .into()
}

/// Black or white, whichever reads better on `bg`.
#[must_use]
pub fn text_on(bg: Hsla) -> Hsla {
    let black = Hsla::black();
    let white = Hsla::white();
    if contrast_ratio(black, bg) >= contrast_ratio(white, bg) {
        black
    } else {
        white
    }
}

/// What the navigator, the status bar and the dialogs show for one connection.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionBadges {
    /// Type name (`Production`).
    pub type_name: SharedString,
    /// Type color.
    pub color: Hsla,
    /// Text color on `color` (AA contrast).
    pub on_color: Hsla,
    /// Production rules apply: colored frame, type badge always shown.
    pub production: bool,
    /// Sessions are read-only (enforced engine-side, I3).
    pub read_only: bool,
    /// Number of weakened security settings (I5).
    pub security_warnings: usize,
}

impl ConnectionBadges {
    /// Badges of a saved connection under `types`. `session_read_only` is the open session's
    /// actual mode, when there is one: it wins over the configured one.
    #[must_use]
    pub fn of(c: &Connection, types: &ConnectionTypes, session_read_only: Option<bool>) -> Self {
        let ty: ConnectionType = types.resolve(&c.connection_type);
        let policy = c.policy(types);
        let color = hsla(ty.color);
        Self {
            type_name: ty.name.into(),
            color,
            on_color: text_on(color),
            production: policy.production,
            read_only: session_read_only.unwrap_or(policy.read_only),
            security_warnings: c.security_warnings().len(),
        }
    }

    /// The contrast of the type badge text.
    #[must_use]
    pub fn badge_contrast(&self) -> f32 {
        contrast_ratio(self.on_color, self.color)
    }
}

/// A small pill.
fn pill(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    bg: Hsla,
    fg: Hsla,
) -> Stateful<Div> {
    div()
        .id(id.into())
        .px_1()
        .rounded_sm()
        .text_xs()
        .bg(bg)
        .text_color(fg)
        .child(text.into())
}

/// Type badge, read-only badge and security badge, side by side. `key` keeps element ids
/// unique per row.
pub fn badges(key: &str, b: &ConnectionBadges, show_type: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let ro_bg = theme.secondary;
    let ro_fg =
        crate::appearance::ensure_contrast(theme.secondary_foreground, &[ro_bg], MIN_TEXT_CONTRAST);
    let warn_fg = crate::appearance::ensure_contrast(
        theme.warning,
        &[theme.background, theme.sidebar],
        MIN_TEXT_CONTRAST,
    );
    h_flex()
        .gap_1()
        .when(show_type || b.production, |el| {
            el.child(pill(
                SharedString::from(format!("type-badge-{key}")),
                b.type_name.clone(),
                b.color,
                b.on_color,
            ))
        })
        .when(b.read_only, |el| {
            el.child(pill(
                SharedString::from(format!("ro-badge-{key}")),
                "Read-only",
                ro_bg,
                ro_fg,
            ))
        })
        .when(b.security_warnings > 0, |el| {
            el.child(
                div()
                    .id(SharedString::from(format!("security-badge-{key}")))
                    .text_xs()
                    .text_color(warn_fg)
                    .child(if b.security_warnings == 1 {
                        "1 security warning".to_owned()
                    } else {
                        format!("{} security warnings", b.security_warnings)
                    }),
            )
        })
}

#[cfg(test)]
mod tests {
    use gpui_kit::Hsla;
    use ninesql_core::{Color, ConnectionType};

    use super::{hsla, text_on};
    use crate::appearance::{MIN_TEXT_CONTRAST, contrast_ratio};

    #[test]
    fn builtin_type_badges_meet_aa_contrast() {
        for ty in ConnectionType::builtins() {
            let bg = hsla(ty.color);
            let ratio = contrast_ratio(text_on(bg), bg);
            assert!(ratio >= MIN_TEXT_CONTRAST, "{}: {ratio}", ty.name);
        }
    }

    #[test]
    fn dark_custom_colors_get_white_text() {
        let bg = hsla(Color::rgb(0x20, 0x20, 0x60));
        assert_eq!(text_on(bg), Hsla::white());
    }
}
