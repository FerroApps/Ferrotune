# Protocol notes (Ferrotune)

Ferrotune talks to Spotify the way the official desktop client does: **one
librespot session**, opened with the desktop client id, is the only identity.
It carries audio (access points), Spotify Connect (dealer + connect-state),
and the session-authenticated HTTP calls to Spotify's internal **spclient**
and **pathfinder** (GraphQL) endpoints. The public Web API
(`api.spotify.com/v1`) is **not** used. On Linux Ferrotune also serves
**MPRIS** on the session bus.

Nothing here is an official, documented API. Every endpoint below was
verified only against the recorded/hand-built fixtures in `tests/` (there is
no Spotify account in CI). Spotify can change any of them at any time.

## 1. Login

Implemented in `src/auth/oauth.rs` (PKCE + loopback listener) and
`src/net/session.rs` (librespot session). `librespot-oauth` is not used,
because our listener is hardened (below) and it would pull in `oauth2` and `open`.

1. **Client id:** `librespot_core::SessionConfig::default().client_id`, the
   public desktop ("keymaster") client id that librespot, ncspot and
   spotify-player use (`65b708073fc0480ea92a077233ca87bd` in librespot 0.8).
   It is read from librespot, not hard-coded.
2. **Redirect:** `http://127.0.0.1:8898/login`, the form librespot's
   `librespot-oauth` example uses with this client id. Spotify requires the
   literal `127.0.0.1`. Neither librespot-oauth's docs nor its code say
   whether other ports are accepted for this id, so Ferrotune does not
   auto-try other ports. A busy port shows an error that explains
   `FERROTUNE_REDIRECT_PORT` (unverified for other ports).
3. **Scopes:** the list the `librespot` binary requests (`streaming`,
   `user-library-*`, `playlist-*`, `user-read-*`, ... see `config::SCOPES`).
4. PKCE: a 64-character verifier, an S256 challenge and a random 16-byte `state`.
   The loopback listener binds **before** the browser opens, answers 400 and
   keeps listening on anything without our `state` (favicon, stray or hostile
   local requests), has a 2 s per-connection read timeout and a 5 min overall
   timeout.
5. `POST accounts.spotify.com/api/token` exchanges the code. The access token
   is used **once**: `Credentials::with_access_token` opens the librespot
   session. It is never stored, and no refresh token is kept.
6. After connecting, librespot's reusable credentials
   (`session.auth_data()`, `AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS`) are
   written to the keyring. Next start: silent login with them, no browser.
   If the access point refuses them (`PermissionDenied`), they are deleted and
   the login screen explains that the browser login is needed again. Network
   failures keep them, and "Log in" retries silently.

### Keyring entries (service `dev.ferro.ferrotune`)

| Account key | Content |
|---|---|
| `librespot` | JSON `{username, auth_data(base64)}`, librespot's reusable credentials |
| `soloist-api-key` | the user's Spotify Soloist API key (§3c) |
| `oauth` | legacy (Web-API releases); deleted on start and logout |

Nothing else secret is persisted. The SQLite cache contains no credentials.

### Advanced: your own client id

`FERROTUNE_CLIENT_ID=<id>` replaces the desktop client id for the browser login
and the session. It is not needed, and it is not mentioned in the UI. The app
must accept `http://127.0.0.1:<FERROTUNE_REDIRECT_PORT or 8898>/login`.
Tokens for spclient come from the session (login5) either way.

## 2. Internal endpoints

All requests go through `net::client::InternalClient` with:

- `Authorization: Bearer <token>`, where the token comes from the session's
  **login5** (`session.login5().auth_token()`), the same source librespot's
  own spclient and dealer use. Tokens are cached and refreshed before expiry.
  A 401 triggers one **single-flight** refresh (parallel requests that hit the
  401 share it).
- `client-token: <session.spclient().client_token()>`.
- Endpoint overrides (`FERROTUNE_SPCLIENT_URL`, `FERROTUNE_PATHFINDER_URL`)
  receive the session token, so only `https://` (or `http://` on loopback,
  for local test servers) is accepted.
- `app-platform` (`Linux`/`OSX`/`Win32`), `spotify-app-version`
  (`librespot_core::version::SPOTIFY_SEMANTIC_VERSION`, the version librespot
  announces), `User-Agent` in librespot's own format, `Accept-Language` from
  `LANG`.
