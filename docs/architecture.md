# Architecture

> Shared skeleton for the Ferro* family (Ferrogram, Ferrotune, Ferrocord,
> Ferrochat). All four clients start from this document. Sections marked
> **(app-specific)** are filled in per repository; everything else must stay
> identical across the four repos so that a fix or convention in one applies
> to all of them.

## 1. Goals and budgets

| Budget | Target | How we stay inside it |
|---|---|---|
| Idle RAM (logged in) | < 150 MB RSS | Bounded in-memory caches, lazy image decode, SQLite as the source of truth for history, `tiny-skia`/`wgpu` renderer chosen at runtime. |
| Cold start | < 1 s to first frame | No network on the critical path: the first frame is rendered from the SQLite cache; the session is restored from the keyring in a background `Task`. |
| Stability | No panics in normal use | `#![deny(clippy::unwrap_used, clippy::expect_used)]` outside tests, typed errors, every network failure is a state, not a crash. |

## 2. Module layout

Every repository has the same top-level layout. Names of files inside each
module may differ, the module boundaries may not.

```
src/
├── main.rs          # Entry point: logging init, config, iced::application(..).run()
├── lib.rs           # Re-exports modules so integration tests can reach them
├── app/             # State + update (the "Elm" side). No I/O here.
│   ├── mod.rs       #   App struct, boot(), update(), subscription()
│   ├── message.rs   #   Message enum (UI events + net events + store events)
│   └── state.rs     #   Pure state types and reducers (unit-tested)
├── ui/              # Views. Pure functions &State -> Element<Message>.
│   ├── mod.rs       #   view() dispatcher by Screen
│   ├── theme.rs     #   Palette, typography, spacing, widget styles
│   ├── icons.rs     #   Lucide SVG icons (ISC licence), embedded with include_bytes!
│   └── <screen>.rs  #   One file per screen (login, sidebar, chat, ...)
├── net/             # Protocol client. Owns sockets, never touches UI types.
│   ├── mod.rs       #   Client handle + Event enum exposed to app/
│   ├── types.rs     #   Wire types (serde) and their conversion to domain types
│   └── ...          #   (app-specific: gateway.rs, rest.rs, mtproto.rs, ...)
├── store/           # Local cache: SQLite via rusqlite.
│   ├── mod.rs       #   Store handle, migrations, typed queries
│   └── schema.sql   #   Embedded with include_str!, versioned by PRAGMA user_version
├── auth/            # Login flows + keyring access.
│   ├── mod.rs       #   AuthState machine (unit-tested)
│   └── secrets.rs   #   The ONLY module allowed to call the `keyring` crate
├── config.rs        # Paths (XDG via `directories`), feature flags, env overrides
├── error.rs         # crate::Error (thiserror) + Result alias
└── mock.rs          # Mock data used by tests and `--mock` screenshot mode
```

Dependency direction (enforced by review, not by the compiler):

```
ui ──▶ app ──▶ net
        │ └──▶ store
        └────▶ auth ──▶ (keyring)
```

`ui` only reads `app` state and emits `Message`s. `net`, `store` and `auth`
never import `ui` or `iced` widgets; they expose plain async APIs and event
streams so they can be tested without a window.

## 3. State model

The app follows iced's Elm architecture.

* **`App`** owns all state. It is a tree of plain structs: `Screen` (what is
  shown), `Session` (who is logged in), domain caches (chats, channels,
  playlists, ...) and transient UI state (composer text, scroll offsets,
  hover, open menus).
* **`Message`** is the only way state changes. Three families:
  * `Ui(..)` — user input (click, type, scroll, key press).
  * `Net(net::Event)` — pushed from the protocol client's event stream
    (new message, presence, typing, playback state, connection state).
  * `Done(..)` — completion of a `Task` (REST response, DB query, keyring read).
* **`update(&mut App, Message) -> Task<Message>`** is the only mutation
  point. Heavy logic lives in `app/state.rs` as pure functions
  (`fn apply_event(state: &mut ChatState, ev: &Event)`) so it is unit-tested
  without iced.
