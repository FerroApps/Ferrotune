//! Update banner (top of the window) and Settings → Updates.

use iced::widget::{button, container, row, toggler};
use iced::{Alignment, Length, Padding};

use super::icons::Icon;
use super::theme::{self, FONT, FONT_BOLD, M, pal};
use super::widgets::{El, icon, label, secondary};
use crate::app::updates::{CheckStatus, Phase, UpdateUi};
use crate::app::{App, Message, Ui, ui};
use crate::config::APP_NAME;
use crate::update::install::Stage;
use crate::update::{self, InstallMethod};

fn msg(u: UpdateUi) -> Message {
    ui(Ui::Update(u))
}

fn primary_btn<'a>(text: &'a str, m: Message) -> El<'a> {
    button(label(text, M.font_sm, pal().accent_text, FONT_BOLD))
        .padding(Padding::from([5, 14]))
        .style(theme::pill_primary)
        .on_press(m)
        .into()
}

fn outline_btn<'a>(text: &'a str, m: Message) -> El<'a> {
    button(label(text, M.font_sm, pal().text_primary, FONT_BOLD))
        .padding(Padding::from([5, 14]))
        .style(theme::pill_outline)
        .on_press(m)
        .into()
}

fn close_btn<'a>(m: Message) -> El<'a> {
    button(icon(Icon::Close, 14.0, pal().text_secondary))
        .padding(6)
        .style(theme::icon_button)
        .on_press(m)
        .into()
}

/// "Downloading 42%" and friends.
pub fn stage_text(s: Stage) -> String {
    match s {
        Stage::Downloading {
            received,
            total: Some(t),
        } if t > 0 => format!(
            "Downloading… {}%",
            (received.saturating_mul(100) / t).min(100)
        ),
        Stage::Downloading { received, .. } => {
            format!("Downloading… {:.1} MB", received as f64 / 1_048_576.0)
        }
        Stage::Verifying => "Verifying…".into(),
        Stage::Installing => "Installing…".into(),
    }
}

/// The slim bar shown at the top while an update is on offer.
pub fn banner(app: &App) -> Option<El<'_>> {
    let u = &app.updater;
    let version = u.banner_version()?;
    let mut actions = row![].spacing(8).align_y(Alignment::Center);
    let text: String = match &u.phase {
        Phase::Working(s) => {
            if matches!(s, Stage::Downloading { .. }) {
                actions = actions.push(outline_btn("Cancel", msg(UpdateUi::Cancel)));
            }
            format!("Updating to {APP_NAME} {version}. {}", stage_text(*s))
        }
        Phase::Installed(i) => {
            actions = actions
                .push(primary_btn("Restart now", msg(UpdateUi::Restart)))
                .push(outline_btn("Later", msg(UpdateUi::Later)));
            format!("{APP_NAME} {} is installed. Restart to finish.", i.version)
        }
        Phase::Failed(e) => {
            actions = actions
                .push(outline_btn(
                    "Open release page",
                    msg(UpdateUi::OpenReleaseNotes),
                ))
                .push(close_btn(msg(UpdateUi::Dismiss)));
            format!("Update failed: {e}")
        }
        Phase::Idle => {
            match &u.method {
                Some(InstallMethod::InstallScript(_)) => {
                    actions = actions
                        .push(primary_btn("Update", msg(UpdateUi::Install)))
                        .push(outline_btn(
                            "Release notes",
                            msg(UpdateUi::OpenReleaseNotes),
                        ));
                }
                Some(InstallMethod::Cargo) => {
                    actions = actions
                        .push(primary_btn("Copy command", msg(UpdateUi::CopyCommand)))
                        .push(outline_btn(
                            "Release notes",
                            msg(UpdateUi::OpenReleaseNotes),
                        ));
                }
                _ => {
                    actions = actions.push(primary_btn(
                        "Open release page",
                        msg(UpdateUi::OpenReleaseNotes),
                    ));
                }
            }
            actions = actions.push(close_btn(msg(UpdateUi::Dismiss)));
            match &u.method {
                Some(InstallMethod::Cargo) => format!(
                    "{APP_NAME} {version} is available. Installed with cargo: run {}",
                    update::cargo_command()
                ),
                _ => format!("{APP_NAME} {version} is available"),
            }
        }
    };
    let busy = matches!(u.phase, Phase::Working(_));
    Some(
        container(
            row![
                icon(
                    if busy { Icon::Loader } else { Icon::ArrowRight },
                    14.0,
                    pal().text_primary
                ),
                label(text, M.font_sm, pal().text_primary, FONT)
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill),
                actions,
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        )
        .padding(Padding::from([4, 12]))
        .width(Length::Fill)
        .style(theme::update_banner)
        .into(),
    )
}