- No stealth: plain rustls/reqwest, no TLS-fingerprint tricks, and no
  telemetry or event-sender traffic, real or fake.

spclient host: resolved by the session (`spclient().base_url()`); pathfinder
host: `https://api-partner.spotify.com`. Both can be overridden for tests
(`FERROTUNE_SPCLIENT_URL`, `FERROTUNE_PATHFINDER_URL`).

| Backend (trait) | Data | Endpoint | Format |
|---|---|---|---|
| `LibraryApi::me` | display name, avatar | `GET /user-profile-view/v3/profile/{user}` (failure falls back to the username) | JSON |
| `LibraryApi::profile` | profile page, playlist owner names | `GET /user-profile-view/v3/profile/{user}?playlist_limit=N&artist_limit=0&episode_limit=0&market=from_token` | JSON |
| `LibraryApi::library` | sidebar playlists | `GET /playlist/v2/user/{user}/rootlist?decorate=revision,attributes,length,owner,capabilities,status_code&from=0&length=500` | playlist4 `SelectedListContent` |
| `LibraryApi::library` / `saved_tracks` | Liked Songs + saved albums | `POST /collection/v2/paging` set `collection` (300/page), then `POST /collection/v2/delta` with the last sync token | collection2v2 (`application/vnd.collection-v2.spotify.proto`) |
| `LibraryApi::library` | followed artists | same, set `artist` | collection2v2 |
| `LibraryApi::set_saved` | like / unlike | `POST /collection/v2/write` | collection2v2 `WriteRequest` |
| `LibraryApi::contains_saved` | heart state | answered from the synced collection index, with no request | — |
| `CatalogApi::playlist(_tracks)` | playlist + entries | `GET /playlist/v2/playlist/{id}?decorate=revision,length,attributes,timestamp,owner,capabilities&from=&length=100` | playlist4 |
| `connect_ctl` | Connect devices | `PUT /connect-state/v1/devices/hobs_{device id}` (hidden observer, `PICKER_OPENED`) + dealer `hm://connect-state/v1/cluster` | connect `PutStateRequest` → `Cluster` |
| `connect_ctl` | control another device | `POST /connect-state/v1/player/command/from/{us}/to/{it}`, `POST …/connect/transfer/from/{us}/to/{it}`, `PUT …/connect/volume/from/{us}/to/{it}` | JSON |
| `CatalogApi::*` | track / album / artist metadata | `POST /extended-metadata/v0/extended-metadata`, **batched** (≤100 URIs), `TRACK_V4` / `ALBUM_V4` / `ARTIST_V4` | `BatchedEntityRequest` → `metadata.Track/Album/Artist` |
| `PlaylistApi::create_playlist` | create | `POST /playlist/v2/playlist` (`ListUpdateRequest` with the name), then add to the rootlist | playlist4 |
| `PlaylistApi::edit_playlist` | add / remove / move / rename | `POST /playlist/v2/playlist/{id}/changes` (`ListChanges`: one `Delta` with `ADD` / `REM` / `MOV` / `UPDATE_LIST_ATTRIBUTES` ops) | playlist4 |
| `PlaylistApi::delete_playlist` | delete / remove from library | `POST /playlist/v2/user/{user}/rootlist/changes` (`REM` of the playlist URI, `items_as_key`) | playlist4 |
| `LyricsApi` | synced lyrics | `GET /color-lyrics/v2/track/{id}?format=json&vocalRemoval=false&market=from_token` | JSON |
| `SearchApi` | search | pathfinder `searchDesktop` (fallback: `GET /searchview/km/v4/search/{q}`) | JSON |
| `HomeApi` | Home shelves | pathfinder `home` (fallback: `GET /recently-played/v3/user/{user}/recently-played`) | JSON |
| — | cover art | `GET https://i.scdn.co/image/{file id hex}` | JPEG |

The collection2v2 messages are not compiled into `librespot-protocol`, so a
~150-line hand-written codec (`net/proto.rs`) encodes and decodes them. The
codec has unit tests, including wire-level checks.

### Playlist editing (`net/playlist_edit.rs`)

Bodies are built from `librespot-protocol`'s `playlist4_external` messages
(`ListChanges`, `Delta`, `Op`, `Add`, `Rem`, `Mov`, `UpdateListAttributes`,
`ListUpdateRequest`, `CreateListReply`) and unit-tested by decoding them again.

