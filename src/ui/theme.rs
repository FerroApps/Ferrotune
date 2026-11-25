//! Single source of truth for the look (architecture §4). Colours and
//! metrics reproduce the dark Spotify desktop client; no Spotify assets
//! are used (Inter + Lucide instead of the proprietary font and icons).

use iced::border::Radius;
use iced::widget::{button, container, scrollable, slider, svg, text_input};
use iced::{Background, Border, Color, Font, Gradient, Shadow, Theme, Vector, font, gradient};

use crate::config::ThemeChoice;

pub const INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
pub const INTER_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");
pub const INTER_BOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-Bold.ttf");

pub const FONT: Font = Font::with_name("Inter");
pub const FONT_SEMIBOLD: Font = Font {
    weight: font::Weight::Semibold,
    ..FONT
};
pub const FONT_BOLD: Font = Font {
    weight: font::Weight::Bold,
    ..FONT
};

const fn rgb(hex: u32) -> Color {
    Color::from_rgb(
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
    )
}

const fn rgba(hex: u32, a: f32) -> Color {
    let c = rgb(hex);
    Color { a, ..c }
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg_app: Color,
    pub bg_sidebar: Color,
    pub bg_panel: Color,
    pub bg_elevated: Color,
    pub bg_hover: Color,
    pub bg_selected: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_muted: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub danger: Color,
    pub success: Color,
    pub border: Color,
    /// Chat apps only; unused here but kept for the shared skeleton.
    pub bubble_out: Color,
    pub bubble_in: Color,
    pub badge: Color,
    pub badge_muted: Color,
    // Ferrotune extras
    pub chip: Color,
    pub chip_hover: Color,
    pub row_hover: Color,
    pub row_selected: Color,
    pub rail: Color,
    pub toast: Color,
}

pub const DARK: Palette = Palette {
    bg_app: rgb(0x000000),
    bg_sidebar: rgb(0x121212),
    bg_panel: rgb(0x121212),
    bg_elevated: rgb(0x282828),
    bg_hover: rgb(0x1f1f1f),
    bg_selected: rgb(0x2a2a2a),
    text_primary: rgb(0xffffff),
    text_secondary: rgb(0xb3b3b3),
    text_muted: rgb(0x6a6a6a),
    accent: rgb(0x1ed760),
    accent_text: rgb(0x000000),
    danger: rgb(0xf3727f),
    success: rgb(0x1ed760),
    border: rgb(0x292929),
    bubble_out: rgb(0x1ed760),
    bubble_in: rgb(0x282828),
    badge: rgb(0x1ed760),
    badge_muted: rgb(0x535353),
    chip: rgba(0xffffff, 0.07),
    chip_hover: rgba(0xffffff, 0.1),
    row_hover: rgba(0xffffff, 0.1),
    row_selected: rgba(0xffffff, 0.3),
    rail: rgba(0xffffff, 0.3),
    toast: rgb(0x2e77d0),
};

/// The official client has no light theme for the app body; this light
/// palette exists for accessibility (FERRO_THEME=light).
pub const LIGHT: Palette = Palette {
    bg_app: rgb(0xe8e8e8),
    bg_sidebar: rgb(0xffffff),
    bg_panel: rgb(0xffffff),
    bg_elevated: rgb(0xf2f2f2),
    bg_hover: rgb(0xf0f0f0),
    bg_selected: rgb(0xe2e2e2),
    text_primary: rgb(0x121212),
    text_secondary: rgb(0x535353),
    text_muted: rgb(0x8a8a8a),
    accent: rgb(0x1db954),
    accent_text: rgb(0x000000),
    danger: rgb(0xd31225),
    success: rgb(0x1db954),
    border: rgb(0xdedede),
    bubble_out: rgb(0x1db954),
    bubble_in: rgb(0xf2f2f2),
    badge: rgb(0x1db954),
    badge_muted: rgb(0xb3b3b3),
    chip: rgba(0x000000, 0.06),
    chip_hover: rgba(0x000000, 0.1),
    row_hover: rgba(0x000000, 0.06),
    row_selected: rgba(0x000000, 0.14),
    rail: rgba(0x000000, 0.2),
    toast: rgb(0x2e77d0),
};