/// Rows of Settings → Updates, built with the settings screen's row helper.
pub fn settings_rows<'a>(
    app: &'a App,
    setting_row: fn(&'a str, Option<&'a str>, El<'a>) -> El<'a>,
) -> Vec<El<'a>> {
    let u = &app.updater;
    let mut rows = vec![setting_row(
        "Version",
        None,
        secondary(format!("{APP_NAME} {}", update::CURRENT_VERSION), M.font_sm).into(),
    )];

    let status: String = if u.checking {
        "Checking…".into()
    } else {
        match &u.last_check {
            None => String::new(),
            Some(CheckStatus::UpToDate) => format!("{APP_NAME} is up to date."),
            Some(CheckStatus::Available(v)) => format!("{APP_NAME} {v} is available."),
            Some(CheckStatus::Failed(e)) => e.clone(),
        }
    };
    let mut check = row![secondary(status, M.font_sm)]
        .spacing(12)
        .align_y(Alignment::Center);
    let mut check_btn = button(label(
        "Check for updates",
        M.font_sm,
        pal().text_primary,
        FONT_BOLD,
    ))
    .padding(Padding::from([6, 14]))
    .style(theme::pill_outline);
    if !u.checking && !u.busy() {
        check_btn = check_btn.on_press(msg(UpdateUi::Check));
    }
    check = check.push(check_btn);
    rows.push(setting_row("Updates", None, check.into()));

    if u.available.is_some() {
        let action: El<'a> = match (&u.phase, &u.method) {
            (Phase::Working(s), _) => {
                let mut r = row![secondary(stage_text(*s), M.font_sm)]
                    .spacing(12)
                    .align_y(Alignment::Center);
                if matches!(s, Stage::Downloading { .. }) {
                    r = r.push(outline_btn("Cancel", msg(UpdateUi::Cancel)));
                }
                r.into()
            }
            (Phase::Installed(_), _) => primary_btn("Restart now", msg(UpdateUi::Restart)),
            (Phase::Failed(e), _) => row![
                secondary(e.as_str(), M.font_sm),
                outline_btn("Open release page", msg(UpdateUi::OpenReleaseNotes)),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into(),
            (Phase::Idle, Some(InstallMethod::InstallScript(_))) => {
                primary_btn("Update now", msg(UpdateUi::Install))
            }
            (Phase::Idle, Some(InstallMethod::Cargo)) => {
                primary_btn("Copy command", msg(UpdateUi::CopyCommand))
            }
            (Phase::Idle, _) => primary_btn("Open release page", msg(UpdateUi::OpenReleaseNotes)),
        };
        let title = match &u.phase {
            Phase::Installed(_) => "Restart to finish",
            _ => "New version",
        };
        let hint: Option<&'a str> = match &u.method {
            Some(InstallMethod::Cargo) => {
                Some("Installed with cargo install: update with the command shown in the banner.")
            }
            Some(InstallMethod::InstallScript(_)) | None => None,
            Some(_) => Some("Ferrotune cannot update this copy itself."),
        };
        rows.push(setting_row(title, hint, action));
    }
    rows.push(setting_row(
        "Release notes",
        None,
        outline_btn("Open", msg(UpdateUi::OpenReleaseNotes)),
    ));

    rows.push(setting_row(
        "Check for updates automatically",
        Some("At start and every 6 hours, also while in the tray."),
        toggler(app.settings.auto_update_check)
            .on_toggle(|v| {
                ui(Ui::Setting(
                    crate::app::settings::SettingChange::AutoUpdateCheck(v),
                ))
            })
            .size(20)
            .into(),
    ));
    let method = u.method.as_ref().map_or("…", InstallMethod::label);
    rows.push(setting_row(
        "Install method",
        Some("Detected from where Ferrotune runs."),
        secondary(method, M.font_sm).into(),
    ));
    rows.push(
        secondary(
            "Releases come from GitHub and are verified with their SHA-256 checksum. They are not signed yet.",
            M.font_sm,
        )
        .into(),
    );
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_texts() {
        assert_eq!(
            stage_text(Stage::Downloading {
                received: 50,
                total: Some(200)
            }),
            "Downloading… 25%"
        );
        assert_eq!(
            stage_text(Stage::Downloading {
                received: 3 * 1_048_576,
                total: None
            }),
            "Downloading… 3.0 MB"
        );
        assert_eq!(stage_text(Stage::Verifying), "Verifying…");
    }
}
