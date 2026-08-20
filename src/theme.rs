//! Fonts, colour and the metrics the window is laid out on.
//!
//! Shared with the accessengine app, so the two look like they come from the
//! same place. The reasoning carries over intact: the bundled bold face,
//! because at these sizes a light weight is the single biggest readability cost
//! and egui ships only Ubuntu-Light; and a palette written down as explicit
//! pairs so the contrast ratios can be reasoned about instead of inherited —
//! every text colour below reaches at least 4.5:1 against the surface it is
//! drawn on.
//!
//! Both themes are defined; egui follows the operating system's appearance, so
//! a user who runs their machine in dark mode gets a dark app without asking.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Stroke, Theme, Visuals};

/// Ubuntu Bold, bundled so the app looks and reads the same on a fresh Windows
/// install as it does on a Mac.
const UBUNTU_BOLD: &[u8] = include_bytes!("../assets/fonts/Ubuntu-Bold.ttf");

/// Height of a control. Comfortably above the 44px touch/pointer target advice
/// once the surrounding item spacing is counted.
pub const CONTROL_HEIGHT: f32 = 34.0;

/// Height of the progress bar under a running download. Tall enough to hold the
/// percentage written across it: egui clips that text to the bar, so a bar
/// shorter than the line it contains shows a percentage with its head and feet
/// cut off.
pub const PROGRESS_HEIGHT: f32 = 24.0;

/// The percentage across the progress bar, and the outline drawn under it.
///
/// The same yellow on both themes, because the surface behind it is the bar
/// rather than the page. It needs the outline because that surface is two
/// surfaces: the text sits over the empty track while the job is less than half
/// done and over the filled part after that, and no single colour is legible on
/// both the white track of the light theme and the blue fill. Outlined, the
/// yellow clears 4.5:1 on either.
pub const PROGRESS_TEXT: Color32 = Color32::from_rgb(255, 209, 74);
pub const PROGRESS_TEXT_OUTLINE: Color32 = Color32::from_rgb(10, 12, 16);

/// The colours that change meaning between light and dark. Held as a struct so
/// a call site asks for "the error colour" and gets one that is legible on the
/// surface it is actually drawing on.
#[derive(Clone, Copy)]
pub struct Palette {
    /// Success / completed.
    pub ok: Color32,
    /// Needs attention but nothing has failed.
    pub warn: Color32,
    /// Something failed.
    pub bad: Color32,
    /// Supporting text. Still 4.5:1 — "muted" here means lower saturation, not
    /// lower contrast, because greyed-out text is where accessibility usually
    /// goes wrong.
    pub muted: Color32,
    /// Focus ring and selection.
    pub accent: Color32,
}

/// 4.5:1 or better against `#FFFFFF` and the panel fill below.
const LIGHT: Palette = Palette {
    ok: Color32::from_rgb(0, 100, 45),
    warn: Color32::from_rgb(133, 77, 0),
    bad: Color32::from_rgb(176, 27, 27),
    muted: Color32::from_rgb(84, 92, 102),
    accent: Color32::from_rgb(11, 87, 164),
};

/// 4.5:1 or better against the dark panel and window fills below.
const DARK: Palette = Palette {
    ok: Color32::from_rgb(109, 219, 133),
    warn: Color32::from_rgb(240, 187, 64),
    bad: Color32::from_rgb(255, 138, 128),
    muted: Color32::from_rgb(176, 186, 197),
    accent: Color32::from_rgb(124, 187, 255),
};

/// The palette matching whichever theme is currently in force.
pub fn palette(visuals: &Visuals) -> Palette {
    if visuals.dark_mode { DARK } else { LIGHT }
}

/// Installs Ubuntu Bold as the default proportional face.
///
/// `RichText::strong()` only recolours; a heavier weight has to arrive as a real
/// font. Putting it first in the `Proportional` chain means every widget picks
/// it up without each call site asking. Everything egui already had stays behind
/// it, so a glyph Ubuntu Bold doesn't cover still renders instead of becoming a
/// tofu box — which is what keeps the `▶`, `📁` and `⚙` in the labels working.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "Ubuntu-Bold".to_owned(),
        std::sync::Arc::new(FontData::from_static(UBUNTU_BOLD)),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Ubuntu-Bold".to_owned());
    ctx.set_fonts(fonts);
}