* **`subscription(&App)`** exposes long-lived streams: the network event
  stream (via `Subscription::run_with`), keyboard shortcuts, a 1 s tick for
  relative timestamps (only while visible), and MPRIS / notification
  callbacks where applicable.
* **Connection state** is explicit: `Disconnected → Connecting →
  Connected | Reconnecting { attempt, next_at }`. Reconnects use exponential
  backoff with jitter (1 s → 60 s cap). The UI renders a thin banner while
  not `Connected`, and keeps showing cached data.
* **Caches are bounded.** In-memory message windows hold at most N messages
  per open conversation (default 200); older pages are loaded from SQLite on
  scroll, then from the network. Decoded images live in an LRU keyed by
  media id with a byte budget (default 48 MB).

### Startup sequence

1. `main` initialises `tracing` (with the redaction layer, §6) and config.
2. `App::boot()` opens SQLite synchronously (it is local and fast), loads the
   last-known chat/channel list and the active screen, and returns a `Task`
   batch: restore session from keyring → connect → sync deltas.
3. First frame is drawn from cached data. Target: < 1 s on a cold cache,
   measured by `FERRO_TRACE_STARTUP=1` which logs `first_frame_ms`.

## 4. Theme system

`ui/theme.rs` is the single source of truth for the look. No view hard-codes a
colour, size or font.

```rust
pub struct Palette {
    pub bg_app: Color,        // window background
    pub bg_sidebar: Color,    // list / navigation column
    pub bg_panel: Color,      // main content panel
    pub bg_elevated: Color,   // popovers, menus, modals
    pub bg_hover: Color,
    pub bg_selected: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_muted: Color,
    pub accent: Color,        // brand-like accent (recreated, not copied assets)
    pub accent_text: Color,
    pub danger: Color,
    pub success: Color,
    pub border: Color,
    pub bubble_out: Color,    // chat apps only
    pub bubble_in: Color,     // chat apps only
    pub badge: Color,
    pub badge_muted: Color,
}

pub struct Metrics { /* spacing scale, radii, sidebar widths, avatar sizes, font sizes */ }

pub struct FerroTheme { pub palette: Palette, pub metrics: Metrics, pub dark: bool }
```

* A `FerroTheme` is converted into an `iced::Theme::custom(..)` for the base
  widgets, and module-level style functions (`fn chat_row(theme, status)`,
  `fn bubble(theme, outgoing)`, `fn primary_button(..)`) provide the
  per-widget `Style` closures used by views.
* **Dark theme is the default.** A light palette exists where the official
  client has one; selection follows the `FERRO_THEME` env var or config, and
  defaults to dark.
* **Typography:** Inter (SIL OFL 1.1) embedded via `include_bytes!` and
  registered with `iced::font::load`; Noto Color Emoji is used from the system
  if available. Font sizes follow a small scale defined in `Metrics`.
* **Icons:** Lucide (ISC licence) SVGs embedded in `ui/icons.rs`. Official
  logos, icon sets and fonts of the service are never included.
* **(app-specific)** The palette values, metrics and layout proportions that
  reproduce the official client are documented in `docs/protocol.md` §UI
  reference or in a table at the end of this file.

## 5. Keyring handling

* `auth/secrets.rs` is the only module that calls the `keyring` crate.
  It exposes:

  ```rust
  pub trait SecretStore: Send + Sync {
      fn get(&self, key: &str) -> Result<Option<Secret>>;
      fn set(&self, key: &str, value: &Secret) -> Result<()>;
      fn delete(&self, key: &str) -> Result<()>;
  }
  pub struct KeyringStore;       // OS keyring (Secret Service / Keychain / Credential Manager)
  pub struct MemoryStore(..);    // tests and --mock mode
  ```

* Service name: `dev.ferro.<app>` (e.g. `dev.ferro.ferrogram`), account key:
  a stable non-secret id such as `session` or `user:<numeric id>`.