#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub gap: f32,
    pub panel_radius: f32,
    pub sidebar_width: f32,
    pub queue_width: f32,
    pub topbar_height: f32,
    pub player_height: f32,
    pub row_height: f32,
    pub art_row: f32,
    pub art_sidebar: f32,
    pub art_card: f32,
    pub art_header: f32,
    pub card_width: f32,
    pub font_xs: f32,
    pub font_sm: f32,
    pub font_md: f32,
    pub font_lg: f32,
    pub font_xl: f32,
    pub font_hero: f32,
}

pub const METRICS: Metrics = Metrics {
    gap: 8.0,
    panel_radius: 8.0,
    sidebar_width: 300.0,
    queue_width: 320.0,
    topbar_height: 64.0,
    player_height: 72.0,
    row_height: 56.0,
    art_row: 40.0,
    art_sidebar: 48.0,
    art_card: 156.0,
    art_header: 192.0,
    card_width: 180.0,
    font_xs: 11.0,
    font_sm: 13.0,
    font_md: 14.0,
    font_lg: 16.0,
    font_xl: 24.0,
    font_hero: 72.0,
};

#[derive(Debug, Clone, Copy)]
pub struct FerroTheme {
    pub palette: Palette,
    pub metrics: Metrics,
    pub dark: bool,
}

impl FerroTheme {
    pub fn new(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::Dark => Self {
                palette: DARK,
                metrics: METRICS,
                dark: true,
            },
            ThemeChoice::Light => Self {
                palette: LIGHT,
                metrics: METRICS,
                dark: false,
            },
        }
    }

    pub fn to_iced(self) -> Theme {
        let p = self.palette;
        Theme::custom(
            "Ferrotune",
            iced::theme::Palette {
                background: p.bg_app,
                text: p.text_primary,
                primary: p.accent,
                success: p.success,
                warning: rgb(0xffa42b),
                danger: p.danger,
            },
        )
    }
}

thread_local! {
    static CURRENT: std::cell::Cell<FerroTheme> = const { std::cell::Cell::new(FerroTheme { palette: DARK, metrics: METRICS, dark: true }) };
}

/// Select the active theme (called once at boot from the UI thread).
pub fn set_current(t: FerroTheme) {
    CURRENT.with(|c| c.set(t));
}

pub fn current() -> FerroTheme {
    CURRENT.with(std::cell::Cell::get)
}

pub fn pal() -> Palette {
    current().palette
}

pub const M: Metrics = METRICS;

// ------------------------------------------------------------ colour helpers

pub fn from_rgb8(c: [u8; 3]) -> Color {
    Color::from_rgb8(c[0], c[1], c[2])
}

/// Darken/desaturate an art colour the way the official headers do.
pub fn header_tint(c: [u8; 3]) -> Color {
    let col = from_rgb8(c);
    let max = col.r.max(col.g).max(col.b).max(0.01);
    // Normalise brightness into a pleasant mid range.
    let k = (0.55 / max).min(1.6);
    Color::from_rgb(
        (col.r * k).min(1.0),
        (col.g * k).min(1.0),
        (col.b * k).min(1.0),
    )
}

pub fn mix(a: Color, b: Color, t: f32) -> Color {
    Color::from_rgba(
        a.r + (b.r - a.r) * t,
        a.g + (b.g - a.g) * t,
        a.b + (b.b - a.b) * t,
        a.a + (b.a - a.a) * t,
    )
}

fn radius(r: f32) -> Radius {
    Radius::from(r)
}

// -------------------------------------------------------------- containers

pub fn app_bg(_: &Theme) -> container::Style {
    container::Style {
        background: Some(pal().bg_app.into()),
        text_color: Some(pal().text_primary),
        ..Default::default()
    }
}

