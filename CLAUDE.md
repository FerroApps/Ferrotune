# Ferrotune

## Overview
Native Rust Spotify desktop client (unofficial, Premium only). iced 0.14 UI
recreating the official dark look. One librespot 0.8 session (desktop client
id, zero-config browser login) drives audio, Spotify Connect and the internal
spclient/pathfinder APIs; no public Web API. MPRIS on Linux; SQLite cache.

## Commands
- `cargo build --release` / `cargo run -- --mock`
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings`
- `dbus-run-session -- cargo test` (MPRIS integration test needs a bus)
- Screenshots: `ICED_BACKEND=tiny-skia xvfb-run -a -s "-screen 0 1280x800x24" ./target/debug/ferrotune --screenshot <screen> docs/screenshots/<screen>.png`
- Build jobs on shared machines: `CARGO_BUILD_JOBS=2`

## Architecture
See `docs/architecture.md` (shared Ferro* skeleton) and `docs/protocol.md`
(login, endpoints, persisted queries, Connect, MPRIS, UI reference).
- `src/app/` state + update (queue/nav reducers in `state.rs`)
- `src/ui/` views + `theme.rs` (only place with colours/sizes)
- `src/net/` session, internal-API backends (traits in `backend.rs`), playback, MPRIS
- `src/store/` SQLite cache, `src/auth/` browser login (PKCE) + keyring

## Conventions
- English everywhere; Conventional Commits; small commits.
- No `unwrap`/`expect` outside tests (crate-level deny).
- Only `auth/secrets.rs` touches `keyring`. Secrets use `Secret` (redacted Debug).
- `net`/`store`/`auth` never import iced widgets.
- New crates need a one-line justification in the PR.
- Views take `&App`; all mutation goes through `App::update`.

## Workflow quirks
- Work on feature branches into `development`; never push `main`/`development`.
- `version.yml` bumps the version in `Cargo.toml`; don't bump by hand.
- Tests never hit the network: use `wiremock` + `tests/fixtures/`.

## Deployment
Releases are automatic: `.github/workflows/release.yml` chains off the "Version bump" workflow
via `workflow_run` (GITHUB_TOKEN bot pushes don't trigger `push` workflows), builds on
ubuntu-22.04, and creates GitHub Release `v<Cargo version>` with `ferrotune-v<ver>-linux-x86_64.tar.gz`
+ `.sha256` (skips if the release exists; manual re-run: workflow_dispatch). Users run
`install.sh` (`curl ... | sh`; `-s -- --uninstall`); test it with `FERRO_RELEASE_API=file://...`.
`ferrotune --install-desktop-entry` still writes the .desktop file + icon from `assets/desktop/`.

## Gotchas
- iced: keep default features on (x11/wayland). `Subscription::map` closures
  must be non-capturing.
- `tokio::time::sleep` must be created inside an `async` block for
  `Task::perform` (no runtime in unit tests otherwise).
- Spotify requires `http://127.0.0.1:<port>` redirect URIs (not localhost).
- Pathfinder persisted-query hashes rotate: they live only in
  `net/pathfinder.rs`; refresh steps in docs/protocol.md.
- Paging uses raw entry counts (`Page::raw_len`), since episodes/null items
  are dropped during conversion.
- librespot rodio backend needs ALSA dev libs on Linux.
- Right-click needs `ui/context_area.rs` (`mouse_area` has no window position).
  `update_ui` closes the context menu on every `Ui` except the exempt list at
  its top: add new passive events (hover, scroll, resize) there.
- Playlist edits are optimistic: keep the `EditSnapshot` rollback path (tested
  in `app/playlists.rs`) when adding new edit kinds.
- Tray: `ksni` must use `default-features = false, features = ["async-io"]`
  (its `tokio` feature switches zbus to tokio and breaks keyring's blocking
  connect). Hiding closes the window (`iced::daemon`); never hide without a
  registered StatusNotifierWatcher.
- librespot's `Unavailable` has no reason: `net/audio_diag.rs` reads it from
  librespot's log. Audio key code 1 = Spotify denies local playback for the
  account (librespot #1649); play through Connect, never bypass it.
- Spotify Soloist (`net/soloist/`) is the local engine when librespot's key
  is refused: downloaded from Spotify's CDN on first use, NEVER bundled or
  committed (Spotify forbids redistribution). Its API key lives only in the
  keyring (`KEY_SOLOIST`) and reaches the child only via `--api-key`
  (1.3.10 has no env var for it); never log argv unredacted. Always pass
  `--cache-size` (Soloist's default cache is unlimited).
  With `engine = soloist`, no Spirc and no local librespot player: the
  Connect controller opens its own dealer (`SessionOptions::controller`).
- In-app updater (`src/update/`, glue `app/updates.rs`, views `ui/update.rs`):
  repo `FerroApps/Ferrotune`. Only an install.sh install (marker
  `installed-version`) self-updates; cargo installs get the command, anything
  else the release page. Never write outside the install.sh paths, keep the
  file list in step with `install.sh`/`release.yml` (`update::BINARIES`),
  and never run timed checks in mock/screenshot/debug builds. Releases are
  checksum-verified only (no signature yet). Test with
  `FERRO_RELEASE_API=http://127.0.0.1:<port>/...` (loopback http only).
