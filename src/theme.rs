use egui::{Color32, Context, Theme, ThemePreference, Visuals};

use crate::i18n::t;

pub fn label(theme: ThemePreference) -> &'static str {
    match theme {
        ThemePreference::System => t("Follow system"),
        ThemePreference::Light => t("Light"),
        ThemePreference::Dark => t("Dark"),
    }
}

pub fn install(ctx: &Context, preference: ThemePreference) {
    for theme in [Theme::Light, Theme::Dark] {
        let mut visuals = theme.default_visuals();
        let dark = visuals.dark_mode;
        let tone = |light, night| if dark { night } else { light };
        visuals.panel_fill = tone(
            Color32::from_rgb(244, 247, 250),
            Color32::from_rgb(20, 33, 51),
        );
        visuals.window_fill = tone(Color32::WHITE, Color32::from_rgb(27, 44, 64));
        visuals.extreme_bg_color = tone(Color32::WHITE, Color32::from_rgb(13, 24, 38));
        visuals.faint_bg_color = tone(
            Color32::from_rgb(234, 240, 245),
            Color32::from_rgb(27, 42, 61),
        );
        visuals.selection.bg_fill = tone(
            Color32::from_rgb(180, 230, 222),
            Color32::from_rgb(32, 88, 85),
        );
        visuals.selection.stroke.color = tone(
            Color32::from_rgb(17, 91, 83),
            Color32::from_rgb(163, 241, 228),
        );
        visuals.hyperlink_color = tone(
            Color32::from_rgb(9, 105, 95),
            Color32::from_rgb(106, 220, 202),
        );
        visuals.warn_fg_color = tone(
            Color32::from_rgb(130, 84, 0),
            Color32::from_rgb(249, 197, 119),
        );
        visuals.error_fg_color = tone(
            Color32::from_rgb(169, 45, 64),
            Color32::from_rgb(255, 156, 157),
        );
        for (widget, fill) in [
            (
                &mut visuals.widgets.inactive,
                tone(
                    Color32::from_rgb(228, 235, 240),
                    Color32::from_rgb(38, 57, 76),
                ),
            ),
            (
                &mut visuals.widgets.hovered,
                tone(
                    Color32::from_rgb(203, 227, 224),
                    Color32::from_rgb(44, 73, 76),
                ),
            ),
            (
                &mut visuals.widgets.active,
                tone(
                    Color32::from_rgb(180, 218, 212),
                    Color32::from_rgb(48, 92, 90),
                ),
            ),
        ] {
            widget.bg_fill = fill;
            widget.weak_bg_fill = fill;
        }
        ctx.set_visuals_of(theme, visuals);
    }
    ctx.set_theme(preference);
}

pub fn success_color(visuals: &Visuals) -> Color32 {
    if visuals.dark_mode {
        Color32::from_rgb(128, 222, 178)
    } else {
        Color32::from_rgb(23, 103, 75)
    }
}