pub fn panel(_: &Theme) -> container::Style {
    container::Style {
        background: Some(pal().bg_panel.into()),
        border: Border {
            radius: radius(M.panel_radius),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Vertical gradient from `top` into the panel colour.
pub fn gradient_header(top: Color, bottom: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Gradient(Gradient::Linear(
            gradient::Linear::new(std::f32::consts::PI)
                .add_stop(0.0, top)
                .add_stop(1.0, bottom),
        ))),
        border: Border {
            radius: Radius {
                top_left: M.panel_radius,
                top_right: M.panel_radius,
                bottom_right: 0.0,
                bottom_left: 0.0,
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn art_placeholder(round: bool, size: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(pal().bg_elevated.into()),
        border: Border {
            radius: radius(if round { size / 2.0 } else { 4.0 }),
            ..Default::default()
        },
        text_color: Some(pal().text_secondary),
        ..Default::default()
    }
}

/// The gradient tile used for "Liked Songs".
pub fn liked_tile(r: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Gradient(Gradient::Linear(
            gradient::Linear::new(std::f32::consts::FRAC_PI_4 * 3.0)
                .add_stop(0.0, rgb(0x450af5))
                .add_stop(1.0, rgb(0xc4efd9)),
        ))),
        border: Border {
            radius: radius(r),
            ..Default::default()
        },
        text_color: Some(Color::WHITE),
        ..Default::default()
    }
}

pub fn toast(_: &Theme) -> container::Style {
    container::Style {
        background: Some(pal().toast.into()),
        text_color: Some(Color::WHITE),
        border: Border {
            radius: radius(8.0),
            ..Default::default()
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.5),
            offset: Vector::new(0.0, 4.0),
            blur_radius: 16.0,
        },
        ..Default::default()
    }
}

pub fn banner(_: &Theme) -> container::Style {
    container::Style {
        background: Some(rgb(0x2a2a2a).into()),
        text_color: Some(pal().text_secondary),
        border: Border {
            radius: radius(4.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// "Playing on <device>" strip (Spotify Connect).
pub fn connect_strip(_: &Theme) -> container::Style {
    container::Style {
        background: Some(pal().accent.into()),
        text_color: Some(pal().accent_text),
        border: Border {
            radius: radius(4.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Spotify Soloist setup card (top of the main panel).
pub fn setup_card(_: &Theme) -> container::Style {
    let p = pal();
    container::Style {
        background: Some(p.bg_elevated.into()),
        text_color: Some(p.text_primary),
        border: Border {
            radius: radius(8.0),
            width: 1.0,
            color: mix(p.bg_elevated, p.accent, 0.35),
        },
        ..Default::default()
    }
}

/// "Ferrotune x.y.z is available" bar at the top of the window.
pub fn update_banner(_: &Theme) -> container::Style {
    let p = pal();
    container::Style {
        background: Some(mix(p.bg_elevated, p.accent, 0.22).into()),
        text_color: Some(p.text_primary),
        border: Border {
            radius: radius(6.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Persistent playback error banner.
pub fn error_banner(_: &Theme) -> container::Style {
    let p = pal();
    container::Style {
        background: Some(mix(p.bg_elevated, p.danger, 0.35).into()),
        text_color: Some(p.text_primary),
        border: Border {
            radius: radius(6.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn login_card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(pal().bg_panel.into()),
        border: Border {
            radius: radius(8.0),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn login_bg(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Gradient(Gradient::Linear(
            gradient::Linear::new(std::f32::consts::PI)
                .add_stop(0.0, rgb(0x2a2a2a))
                .add_stop(1.0, rgb(0x000000)),
        ))),
        text_color: Some(pal().text_primary),
        ..Default::default()
    }
}

pub fn avatar(_: &Theme) -> container::Style {
    container::Style {
        background: Some(rgb(0xf573a0).into()),
        text_color: Some(Color::BLACK),
        border: Border {
            radius: radius(16.0),
            width: 4.0,
            color: rgb(0x1f1f1f),
        },
        ..Default::default()
    }
}

pub fn separator(_: &Theme) -> container::Style {
    container::Style {
        background: Some(rgba(0xffffff, 0.1).into()),
        ..Default::default()
    }
}

// ----------------------------------------------------------------- buttons

fn btn(bg: Option<Color>, text: Color, r: f32) -> button::Style {
    button::Style {
        background: bg.map(Background::Color),
        text_color: text,
        border: Border {
            radius: radius(r),
            ..Default::default()
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Transparent icon button whose icon brightens on hover.
pub fn icon_button(_: &Theme, status: button::Status) -> button::Style {
    let p = pal();
    match status {
        button::Status::Hovered | button::Status::Pressed => btn(None, p.text_primary, 16.0),
        button::Status::Disabled => btn(None, p.text_muted, 16.0),
        button::Status::Active => btn(None, p.text_secondary, 16.0),
    }
}

/// Round dark button (top bar back/forward, home).
pub fn round_dark(_: &Theme, status: button::Status) -> button::Style {
    let p = pal();
    let bg = match status {
        button::Status::Hovered | button::Status::Pressed => p.bg_selected,
        _ => p.bg_hover,
    };
    let fg = if status == button::Status::Disabled {
        p.text_muted
    } else {
        p.text_primary
    };
    btn(Some(bg), fg, 999.0)
}

/// The white circular play/pause button in the player bar.
pub fn play_white(_: &Theme, status: button::Status) -> button::Style {
    let mut s = btn(Some(Color::WHITE), Color::BLACK, 999.0);
    if status == button::Status::Hovered {
        s.shadow = Shadow {
            color: rgba(0xffffff, 0.15),
            offset: Vector::ZERO,
            blur_radius: 6.0,
        };
    }
    s
}

/// The big green play button on headers.
pub fn play_green(_: &Theme, status: button::Status) -> button::Style {
    let p = pal();
    let bg = match status {
        button::Status::Hovered => rgb(0x3be477),
        _ => p.accent,
    };
    btn(Some(bg), p.accent_text, 999.0)
}

/// Filter chip (sidebar, search). `active` = selected.
pub fn chip(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let p = pal();
        if active {
            btn(Some(p.text_primary), p.bg_app, 999.0)
        } else {
            let bg = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
                p.chip_hover
            } else {
                p.chip
            };
            btn(Some(bg), p.text_primary, 999.0)
        }
    }
}

/// Flat list row (sidebar items, queue items, cards).
pub fn list_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let p = pal();
        let bg = if selected {
            Some(p.bg_selected)
        } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            Some(p.bg_hover)
        } else {
            None
        };
        btn(bg, p.text_primary, 6.0)
    }
}

pub fn card(_: &Theme, status: button::Status) -> button::Style {
    let p = pal();
    let bg = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        Some(p.bg_hover)
    } else {
        None
    };
    btn(bg, p.text_primary, 8.0)
}

/// Home "shortcut" tile.
pub fn shortcut(_: &Theme, status: button::Status) -> button::Style {
    let bg = if matches!(status, button::Status::Hovered | button::Status::Pressed) {
        rgba(0xffffff, 0.2)
    } else {
        rgba(0xffffff, 0.1)
    };
    btn(Some(bg), pal().text_primary, 4.0)
}

/// Underlined-on-hover text link (artist names in rows).
pub fn link(muted: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let p = pal();
        let c = if matches!(status, button::Status::Hovered) {
            p.text_primary
        } else if muted {
            p.text_secondary
        } else {
            p.text_primary
        };
        let mut s = btn(None, c, 0.0);
        s.snap = true;
        s
    }
}

pub fn pill_primary(_: &Theme, status: button::Status) -> button::Style {
    let p = pal();
    let bg = if status == button::Status::Hovered {
        rgb(0x3be477)
    } else {
        p.accent
    };
    btn(Some(bg), p.accent_text, 999.0)
}

pub fn pill_outline(_: &Theme, status: button::Status) -> button::Style {
    let p = pal();
    let mut s = btn(None, p.text_primary, 999.0);
    s.border = Border {
        radius: radius(999.0),
        width: 1.0,
        color: if status == button::Status::Hovered {
            p.text_primary
        } else {
            rgb(0x7c7c7c)
        },
    };
    s
}

pub fn track_row(
    hovered: bool,
    selected: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, _status| {
        let p = pal();
        let bg = if selected {
            Some(p.row_selected)
        } else if hovered {
            Some(p.row_hover)
        } else {
            None
        };
        btn(bg, p.text_primary, 4.0)
    }
}

// ------------------------------------------------------------------ inputs

pub fn search_input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let p = pal();
    let (bg, border) = match status {
        text_input::Status::Focused { .. } => (
            rgb(0x2a2a2a),
            Border {
                radius: radius(999.0),
                width: 2.0,
                color: Color::WHITE,
            },
        ),
        text_input::Status::Hovered => (
            rgb(0x2a2a2a),
            Border {
                radius: radius(999.0),
                width: 1.0,
                color: rgb(0x535353),
            },
        ),
        _ => (
            p.bg_hover,
            Border {
                radius: radius(999.0),
                width: 1.0,
                color: Color::TRANSPARENT,
            },
        ),
    };
    text_input::Style {
        background: bg.into(),
        border,
        icon: p.text_secondary,
        placeholder: rgb(0x9a9a9a),
        value: p.text_primary,
        selection: rgba(0x1ed760, 0.4),
    }
}

/// Seek / volume bar: white fill, green on hover, thin grey rail.
pub fn progress_slider(_: &Theme, status: slider::Status) -> slider::Style {
    let p = pal();
    let active = matches!(status, slider::Status::Hovered | slider::Status::Dragged);
    let fill = if active { p.accent } else { p.text_primary };
    slider::Style {
        rail: slider::Rail {
            backgrounds: (fill.into(), p.rail.into()),
            width: 4.0,
            border: Border {
                radius: radius(2.0),
                ..Default::default()
            },
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle {
                radius: if active { 6.0 } else { 0.0 },
            },
            background: Color::WHITE.into(),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

pub fn thin_scroll(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut s = scrollable::default(theme, status);
    let hovered = matches!(
        status,
        scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. }
    );
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: if hovered {
                rgba(0xffffff, 0.3).into()
            } else {
                Color::TRANSPARENT.into()
            },
            border: Border {
                radius: radius(0.0),
                ..Default::default()
            },
        },
    };
    s.vertical_rail = rail;
    s.horizontal_rail = rail;
    s
}

pub fn svg_color(c: Color) -> impl Fn(&Theme, svg::Status) -> svg::Style {
    move |_, _| svg::Style { color: Some(c) }
}

/// Icon colour that follows the parent button's text colour on hover.
pub fn svg_hover(normal: Color, hover: Color) -> impl Fn(&Theme, svg::Status) -> svg::Style {
    move |_, status| svg::Style {
        color: Some(if status == svg::Status::Hovered {
            hover
        } else {
            normal
        }),
    }
}

// ------------------------------------------------------- menus and dialogs

/// Context menu width and row height.
pub const MENU_WIDTH: f32 = 240.0;
pub const MENU_ROW: f32 = 36.0;

pub fn menu_panel(_: &Theme) -> container::Style {
    container::Style {
        background: Some(pal().bg_elevated.into()),
        text_color: Some(pal().text_primary),
        border: Border {
            radius: radius(6.0),
            width: 1.0,
            color: pal().border,
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.6),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        ..Default::default()
    }
}

pub fn menu_item(_: &Theme, status: button::Status) -> button::Style {
    let p = pal();
    let bg =
        matches!(status, button::Status::Hovered | button::Status::Pressed).then_some(p.row_hover);
    btn(bg, p.text_primary, 3.0)
}

/// Dimmed backdrop behind a dialog.
pub fn scrim(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.7).into()),
        ..Default::default()
    }
}

pub fn dialog_card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(pal().bg_elevated.into()),
        text_color: Some(pal().text_primary),
        border: Border {
            radius: radius(8.0),
            ..Default::default()
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.6),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 32.0,
        },
        ..Default::default()
    }
}

/// Drop target highlight while dragging a playlist row.
pub fn drop_line(_: &Theme) -> container::Style {
    container::Style {
        background: Some(pal().accent.into()),
        ..Default::default()
    }
}

/// A keyboard key in the shortcuts dialog.
pub fn keycap(_: &Theme) -> container::Style {
    container::Style {
        background: Some(pal().chip.into()),
        border: Border {
            radius: radius(4.0),
            width: 1.0,
            color: pal().border,
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_tint_is_bounded() {
        let c = header_tint([10, 10, 10]);
        assert!(c.r <= 1.0 && c.g <= 1.0 && c.b <= 1.0);
        let bright = header_tint([255, 255, 255]);
        assert!(bright.r < 0.6, "very bright art is toned down");
        let m = mix(Color::BLACK, Color::WHITE, 0.5);
        assert!((m.r - 0.5).abs() < 1e-6);
    }
}