* `Secret` wraps the value in `secrecy::SecretString` / `SecretBox<Vec<u8>>`:
  it is zeroised on drop and its `Debug` impl prints `[REDACTED]`.
* Large blobs (e.g. an MTProto auth key or Signal identity store) are encrypted
  with a random key that lives in the keyring; only the ciphertext is written
  to the data directory. Plain-text tokens never touch disk.
* Keyring calls are blocking; they run in `tokio::task::spawn_blocking` from a
  `Task`, never on the UI thread.
* If no Secret Service is available (headless CI), the app shows an explicit
  error on the login screen instead of falling back to a file.

## 6. Logging and redaction

* `tracing` + `tracing-subscriber` with `EnvFilter` (`RUST_LOG`, default
  `info`). Logs go to stderr and to `$XDG_STATE_HOME/<app>/log` (rotated,
  max 5 MB).
* **Never log** tokens, auth keys, phone numbers, email addresses, message
  bodies, file names of user media, or full URLs that contain signatures.
* Types that carry such data implement `Debug` manually with redaction
  (`Phone("+34•••••••12")`, `MessageBody(len=42)`).
* A `Redacted<T>` newtype in `error.rs` / `util` is used in `tracing` fields:
  `info!(chat = %id, body = %Redacted(&text))`.
* A unit test greps formatted log output for known secret fixtures to prove
  redaction works.

## 7. Error handling

* `crate::Error` (thiserror) groups errors by origin: `Net`, `Protocol`,
  `Auth`, `Store`, `Keyring`, `Io`, plus `Cancelled`.
* Library-style modules (`net`, `store`, `auth`) return `crate::Result<T>`.
  `anyhow` is not used.
* `update()` never panics: errors coming back from tasks become state
  (`toast`, inline error under a field, connection banner). Each error has a
  user-facing message (`Error::user_message()`) that is free of secrets.
* Auth errors that mean "session revoked" clear the keyring entry and return
  to the login screen.
* Clippy runs with `-D warnings`; `unwrap()`/`expect()` are denied in non-test
  code via crate-level lints.

## 8. Local store (SQLite)

* One database per app at `$XDG_DATA_HOME/<app>/cache.sqlite3`, opened with
  `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`.
* Migrations are embedded SQL run in order and tracked with `PRAGMA
  user_version`.
* The store is a cache: it can be deleted at any time and is rebuilt from the
  network. It must not contain credentials.
* Access is through a `Store` handle wrapping `rusqlite::Connection` in a
  dedicated blocking thread (or `spawn_blocking` + `Mutex`) so the UI thread
  never waits on disk.

## 9. Notifications and desktop integration

* Desktop notifications via `notify-rust` (D-Bus on Linux). Notification
  text follows the official client's privacy default (sender + preview), and
  can be switched to "sender only".
* Focus-aware: no notification for the conversation that is currently open
  and focused.
* Wayland/KDE: the app sets a stable `app_id` (`dev.ferro.<app>`) so KDE
  groups windows and notifications correctly.

## 10. Testing strategy

* **Unit tests** next to the code: protocol parsing (fixtures under
  `tests/fixtures/`), state reducers, store queries (in-memory SQLite),
  redaction.
* **Integration tests** in `tests/`: run the net client against a local mock
  server (`tokio` TCP/WebSocket or `wiremock`) replaying recorded or
  hand-written responses. No real accounts are used.
* **Screenshot mode:** `cargo run -- --mock --screenshot <screen> <out.png>`
  boots the app with `mock.rs` data and an in-memory secret store, renders the
  requested screen, captures it with `iced::window::screenshot`, writes a PNG
  and exits. Used to produce PR screenshots under Xvfb.

## 11. App-specific sections (app-specific)

* Protocol client design and library choice → `docs/protocol.md`.
* Screens and navigation map.
* Palette table.

### Ferrotune