fn light_visuals() -> Visuals {
    let mut visuals = Visuals::light();
    let text = Color32::from_rgb(18, 22, 28);

    visuals.panel_fill = Color32::from_rgb(244, 246, 249);
    visuals.window_fill = Color32::WHITE;
    visuals.extreme_bg_color = Color32::WHITE;
    visuals.faint_bg_color = Color32::from_rgb(236, 239, 243);
    visuals.hyperlink_color = LIGHT.accent;
    visuals.warn_fg_color = LIGHT.warn;
    visuals.error_fg_color = LIGHT.bad;
    visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(150, 158, 168));

    // Control surfaces: white fills with a stroke dark enough to be a real
    // boundary rather than a suggestion, which is what a low-vision user needs
    // in order to see where one field ends and the next begins.
    visuals.widgets.noninteractive.bg_fill = visuals.panel_fill;
    visuals.widgets.noninteractive.weak_bg_fill = visuals.panel_fill;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(170, 178, 188));
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.inactive.bg_fill = Color32::WHITE;
    visuals.widgets.inactive.weak_bg_fill = Color32::WHITE;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.5, Color32::from_rgb(96, 105, 116));
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(228, 238, 250);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(228, 238, 250);
    visuals.widgets.hovered.bg_stroke = Stroke::new(2.0, LIGHT.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, text);

    // `active` is also what egui uses for the keyboard-focused widget, so this
    // is the focus ring. It is deliberately the loudest thing on screen.
    visuals.widgets.active.bg_fill = Color32::from_rgb(214, 231, 249);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(214, 231, 249);
    visuals.widgets.active.bg_stroke = Stroke::new(3.0, LIGHT.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, Color32::from_rgb(8, 12, 18));

    visuals.widgets.open.bg_fill = Color32::WHITE;
    visuals.widgets.open.weak_bg_fill = Color32::WHITE;
    visuals.widgets.open.bg_stroke = Stroke::new(2.0, LIGHT.accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, text);

    visuals.selection.bg_fill = LIGHT.accent;
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    visuals
}

fn dark_visuals() -> Visuals {
    let mut visuals = Visuals::dark();
    let text = Color32::from_rgb(240, 244, 249);

    visuals.panel_fill = Color32::from_rgb(20, 24, 31);
    visuals.window_fill = Color32::from_rgb(28, 33, 41);
    visuals.extreme_bg_color = Color32::from_rgb(13, 16, 21);
    visuals.faint_bg_color = Color32::from_rgb(32, 38, 47);
    visuals.hyperlink_color = DARK.accent;
    visuals.warn_fg_color = DARK.warn;
    visuals.error_fg_color = DARK.bad;
    visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(96, 106, 118));

    visuals.widgets.noninteractive.bg_fill = visuals.panel_fill;
    visuals.widgets.noninteractive.weak_bg_fill = visuals.panel_fill;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(88, 98, 110));
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.inactive.bg_fill = Color32::from_rgb(38, 45, 55);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(38, 45, 55);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.5, Color32::from_rgb(140, 152, 166));
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(48, 60, 76);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(48, 60, 76);
    visuals.widgets.hovered.bg_stroke = Stroke::new(2.0, DARK.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, text);

    visuals.widgets.active.bg_fill = Color32::from_rgb(58, 74, 94);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(58, 74, 94);
    visuals.widgets.active.bg_stroke = Stroke::new(3.0, DARK.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);

    visuals.widgets.open.bg_fill = Color32::from_rgb(38, 45, 55);
    visuals.widgets.open.weak_bg_fill = Color32::from_rgb(38, 45, 55);
    visuals.widgets.open.bg_stroke = Stroke::new(2.0, DARK.accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, text);

    visuals.selection.bg_fill = Color32::from_rgb(31, 92, 156);
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    visuals
}

/// Applies fonts, both palettes and the spacing. Rebuilding the glyph atlas is
/// expensive, so this runs once, from the constructor — never per frame.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_visuals_of(Theme::Light, light_visuals());
    ctx.set_visuals_of(Theme::Dark, dark_visuals());

    ctx.all_styles_mut(|style| {
        // Generous by egui's standards. Everything on screen is either a control
        // the user has to hit or a row they have to read and tell apart from its
        // neighbour.
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);
        style.spacing.button_padding = egui::vec2(12.0, 8.0);
        style.spacing.interact_size.y = CONTROL_HEIGHT;
        style.spacing.scroll.bar_width = 12.0;

        // Square-ish. Large radii blur the boundary between a control and its
        // background, which is exactly the edge low-vision users rely on.
        for widget in [
            &mut style.visuals.widgets.noninteractive,
            &mut style.visuals.widgets.inactive,
            &mut style.visuals.widgets.hovered,
            &mut style.visuals.widgets.active,
            &mut style.visuals.widgets.open,
        ] {
            widget.corner_radius = CornerRadius::same(3);
        }

        for (text_style, size) in [
            (egui::TextStyle::Heading, 22.0),
            (egui::TextStyle::Body, 15.0),
            (egui::TextStyle::Button, 15.0),
            (egui::TextStyle::Small, 13.0),
        ] {
            if let Some(font) = style.text_styles.get_mut(&text_style) {
                font.size = size;
            }
        }

        // egui's default is 60% alpha, which drops supporting text below 4.5:1
        // on both themes. Weak text here is a shade, not a whisper.
        style.visuals.weak_text_alpha = 0.85;

        // And the same argument for anything switched off. egui's default of
        // 0.5 takes a podcast title on the light theme from 16.8:1 down to
        // 3.4:1 — under the 4.5:1 floor — and every title in the list is
        // switched off for as long as a run lasts, because the title is its
        // tick box's label. That is precisely the stretch during which somebody
        // is reading the list to see where the run has got to. At 0.75 the same
        // title measures 7.7:1 on the light theme and 9.4:1 on the dark, and it
        // still reads as plainly unavailable.
        style.visuals.disabled_alpha = 0.75;
    });
}

