#![deny(clippy::unwrap_used, clippy::expect_used)]
//! Entry point: logging, config, `iced::application(..).run()`.
//!
//! Usage: `ferrotune [--mock] [--minimized] [--screenshot <screen> <out.png>] [--install-desktop-entry]`
//! Screens: login, home, search, library, playlist, album, artist, liked, queue, lyrics, connect, keyring,
//! menu, shortcuts, rename, settings, soloist, soloist-settings, update-banner, updates.

use std::time::Instant;

use ferrotune::app::{App, Options, ScreenshotPlan};
use ferrotune::config::{APP_ID, Config};
use ferrotune::tray::instance::{self, Role};
use ferrotune::ui::{self, theme};

fn usage() -> ! {
    eprintln!(
        "usage: ferrotune [--mock] [--minimized] [--screenshot <screen> <out.png>] [--install-desktop-entry]"
    );
    std::process::exit(2);
}

/// Write the .desktop entry and icon to `~/.local/share` and exit.
fn install_desktop_entry() -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| "ferrotune".into());
    let Some(home) = ferrotune::desktop::data_home() else {
        eprintln!("could not determine the XDG data directory");
        std::process::exit(1);
    };
    match ferrotune::desktop::install(&home, &exe) {
        Ok((entry, icon)) => {
            println!("installed {}", entry.display());
            println!("installed {}", icon.display());
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("could not install the desktop entry: {e}");
            std::process::exit(1);
        }
    }
}

fn main() -> iced::Result {
    let started = Instant::now();
    if std::env::var_os("ICED_BACKEND").is_none() {
        // The CPU renderer keeps idle RSS well under the 150 MB budget
        // (~46 MB vs ~200 MB with wgpu on Mesa lavapipe). Users can opt into
        // the GPU renderer with ICED_BACKEND=wgpu.
        // SAFETY: called first thing in main, before any thread is spawned.
        unsafe { std::env::set_var("ICED_BACKEND", "tiny-skia") };
    }
    let mut mock = false;
    let mut minimized = false;
    let mut screenshot = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mock" => mock = true,
            "--minimized" => minimized = true,
            "--screenshot" => {
                let (Some(screen), Some(out)) = (args.next(), args.next()) else {
                    usage()
                };
                screenshot = Some(ScreenshotPlan {
                    screen,
                    out: out.into(),
                });
                mock = true;
            }
            "--install-desktop-entry" => install_desktop_entry(),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    let config = Config::load(mock);
    ferrotune::logging::init((!mock).then(|| config.log_path()).as_deref());
    let ferro = theme::FerroTheme::new(config.theme);
    theme::set_current(ferro);
    // One instance (and one tray icon): a second launch asks the running one
    // to show its window and exits. Screenshots never take the lock.
    if screenshot.is_none() {
        let name = if mock {
            format!("{APP_ID}.mock")
        } else {
            APP_ID.to_owned()
        };
        match instance::acquire(&name, !minimized) {
            Role::Primary(p) => p.serve(instance::request_show_locally),
            Role::Secondary => {
                tracing::info!("already running; asked it to show its window");
                return Ok(());
            }
        }
    }
    let opts = Options {
        screenshot,
        minimized,
        started: Some(started),
    };

    // A daemon, not an application: hiding to the tray closes the window
    // (freeing the renderer) while playback, Connect and MPRIS keep running.
    iced::daemon(
        move || App::boot(config.clone(), opts.clone()),
        App::update,
        ui::view_window,
    )
    .title(|app: &App, _window: iced::window::Id| app.title())
    .subscription(App::subscription)
    .theme(|app: &App, _window: iced::window::Id| {
        theme::FerroTheme::new(app.settings.theme).to_iced()
    })
    .font(theme::INTER_REGULAR)
    .font(theme::INTER_SEMIBOLD)
    .font(theme::INTER_BOLD)
    .default_font(theme::FONT)
    .run()
}