- Every edit first reads the list's **current revision** (uncached,
  `?from=0&length=1`) and sends it as `base_revision`, so it applies on top
  of the server state. After a write, every cached `/playlist/v2/` read is
  dropped.
- Removes carry `from_index`, `length = 1` and the item URI, highest index
  first. Moves send `to_index` as "insert before the original item at this
  index" (our API takes the final position and converts, +1 when moving
  down).
- Create adds the new list to the top of the rootlist (`add_first`); delete
  removes it from the rootlist. That is also "Remove from Your Library" for
  playlists owned by someone else.
- The app refuses index-based edits (remove, move) when the loaded entries
  do not line up with the server's positions (episodes or removed items were
  dropped while loading: `TrackList::indices_exact`).
- **Who may edit:** Spotify's `capabilities.can_edit_items` (rootlist
  `MetaItem` / playlist `SelectedListContent`) decides, which also covers
  collaborative lists. Without it, the owner is compared (as a bare,
  case-insensitive username; `spotify:user:` and percent-encoding are
  stripped) with the session's canonical username (`session.username()`),
  so editing never depends on the profile fetch.
- **Unverified against the live service:** the endpoint paths and op shapes
  follow the playlist4 protocol but were only exercised against wiremock
  fixtures.

### Owner names and incomplete playlists (`app/meta.rs`)

- Owners arrive as usernames (`0d2x…`). Like the desktop client, the app
  resolves them with the profile view (cached 24 h in the SQLite HTTP
  cache), at most 20 per batch, only for strings that look like usernames.
  `spotify` is always shown as "Spotify", the user's own lists show their
  display name.
- Rootlist entries can come without name or picture (personalised and
  Spotify-made lists such as Daily Mix or Discover Weekly). Covers are read
  from `picture`, then `picture_size` (server-rendered mosaics), then an
  `image_url` format attribute; names from `name`, then `title`/`name`
  format attributes. Anything still missing is fetched lazily from the
  playlist itself (12 per batch, once per run) and, without a picture, drawn
  as a 2x2 mosaic of the first four distinct album covers. The filled
  library is written back to the cache.

### Resilience and request behaviour

- **Cache (SQLite `http_cache`).** TTLs: metadata and profile 24 h,
  playlists/rootlist 10 min, search 10 min, Home 30 min, recently played
  5 min, lyrics 7 days. A fresh entry is served without a request. A stale entry
  is served if the request fails (offline, 5xx, 429). Like/unlike updates the
  local collection index directly.
- **Collection sync** works like the desktop client: a full paging sync once,
  then deltas from the persisted sync token (kept across restarts), at most
  every 10 min.
- **Coalescing:** identical in-flight requests (method + URL + body digest)
  share one round trip.
- **Size limits:** a response over 2 MB is never cached, and the cache keeps
  at most 64 MB / 3000 responses, dropping the oldest first.
- **Concurrency:** at most 4 requests in flight; metadata is batched instead of
  fetched per item. Opening a screen never fans out into dozens of calls.
- **429:** `Retry-After` is honoured. Without the header, the backoff is
  1 s → 2 s → 4 s. Waits up to 10 s are slept inline (max 3 attempts). Longer
  waits put that **endpoint family** (first spclient path segment, or
  pathfinder) into a **cooldown**. Requests to it fail fast with "Spotify is
  limiting requests" instead of hammering the server, so a search 429 does not
  block collection writes. Server values are clamped to 15 min.
- **Coalescing runs detached:** each network request runs in its own task, so
  a caller that goes away (screen closed) never leaves a request or its
  concurrency permit parked.
- **5xx:** one retry after 500 ms. **401:** one single-flight token refresh.
- **Now playing** is driven only by librespot player events. Nothing polls
  Spotify for playback state.
- Internal APIs have server-side limits too. None of this guarantees
  that requests will not be limited.

### Pathfinder persisted queries

`GET /pathfinder/v1/query?operationName=<op>&variables=<json>&extensions={"persistedQuery":{"version":1,"sha256Hash":"<hash>"}}`

| Operation | Variables | Used for |
|---|---|---|
| `searchDesktop` | `searchTerm, offset 0, limit 10, numberOfTopResults 5, include* false` | Search |
| `home` | `timeZone` (from `TZ`, else UTC), `sp_t ""`, `facet ""`, `sectionItemsLimit 10` | Home shelves |