/// Writes the percentage across the middle of a progress bar.
///
/// Painted here rather than left to `ProgressBar::show_percentage`, which puts
/// it against the left edge in the selection colour — white on both themes, so
/// on the white track of the light theme the first tenth of every job is a
/// percentage nobody can read. Centred, it is also where the eye already is.
///
/// The outline underneath is what makes one colour work over both the empty
/// track and the filled part; see [`PROGRESS_TEXT`].
pub fn percentage_across(ui: &egui::Ui, bar: egui::Rect, progress: f32) {
    let text = format!("{}%", (progress * 100.0).round() as u32);
    let font = egui::TextStyle::Button.resolve(ui.style());
    let painter = ui.painter().with_clip_rect(bar);
    for offset in [
        egui::vec2(-1.0, -1.0),
        egui::vec2(1.0, -1.0),
        egui::vec2(-1.0, 1.0),
        egui::vec2(1.0, 1.0),
    ] {
        painter.text(
            bar.center() + offset,
            egui::Align2::CENTER_CENTER,
            &text,
            font.clone(),
            PROGRESS_TEXT_OUTLINE,
        );
    }
    painter.text(
        bar.center(),
        egui::Align2::CENTER_CENTER,
        &text,
        font,
        PROGRESS_TEXT,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG's relative luminance, and the contrast between two opaque colours.
    fn contrast(a: Color32, b: Color32) -> f32 {
        fn channel(value: u8) -> f32 {
            let value = value as f32 / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        fn luminance(c: Color32) -> f32 {
            0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
        }
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// What a colour becomes when egui fades it for a switched-off widget.
    fn faded(fg: Color32, bg: Color32, alpha: f32) -> Color32 {
        let mix = |f: u8, b: u8| (alpha * f as f32 + (1.0 - alpha) * b as f32).round() as u8;
        Color32::from_rgb(mix(fg.r(), bg.r()), mix(fg.g(), bg.g()), mix(fg.b(), bg.b()))
    }

    /// The claim at the top of this module, checked rather than asserted in
    /// prose: every colour that carries meaning is legible on every surface it
    /// is drawn on, in both themes.
    #[test]
    fn every_palette_colour_clears_the_contrast_floor() {
        let cases: [(&str, Palette, Color32, &[Color32]); 2] = [
            (
                "light",
                LIGHT,
                Color32::from_rgb(18, 22, 28),
                &[
                    Color32::WHITE,
                    Color32::from_rgb(244, 246, 249),
                    Color32::from_rgb(236, 239, 243),
                ],
            ),
            (
                "dark",
                DARK,
                Color32::from_rgb(240, 244, 249),
                &[
                    Color32::from_rgb(20, 24, 31),
                    Color32::from_rgb(28, 33, 41),
                    Color32::from_rgb(13, 16, 21),
                    Color32::from_rgb(32, 38, 47),
                    Color32::from_rgb(38, 45, 55),
                ],
            ),
        ];

        for (theme, palette, text, surfaces) in cases {
            let colours = [
                ("ok", palette.ok),
                ("warn", palette.warn),
                ("bad", palette.bad),
                ("muted", palette.muted),
                ("accent", palette.accent),
                ("text", text),
            ];
            for (name, colour) in colours {
                for surface in surfaces {
                    let ratio = contrast(colour, *surface);
                    assert!(
                        ratio >= 4.5,
                        "{theme} {name} is {ratio:.2}:1 on {surface:?}, under the 4.5:1 floor"
                    );
                }
            }
        }
    }

    /// A run switches every podcast's tick box off, and the podcast's name is
    /// that tick box's label — so the whole list is drawn faded for as long as
    /// the run lasts. egui's own default for that fade puts the light theme
    /// under the floor; this is the value that keeps it over.
    #[test]
    fn a_switched_off_row_is_still_readable() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let alpha = ctx.style_of(Theme::Light).visuals.disabled_alpha;
        assert_eq!(alpha, ctx.style_of(Theme::Dark).visuals.disabled_alpha);

        for (theme, text, panel) in [
            ("light", Color32::from_rgb(18, 22, 28), Color32::from_rgb(244, 246, 249)),
            ("dark", Color32::from_rgb(240, 244, 249), Color32::from_rgb(20, 24, 31)),
        ] {
            let ratio = contrast(faded(text, panel, alpha), panel);
            assert!(
                ratio >= 4.5,
                "{theme} titles are {ratio:.2}:1 while a run is going, under the 4.5:1 floor"
            );
        }

        // And egui's default is genuinely the thing being corrected, so this
        // test fails for the right reason if the override is ever dropped.
        let ratio = contrast(
            faded(
                Color32::from_rgb(18, 22, 28),
                Color32::from_rgb(244, 246, 249),
                0.5,
            ),
            Color32::from_rgb(244, 246, 249),
        );
        assert!(ratio < 4.5, "egui's default no longer needs overriding");
    }
}
