//! Settings screen (avatar menu → Settings), laid out like the official
//! client's: section titles, a label on the left, the control on the right.

use iced::widget::{button, column, container, row, text_input, toggler};
use iced::{Alignment, Length, Padding};

use super::theme::{self, FONT, FONT_BOLD, M, pal};
use super::widgets::{El, bold, fill, label, secondary};
use crate::app::settings::{SettingChange, quality_label};
use crate::app::{App, Ui, ui};
use crate::config::{Bitrate, ThemeChoice};

fn section<'a>(title: &'a str, rows: Vec<El<'a>>) -> El<'a> {
    let mut col = column![bold(title, M.font_lg)].spacing(12);
    for r in rows {
        col = col.push(r);
    }
    container(col).padding(Padding::from([12, 0])).into()
}

fn setting_row<'a>(title: &'a str, hint: Option<&'a str>, control: El<'a>) -> El<'a> {
    let mut text = column![label(title, M.font_md, pal().text_primary, FONT)].spacing(2);
    if let Some(h) = hint {
        text = text.push(secondary(h, M.font_sm));
    }
    row![container(text).width(Length::Fill), control]
        .spacing(16)
        .align_y(Alignment::Center)
        .into()
}

fn switch<'a>(on: bool, f: fn(bool) -> SettingChange) -> El<'a> {
    toggler(on)
        .on_toggle(move |v| ui(Ui::Setting(f(v))))
        .size(20)
        .into()
}

fn chips<'a, T: Copy + PartialEq + 'a>(
    current: T,
    options: &[(T, &'a str)],
    f: fn(T) -> SettingChange,
) -> El<'a> {
    let mut r = row![].spacing(8);
    for (v, name) in options {
        let active = *v == current;
        r = r.push(
            button(label(
                *name,
                M.font_sm,
                if active {
                    pal().bg_app
                } else {
                    pal().text_primary
                },
                FONT,
            ))
            .padding(Padding::from([6, 12]))
            .style(theme::chip(active))
            .on_press(ui(Ui::Setting(f(*v)))),
        );
    }
    r.into()
}

fn format_bytes(n: u64) -> String {
    let mb = n as f64 / (1024.0 * 1024.0);
    if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{:.0} kB", n as f64 / 1024.0)
    }
}

pub fn view(app: &App) -> El<'_> {
    let s = &app.settings;
    let audio = section(
        "Audio quality",
        vec![
            setting_row(
                "Streaming quality",
                Some("Higher quality uses more data."),
                chips(
                    s.bitrate,
                    &[
                        (Bitrate::Low, quality_label(Bitrate::Low)),
                        (Bitrate::Normal, quality_label(Bitrate::Normal)),
                        (Bitrate::High, quality_label(Bitrate::High)),
                    ],
                    SettingChange::Quality,
                ),
            ),
            setting_row(
                "Normalize volume",
                Some("Set the same volume level for all songs."),
                switch(s.normalisation, SettingChange::Normalisation),
            ),
        ],
    );
    let mut playback_rows = super::soloist::settings_rows(
        app,
        setting_row,
        toggler(app.soloist_enabled())
            .on_toggle(|v| ui(Ui::Soloist(crate::app::soloist::SoloistUi::Enable(v))))
            .size(20)
            .into(),
    );
    playback_rows.extend([
            setting_row(
                "Gapless playback",
                Some("Start the next song without a pause. (Crossfade is not supported.)"),
                switch(s.gapless, SettingChange::Gapless),
            ),
            setting_row(
                "Autoplay",
                Some("Keep playing similar songs when your music ends (Spotify Connect)."),
                switch(s.autoplay, SettingChange::Autoplay),
            ),
            setting_row(
                "Play on this computer",
                Some(
                    "When off, or when Spotify does not allow playback here, music plays on your active Spotify device.",
                ),
                switch(s.play_local, SettingChange::PlayLocal),
            ),
    ]);
    let playback = section("Playback", playback_rows);
    let name = text_input(&app.config.device_name, &s.device_name)
        .on_input(|v| ui(Ui::Setting(SettingChange::DeviceName(v))))
        .on_submit(ui(Ui::Setting(SettingChange::DeviceNameCommit)))
        .padding(Padding::from([8, 12]))
        .size(M.font_md)
        .width(260)
        .style(theme::search_input);
    let connect = section(
        "Spotify Connect",
        vec![
            setting_row(
                "Show this computer to other devices",
                Some("Lets your phone play music here."),
                switch(s.connect, SettingChange::Connect),
            ),
            setting_row("Device name", Some("Press Enter to save."), name.into()),
        ],
    );
    let display = section(
        "Display",
        vec![setting_row(
            "Theme",
            None,
            chips(
                s.theme,
                &[(ThemeChoice::Dark, "Dark"), (ThemeChoice::Light, "Light")],
                SettingChange::Theme,
            ),
        )],
    );
    let window = section(
        "Window",
        vec![
            setting_row(
                "Close button keeps Ferrotune running in the tray",
                Some("Music keeps playing. Without a system tray, closing quits."),
                switch(s.close_to_tray, SettingChange::CloseToTray),
            ),
            setting_row(
                "Start minimized to tray",
                Some("Open with only the tray icon, no window."),
                switch(s.start_minimized, SettingChange::StartMinimized),
            ),
            setting_row(
                "Open Ferrotune at login",
                Some("Adds an autostart entry that starts minimized."),
                switch(app.autostart, SettingChange::Autostart),
            ),
        ],
    );
    let updates = section("Updates", super::update::settings_rows(app, setting_row));
    let size = app
        .cache_bytes
        .map(format_bytes)
        .unwrap_or_else(|| "…".into());
    let storage = section(
        "Storage",
        vec![setting_row(
            "Cache",
            None,
            row![
                secondary(size, M.font_sm),
                button(label(
                    "Clear cache",
                    M.font_sm,
                    pal().text_primary,
                    FONT_BOLD
                ))
                .padding(Padding::from([6, 14]))
                .style(theme::pill_outline)
                .on_press(ui(Ui::Setting(SettingChange::ClearCache))),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into(),
        )],
    );
    let account = section(
        "Account",
        vec![setting_row(
            "Log out",
            Some("Removes the saved login from the keyring and clears the cache."),
            button(label("Log out", M.font_sm, pal().text_primary, FONT_BOLD))
                .padding(Padding::from([6, 14]))
                .style(theme::pill_outline)
                .on_press(ui(Ui::Logout))
                .into(),
        )],
    );
    container(
        column![
            row![bold("Settings", 32.0), fill()],
            audio,
            playback,
            connect,
            display,
            window,
            updates,
            storage,
            account,
        ]
        .spacing(8)
        .max_width(860),
    )
    .padding(Padding::from([24, 32]))
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn bytes_format() {
        assert_eq!(super::format_bytes(100), "0 kB");
        assert_eq!(super::format_bytes(3 * 1024 * 1024 / 2), "1.5 MB");
    }
}