The hashes live **only** in `src/net/pathfinder.rs` (`DEFAULT_HASHES`).
**They ship as unverified defaults:** they are the last values known to us
and have never been checked against the live service. Spotify rotates them whenever the clients update. When a hash is
rejected (`PersistedQueryNotFound` in `errors`, or HTTP 400/404/412), the call
returns `Error::QueryRejected(op)`. That, or any other failure except being
offline or in a rate-limit cooldown (401, 403, 5xx, unexpected shapes),
makes the backend fall back to the spclient endpoint listed above, so Search
and Home keep working with less detail. A 200 that carries GraphQL `errors`
or null `data` is never cached.

#### Refreshing persisted queries

1. Open <https://open.spotify.com> in a desktop browser, log in, open the
   developer tools → Network, and filter for `pathfinder`.
2. Search for something (for `searchDesktop`), or load the Home page (for `home`).
3. In the request URL or body, copy `extensions.persistedQuery.sha256Hash`.
   Also check that `variables` still has the same names.
4. Without rebuilding: put the hashes in
   `~/.config/ferrotune/pathfinder.json`, e.g.
   `{"searchDesktop": "<64 hex chars>", "home": "<64 hex chars>"}`.
   Invalid entries are ignored with a log warning.
5. For a release: update `DEFAULT_HASHES`. If the response shape changed,
   update `parse_search` / `parse_home` and the fixtures
   `tests/fixtures/pathfinder_*.json`.

## 3. Audio: librespot (`src/net/playback.rs`)

- The session from §1 (`net/session.rs`). No librespot `Cache` is created,
  so librespot never writes credentials or audio to disk. After a reconnect
  the backends are re-bound to the new session (`Backend::rebind`).
- `librespot-playback` `Player` with the `SoftMixer` (volume 0..1 →
  0..65535) and the **rodio** backend (ALSA on Linux, CoreAudio on macOS,
  WASAPI on Windows). TLS via rustls + webpki roots (no OpenSSL).
- Bitrate 96/160/320 from config; `position_update_interval = 1 s`.
- The engine runs as a tokio task fed by a command channel
  (`Load/Play/Pause/Seek/SetVolume/Stop/Attach`) and emits
  `PlaybackEvent`s that the app turns into state. Events for a URI other than
  the current track are ignored (stale end-of-track after a skip).
- The queue (order, shuffle, repeat, "Next in queue") lives in the app
  (`app/state.rs::Queue`); librespot only plays single tracks. Gapless
  playback: when librespot reports `TimeToPreloadNextTrack`, the app sends
  `Preload` for `Queue::peek_next()` (not for repeat-one or a shuffle round
  that would be reshuffled), and the following `Load` uses the preloaded data.
- Audio session failures retry with exponential backoff (1 s → 60 s).
- A dead session (`Session::is_invalid()`, `Player::is_invalid()` or a closed
  player channel, checked every 2 s) emits `SessionLost`: the app pauses at
  the current position, reconnects, and reloads the track on play instead
  of skipping "unavailable" tracks.
- `--mock` uses `MockEngine`, a simulated clock with the same events.

### Spotify Connect controller (`net/connect_ctl.rs`)

- The engine starts an observer per session once the dealer has a
  connection id (i.e. Spotify Connect is enabled): one `PUT` as a hidden
  observer device `hobs_<device id>` (type `OBSERVER`, `can_be_player`
  false, `hidden` true, reason `PICKER_OPENED`) returns the cluster, and
  dealer pushes on `hm://connect-state/v1/cluster` keep it current. Opening
  the picker re-reads it.
- The picker lists the other visible devices (ours, observers, hidden and
  offline ones are left out), active first. Choosing one transfers the
  current remote session, or hands the local queue over as a `play` command
  (context, track and position). "This computer" transfers back.
- While another device is active, the player bar, seek bar, volume, shuffle,
  repeat and MPRIS show and control it (`pause`, `resume`, `skip_next`,
  `skip_prev`, `seek_to`, `set_shuffling_context`, `set_options`, volume
  `PUT`), with optimistic updates corrected by the next cluster push. The
  local player pauses when another device starts playing.
- **Unverified against the live service:** the observer registration and
  command shapes follow what the web player sends and librespot's own
  connect-state code; no real account was available here.