**Library choice.** `librespot` 0.8 (`core`, `playback`, `metadata`,
`protocol`, `connect`). One session, opened with the desktop client id after
a zero-config browser login (own PKCE implementation, `auth/oauth.rs`), carries
audio, Spotify Connect and the session-authenticated internal APIs (spclient
protobuf, pathfinder GraphQL, over `reqwest`). The public Web API is not used.
`mpris-server` 0.9 (zbus 5) provides MPRIS. Details in `docs/protocol.md`.

**Module map.**

| Module | Ferrotune files |
|---|---|
| `app/` | `mod.rs` (App, boot, update, subscription), `message.rs`, `state.rs` (Queue, Nav, Playback, backoff), `images.rs` (LRU of decoded art, 48 MB), `shortcuts.rs` (key → message), `menu.rs` (context-menu and dialog model), `playlists.rs` (optimistic playlist edits), `edit.rs` (pure edit reducers) |
| `ui/` | `theme.rs`, `icons.rs`, `widgets.rs`, `login.rs`, `topbar.rs`, `sidebar.rs`, `home.rs`, `search.rs`, `library.rs`, `collection.rs` (playlist/album/liked), `artist.rs`, `queue.rs`, `now_playing.rs`, `menu.rs` (context menu + dialogs), `context_area.rs` (right-click position widget) |
| `net/` | `session.rs` (librespot session, Connect device, `LiveAuth`), `client.rs` (authenticated HTTP: cache, coalescing, 429/401 handling), `tokens.rs`, `backend.rs` (Catalog/Library/Search/Home/Lyrics traits), `spclient.rs`, `pathfinder.rs`, `proto.rs` (collection2v2 codec), `types.rs` (wire → domain), `model.rs`, `paging.rs`, `playlist_edit.rs` (playlist4 change bodies), `playback.rs` (local + Connect players, mock engine), `mpris.rs`, `images.rs` |
| `store/` | `mod.rs`, `schema.sql` (`entity` JSON blobs by kind/id, `kv` settings), `schema_v2.sql` (`http_cache`) |
| `auth/` | `mod.rs` (AuthState: silent login → browser fallback), `oauth.rs` (PKCE, loopback listener, code exchange), `secrets.rs` |
| `tray/` | `model.rs` (tray state + `Command`, pure), `sni.rs` (StatusNotifierItem via `ksni`, Linux), `stub.rs` (other OSes: no tray, not implemented), `icon.rs` (app SVG to ARGB pixmaps), `instance.rs` (single-instance Unix socket), `autostart.rs` (XDG autostart entry) |
| `update/` | in-app updater, no iced: `release.rs` (GitHub latest release, ETag, asset choice), `version.rs` (semver), `method.rs` (install-method detection, pure), `download.rs` (GitHub-only HTTPS, caps, SHA-256), `extract.rs` (safe tar.gz), `install.rs` (atomic swap + rollback), `schedule.rs`, `state.rs` (`update.json`), `restart.rs`; app glue `app/updates.rs`, views `ui/update.rs` |
| other | `desktop.rs` (.desktop entry + icon installer) |

**Tray and background mode.** The app is an `iced::daemon`. Closing the
window (with a StatusNotifierWatcher present and "close to tray" on) CLOSES
it, freeing the renderer; `app/window.rs` is the pure window state machine,
`app/background.rs` turns its `Action`s into iced window tasks. The librespot
session, Connect device, playback engine and MPRIS live in `App`, not in the
window, so they keep running while hidden. The tray menu (Show/Hide,
Play/Pause, Next, Previous, Start minimized, Quit) sends the same `Ui`
messages as the player bar, so with a Connect device in control they control
that device. Scroll over the icon steps the volume. No tray host on the bus
means closing quits. `--minimized` starts with only the tray; a second launch
asks the running instance (socket in `$XDG_RUNTIME_DIR`) to show its window.
Settings: close to tray and start minimized live in `config.json`; "open at
login" is the existence of `~/.config/autostart/dev.ferro.ferrotune.desktop`.