## 3a. Spotify Connect (`net/session.rs`, `net/playback.rs`)

- `librespot-connect` 0.8 `Spirc` registers the device **"Ferrotune on
  &lt;hostname&gt;"** (type Computer) with Spotify's connect-state service
  over the session's dealer websocket. This is session-based Connect: no
  zeroconf/mDNS discovery and no extra credentials. Phones and other clients
  logged into the same account see the device. `FERROTUNE_CONNECT=0` or
  `"connect": false` disables it.
- `Spirc::new` opens the session itself, after registering its listeners. If
  Connect fails for any reason other than bad credentials, Ferrotune logs a
  warning and opens a plain session without it.
- The Connect device has its own librespot `Player` and soft mixer. When
  another device casts to Ferrotune, the engine pauses the local queue's
  player, and the app shows the cast track ("Playing via Spotify Connect") in
  the player bar and MPRIS. Transport controls, seek and volume are sent to
  `Spirc` (`play/pause/next/prev/set_position_ms/set_volume`). Starting
  anything locally calls `Spirc::disconnect(true)`, which takes playback back.
- **Device id:** a random 40-hex id is created once and stored in the cache
  database (`kv.device_id`). It is reused by every session, so the phone does
  not list a new "Ferrotune" after each restart.
- **Two audio streams:** the Connect device has its own `Player`, because
  `Spirc` treats every event of its player as its own and cannot share one
  with the app's queue. Only one of them plays at a time: a cast pauses the
  local player from its first `TrackChanged`/`Loading`/`Playing`. With
  PipeWire or PulseAudio (the default on Plasma) both can hold the device. On
  bare ALSA hardware without dmix, which allows one opener only, the second
  stream may fail to open until the first is released (unverified).
- **Login failures:** librespot reports every access-point refusal as
  `PermissionDenied`. Only "Bad credentials" deletes the stored login.
  "Premium account required" keeps it and explains Premium, and
  TryAnotherAP / CouldNotValidateCredentials / ProtocolError are treated as
  temporary.
- Not done yet: local playback is not published to connect-state, so other
  devices do not see what Ferrotune plays locally, and Ferrotune cannot
  control other devices (no device picker). `Spirc` does not expose the
  device list; it needs a direct connect-state client.

## 3c. Spotify Soloist (`src/net/soloist/`, `src/app/soloist.rs`)

[Spotify Soloist](https://developer.spotify.com/documentation/soloist) is
Spotify's official headless Spotify Connect player. It does its own audio
decryption, so it plays for accounts whose librespot audio keys are refused
(§3, code 1). Ferrotune runs it as a child process and controls it; the
librespot session stays for login, metadata, browse and the Connect
controller.

**Setup (one extra step the official app does not have).** Soloist needs a
personal API key from the Spotify for Developers dashboard
(`https://developer.spotify.com/dashboard/soloist`, Premium). The setup card
opens that page and takes the pasted key. Keyring entry `soloist-api-key`;
`config.json` only stores `"engine": "soloist"` and `"soloist_output"`.

**Install.** `https://soloist-builds.spotifycdn.com/soloist_release_<arch>.tar.gz`
(`x86_64`, `arm64`, `arm32`, `osx_arm64`; Windows ships a .zip and is not
handled) is downloaded on first use into
`$XDG_DATA_HOME/ferrotune/soloist/` (dir mode 0700). Verified on 1.3.10
(x86_64): a 13.8 MB gzip archive (served as `application/x-tar`) with a flat
layout (`CHANGELOG.md`, `THIRD_PARTY_LICENSES.txt`, `soloist`), and a 36 MB
binary linked only against libc, libm and libatomic. It is streamed to
`download.part` (≤ 128 MB), then only the regular file `soloist` (top level
or one directory deep; no links, no `..`) is extracted (≤ 128 MB binary,
≤ 256 MB unpacked in total, header checksums verified) to `bin/soloist.new`,
made `0755`, run with `--version` (10 s timeout), and renamed over
`bin/soloist`. `build.json` keeps version, build time, ETag/Last-Modified and
the last check. **Never bundled or redistributed**: Spotify's documentation
forbids it, so each user's machine fetches it from Spotify.

**Expiry and updates.** Builds expire 90 days after their build date; an
expired build exits with code 10. The documentation names no version
endpoint, only "download the build again and replace the executable", so
Ferrotune asks the same URL with a conditional GET: `If-None-Match` with
the stored ETag (`If-Modified-Since` only when there is none); `304` =
nothing new. Verified: the CDN sends `etag`, `last-modified` and
`cache-control: private, max-age=0`, and answers both conditionals with 304.
Checks run at start when due and hourly while running: daily, every 6 h in
the last 14 days, always after expiry; **Update now** forces one; exit code
10 forces one and restarts. A build older than the installed one is never
installed. The build date comes from `--version`, which for 1.3.10 prints
`soloist 1.3.10.87 build 1795111339 (20261119) (g19412df6ea) (linux/x86_64)`:
the version is the first dotted number and the build date the 8-digit
`(YYYYMMDD)`, so this build expires 2027-02-17. The parser stays lenient (ISO
dates and Unix timestamps also work); without a date, the archive's
`Last-Modified` stands in.

**Process.** `soloist --device-name "<Connect device name>" --data-dir
<root>/data --cache-dir <cache>/soloist --cache-size 500 --ws 127.0.0.1:0
[--initial-volume <0-100>] [--pipewire-device <node>] --api-key <key>`. The
name is Ferrotune's Connect device name (Settings, default "Ferrotune on
&lt;host&gt;"). `--cache-size` is `soloist_cache_mb` in `config.json`
(default 500, at least 100): Soloist's own default is unlimited.
`--initial-volume` is Ferrotune's last volume (a volume change alone never
restarts the daemon). Soloist 1.3.10's `--help` lists no environment variable
for the key, only `-k/--api-key`, so the key is visible in
`/proc/<pid>/cmdline` to processes of the same user. Other flags it accepts
and Ferrotune does not use: `-s/--single-track`, `-v/--verbose`. stdout/stderr are logged at debug level
under target `soloist`, lines capped at 2 KB, with the key replaced by
`[REDACTED]` (plus the global token scrubber). On Linux the child gets
`PR_SET_PDEATHSIG(SIGTERM)`; quit sends SIGTERM (3 s grace, then SIGKILL).
A `soloist.pid` left by a crashed run is stopped only if `/proc/<pid>/exe` is
our binary. Crashes restart with backoff 1 s → 60 s (reset after 60 s up).
Re-pair runs `--pair` (no `--ws`) and restarts the daemon when it exits.

**WebSocket API.** `--ws 127.0.0.1:0` makes Soloist write the OS-assigned
port to `<data-dir>/ws.port` (`soloist ctl` reads `ws.addr` / `ws.port`
too). Ferrotune reads `ws.port` and `ws.addr` when present, else
127.0.0.1 (loopback only; stale files removed before each start), frames
≤ 1 MB.
Commands (`{"type":"command","command":…}`): `play` (+`uri`), `pause`,
`skip_next`, `skip_prev`, `seek` (`position_ms`), `set_volume` (0–100),
`set_shuffle`, `set_repeat_track`/`set_repeat_context` (the documented
pairs), `add_to_queue` (tracks), `activate`, `get_state`. `play` (with or
without a URI) is preceded by `activate` when Soloist is not the active
device, so pressing play in Ferrotune moves playback to this computer. Events:
`auth_state` (logged_in → Ready, else Needs pairing), `playback_state`,
`track_changed`, `playback_changed`, `position_sync` (position extrapolated
from `timestamp_ms` × `speed`), `volume_changed`, `options_changed`,
`device_changed`, `queue_changed`, `command_result`, `error`. Unknown events
and fields are ignored.

**Routing.** With Soloist on, librespot runs no local player and announces
no Connect device (one "Ferrotune" in every picker: Soloist). The Connect
controller still needs a dealer connection id, so the session opens the
dealer itself (`hm://pusher/v1/connections/`, no device registered). The
Soloist device is the cluster device with our Connect name (until it shows
up: a local placeholder id). `App::connect_task` sends every controller
operation for it to `soloist_intercept`: transport, seek, volume, shuffle,
repeat and transfer-without-id go over the WebSocket; a context started at
a track (`skip_to`) or a transfer go through connect-state to the device id;
without an id, `play` gets the track (or context) URI. While Soloist plays,
the player bar and MPRIS follow its WebSocket events (cluster pushes for it
are ignored while the WebSocket is up). Requests made before Soloist is
ready wait and are sent when it is.