**Updates.** `GET https://api.github.com/repos/FerroApps/Ferrotune/releases/latest`
(`Accept: application/vnd.github+json`, `User-Agent: ferrotune/<version>`,
`If-None-Match` with the stored ETag; no account data, no IDs). Timed checks
run 10-20 s after start and then every 6 h ± 30 min, also while hidden in the
tray (`Subscription::run(ticks)`, dropped when "Check for updates
automatically" is off); never in `--mock`, `--screenshot` or debug builds
unless `FERRO_UPDATE_FORCE=1`. 403/429 back off silently (`Retry-After` /
`x-ratelimit-reset`, clamped). `FERRO_RELEASE_API` replaces the full
"latest release" URL, as in `install.sh`, but only `https://` or loopback
`http://` (tests). Strict semver against `CARGO_PKG_VERSION`; drafts and
prereleases are ignored. The banner sits above the top bar: "Ferrotune X is
available" [Update] [Release notes] [x]; [x] stores the version in
`$XDG_STATE_HOME/ferrotune/update.json` (with the ETag and its release) and a
newer version shows again. Settings → Updates: version, check button and
result, update / restart button, release notes, the auto-check toggle and the
detected install method.

Install methods (`update::method::detect`, pure, fed the canonical
`current_exe()` plus env markers):

| Method | Detected by | Update does |
|---|---|---|
| install.sh | exe = `${XDG_BIN_HOME:-~/.local/bin}/ferrotune` and `${XDG_DATA_HOME:-~/.local/share}/ferrotune/installed-version` exists, bin dir writable | self-update |
| cargo install | exe in `${CARGO_HOME:-~/.cargo}/bin` | banner shows `cargo install --git https://github.com/FerroApps/Ferrotune --locked --force` + copy button |
| other | system path, Flatpak (`FLATPAK_ID`), AppImage (`APPIMAGE`/`APPDIR`), a cargo `target/` dir, read-only install.sh dir, non-Linux-x86_64 | open the release page only |

Self-update (`update::install`): the `.sha256` and the tarball are streamed
into `.ferrotune-update.<pid>/` inside the bin dir (same file system), the
SHA-256 is checked (mismatch: abort, nothing touched), the tarball is
extracted by a reader that refuses absolute paths, `..`, symlinks, hardlinks,
special files, pax headers and more than one top directory (caps: 300 MB
download, 400 MB unpacked). Then the binary, desktop entry (`Exec=` rewritten
exactly like `install.sh`), icon and `installed-version` are staged as
`.<name>.new.<pid>` and renamed over their targets; the old files are kept as
hard links until every rename succeeded and are put back on failure. The
desktop caches are refreshed like `install.sh` on a background thread.
Downloads only go to `api.github.com`, `github.com`,
`objects.githubusercontent.com` and `release-assets.githubusercontent.com`
over HTTPS (redirects included); logged URLs drop their signed query. The
download is a cancellable `Task::run` stream; the swap runs in
`spawn_blocking`. "Restart now" removes the single-instance socket
(`tray::instance::release`), spawns the new binary with the same args (plus
`--minimized` when hidden) and quits through `App::quit`.

Integrity: the checksum is published by the same release, so it only proves
the download is intact, not who built it. Releases are not signed yet.
Proposal: sign the tarball in `release.yml` with minisign (or cosign
keyless) and embed the public key in the binary; this needs a signing-key
repository secret that the maintainer has to add first.

**Long-lived subscriptions.** Playback engine events
(`Subscription::run_with(EngineKind, ..)`), MPRIS commands (Linux), a 500 ms
tick only while playing or a toast is visible (and the window is shown), the
tray (`tray::events`), single-instance "show" requests and the update timer.

**Screens and navigation.**

```
Login ──(keyring creds, silent │ browser PKCE)──▶ librespot session ──▶ Main layout
Main layout = Top bar (back/forward, Home, Search) + Sidebar (Your Library)
            + Main panel (Route) + optional Queue or Lyrics panel + Now-playing bar
Route: Home | Search | Library | Liked | Playlist(id) | Album(id) | Artist(id)
Back/forward history: app::state::Nav (max 50 entries)
```

**Palette.** See `docs/protocol.md` §5 (UI reference).