**Pairing.** Soloist logs in only through Spotify Connect pairing on the
local network (zeroconf). Ferrotune does **not** pair it itself: that would
mean implementing the zeroconf `addUser` blob exchange with the session's
credentials, which Spotify does not document for third parties. The card
asks the user to pick the device once in the Spotify app on their phone and
disappears when `auth_state.logged_in` turns true.

**Verified against the real 1.3.10 build** (without running it): the
archive layout and sizes, our extractor on the real archive (an opt-in test,
`FERROTUNE_SOLOIST_ARCHIVE=… cargo test --test soloist real_archive --
--ignored`), the CDN headers and 304 answers, and the `--version` / `--help`
text (taken from the binary's output). **Unverified:** the binary was never
started here, so the WebSocket greeting and event shapes (written from the
reference examples), `soloist ctl` output, pairing, audio and Soloist's
memory use are not checked.

## 3b. Keyboard shortcuts, menus, dialogs

- `app/shortcuts.rs::map` turns a key press into a `Ui` message (pure, tested).
  The subscription (`event::listen_with`) ignores key presses a widget already
  handled, so typing in the search box never triggers a shortcut. `Ctrl` is
  `Cmd` on macOS. The help dialog's table (`shortcuts::HELP`) sits next to the
  mapping.
- Right clicks are caught by `ui/context_area.rs`, a small wrapper widget that
  reports the cursor position in window coordinates (iced's `mouse_area`
  does not). `app/menu.rs` is the pure model: `entries(target, liked)` lists
  what a menu offers and `place` keeps it inside the window.
- Optimistic playlist edits take an `EditSnapshot` (library list + detail
  view) and restore it when the server refuses. Overlapping edits of the same
  playlist roll back to the older snapshot.

## 4. MPRIS (`src/net/mpris.rs`, Linux only)

Bus name `org.mpris.MediaPlayer2.ferrotune`, object `/org/mpris/MediaPlayer2`,
`DesktopEntry = dev.ferro.ferrotune` (matches the Wayland `app_id`).

- Implemented with `mpris-server` 0.9 (`Server` + our own `Send` struct
  implementing `RootInterface` / `PlayerInterface`).
- The app pushes a `MediaSnapshot` after every update; the server diffs it
  against the previous one and emits `PropertiesChanged` for
  `PlaybackStatus, Metadata, Volume, Shuffle, LoopStatus, CanGoNext,
  CanGoPrevious`, plus `Seeked` on every user seek (`seek_seq`) or large
  position jump. `Position` is extrapolated on read while playing.
- Metadata strings are sent as plain text, as the MPRIS spec requires (no
  markup escaping). Ferrotune sends no desktop notifications.
- Track ids: `/dev/ferro/ferrotune/track/<base62 id>`. `mpris:artUrl` is the
  300 px `i.scdn.co` image.
- Methods `Play, Pause, PlayPause, Stop, Next, Previous, Seek, SetPosition`
  and the writable `Volume, Shuffle, LoopStatus` map to UI messages;
  `Raise` focuses the window, `Quit` exits. `OpenUri` is not supported.
- Loop mapping: Repeat off ↔ `None`, context ↔ `Playlist`, track ↔ `Track`.
- `tests/mpris.rs` checks properties and method calls on a private bus
  (`dbus-run-session`).

## 5. UI reference

Values in `src/ui/theme.rs` (recreated, not copied from Spotify assets):

| Token | Value |
|---|---|
| Window background | `#000000` |
| Panels (sidebar/main/queue) | `#121212`, radius 8, 8 px gaps |
| Elevated / hover / selected | `#282828` / `#1f1f1f` / `#2a2a2a` |
| Text primary / secondary / muted | `#ffffff` / `#b3b3b3` / `#6a6a6a` |
| Accent (play buttons, active state) | `#1ed760` |
| Track row hover / selected | white 10 % / 30 % |
| Toast | `#2e77d0` |
| Font | Inter 400/600/700; hero title 72 px, section 24 px, rows 16/13 px |
| Sizes | sidebar 300 px, queue 320 px, top bar 64 px, player 72 px, rows 56 px, card art 156 px, header art 192 px |

Headers use a vertical gradient from the (brightness-normalised) average
colour of the cover art into the panel colour.
