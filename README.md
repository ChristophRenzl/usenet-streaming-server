# Usenet Streaming Server

Self-hosted backend that lets a client search movies/TV via TMDB and stream the
content **on-the-fly directly from Usenet** — no full download required — with
optional server-side downloads. The JSON API is designed for tvOS and web
clients; a built-in web admin UI at `/` handles all configuration.

## How it works

1. Client searches TMDB through the server and picks a movie or episode.
2. The server queries your Newznab indexers, parses and ranks the candidate
   releases against your preferences (resolution, codecs, size, blocked terms),
   health-checks the winner via NNTP `STAT`, and falls back automatically.
3. A **virtual file** is built over the NZB: byte ranges are served by fetching
   only the needed article segments, yEnc-decoding them, and mapping through
   store-mode RAR offsets. Nothing is written to disk.
4. ffmpeg remuxes the virtual file to HLS (fMP4, video stream-copy, audio
   copied or transcoded to AAC) for AVPlayer/tvOS — or clients that play MKV
   directly use the raw byte-range endpoint.

## Quick start (Docker)

```sh
mkdir -p config data
cp config.example.toml config/config.toml   # set auth.api_key!
docker compose -f docker-compose.example.yml up -d
```

Then open the **web admin UI at `http://localhost:8080/`**, sign in with your
API key and configure everything in the browser: Usenet providers, indexers,
the TMDB key, quality preferences, downloads and API-key rotation — no curl
required.

Prefer the command line? Swagger UI lives at `http://localhost:8080/docs`
(click **Authorize** and paste the API key to use "Try it out"), or configure
via the API directly:

```sh
# NNTP provider
curl -X POST localhost:8080/api/v1/settings/providers \
  -H "X-Api-Key: $KEY" -H "Content-Type: application/json" \
  -d '{"name":"main","host":"news.example.com","port":563,"use_tls":true,
       "username":"u","password":"p","max_connections":20}'

# Newznab indexer
curl -X POST localhost:8080/api/v1/settings/indexers \
  -H "X-Api-Key: $KEY" -H "Content-Type: application/json" \
  -d '{"name":"indexer","base_url":"https://indexer.example.com","api_key":"..."}'

# TMDB API key
curl -X PUT localhost:8080/api/v1/settings/app \
  -H "X-Api-Key: $KEY" -H "Content-Type: application/json" \
  -d '{"tmdb_api_key":"..."}'
```

Start watching:

```sh
# 1. search
curl "localhost:8080/api/v1/search?query=inception" -H "X-Api-Key: $KEY"
# 2. start a playback session (server picks + health-checks a release)
curl -X POST localhost:8080/api/v1/stream/sessions \
  -H "X-Api-Key: $KEY" -H "Content-Type: application/json" \
  -d '{"tmdb_id":27205,"type":"movie"}'
# → { "session_id": "...", "hls_master_url": "/api/v1/stream/<id>/master.m3u8", ... }
```

## Building from source

Requires Rust (stable) and ffmpeg on `PATH`.

```sh
cargo build --release
cp config.example.toml config.toml   # set auth.api_key
./target/release/usenet-streaming-server --config config.toml
```

Run tests with `cargo test`.

## Web admin UI

The server ships a built-in admin panel at `/` (no extra container, works
offline on your LAN). It covers first-time setup (a dashboard checklist shows
what is still missing) and all operational settings:

- **Usenet Providers** — add/edit/test/delete NNTP servers
- **Indexers** — add/edit/test/delete Newznab indexers
- **TMDB** — set the metadata API key
- **Preferences** — resolutions, codecs, audio language (fixed or `original`, e.g. Japanese for anime), size limit, blocked terms
- **Downloads** — watch progress, cancel or delete jobs
- **Security** — rotate the server API key

## API overview

Full interactive documentation: Swagger UI at `/docs`
(OpenAPI JSON at `/api-docs/openapi.json`). All endpoints are under `/api/v1`
and require the `X-Api-Key` header (or `?apikey=` for media URLs); in Swagger
UI, click **Authorize** to set the key for "Try it out".

| Area | Endpoints |
|---|---|
| Search | `GET /search`, `GET /movies/{id}`, `GET /tv/{id}[/season/{n}[/episode/{e}]]` |
| Discovery | `GET /trending?media_type=all\|movie\|tv&window=day\|week`, `GET /movies/popular`, `GET /movies/top_rated`, `GET /tv/popular`, `GET /tv/top_rated` (all take `?page=`) |
| Watchlist | `GET/POST /watchlist`, `GET /watchlist/status?tmdb_id=…&media_type=…`, `DELETE /watchlist/{tmdb_id}?media_type=…` |
| Releases | `GET /releases?tmdb_id=…[&max_resolution=…]` — ranked candidates for manual override; `max_resolution` caps quality to what the device supports (also accepted in `POST /stream/sessions` and `POST /downloads` bodies) |
| Streaming | `POST /stream/sessions`, `GET /stream/{id}/master.m3u8`, `GET /stream/{id}/raw` (byte ranges), `DELETE /stream/{id}` |
| Subtitles | `GET /subtitles/search?tmdb_id=…`, `POST /stream/{id}/subtitles` (attach), `POST /stream/{id}/subtitles/{language}/offset` (nudge timing) |
| Downloads | `POST /downloads`, `GET /downloads[/{id}]`, `DELETE /downloads/{id}` |
| History | `GET/POST /history`, `DELETE /history/{id}` |
| Settings | `/settings/preferences`, `/settings/providers`, `/settings/indexers`, `/settings/app` |

## Subtitles

Optional [OpenSubtitles](https://www.opensubtitles.com) integration. Add a free
consumer API key on the **Subtitles** page of the web UI (or via
`PUT /api/v1/settings/app` with `opensubtitles_api_key`); an OpenSubtitles
account (`opensubtitles_username` / `opensubtitles_password`) is optional and
lifts the anonymous daily download quota. Subtitles are **best-effort and
non-fatal** — playback works without a key, and any subtitle failure is logged
and skipped rather than failing the session.

### Configure the API key once at deploy time (the "Jellyfin experience")

OpenSubtitles requires a consumer `Api-Key` on every request, while
username/password only grant download quota. To avoid making every user paste an
API key, an **operator can supply a default key once at deploy time** and then
users only ever manage their OpenSubtitles username/password:

```
APP_SUBTITLES__OPENSUBTITLES_DEFAULT_API_KEY=your-consumer-key
```

(equivalently `subtitles.opensubtitles_default_api_key` in the config file — see
[config.example.toml](config.example.toml)). Effective-key resolution: a
per-user `opensubtitles_api_key` stored via the API **wins**; otherwise the
operator default is used; if neither is set, subtitles stay disabled. When the
default is active the web UI shows *"Using the server's built-in API key"* and
the per-user key becomes an optional override.

The default key is **operator-supplied only and never bundled** — hardcoding a
key would violate OpenSubtitles' per-consumer terms and be rate-limited. Get a
free consumer key at
[opensubtitles.com/consumers](https://www.opensubtitles.com/consumers).

Subtitles are delivered as **HLS `#EXT-X-MEDIA:TYPE=SUBTITLES` renditions**, so
tvOS AVPlayer offers them natively in its own subtitle menu (each track is a
single-segment WebVTT converted from the source SubRip). Three things keep the
timing accurate:

- **moviehash auto-sync (release-accurate).** When a session starts with
  `subtitle_languages` requested, the server computes the OpenSubtitles/OSDb
  moviehash of the media file (size + first/last 64 KiB) and passes it to the
  search. Hash-matched subtitles were timed against *this exact release*, so
  they line up perfectly and are ranked first.
- **fps drift correction.** For a chosen subtitle that is *not* hash-matched but
  reports its own frame rate, cue timestamps are linearly rescaled by
  `media_fps / subtitle_fps` (media fps comes from ffprobe) to remove the
  constant drift a frame-rate mismatch causes. Hash-matched subtitles are
  assumed correct and never rescaled.
- **manual offset (the nudge fallback).**
  `POST /api/v1/stream/{session_id}/subtitles/{language}/offset` with body
  `{"ms": <i64>}` shifts a track's cues (positive = later, negative = earlier;
  negatives clamp to 0). `{language}` addresses the track by its primary
  language subtag (case-insensitive: `en` also matches `en-US`). `ms` is an
  **absolute** offset relative to the original timing — repeated nudges replace
  the previous offset rather than compounding — and the WebVTT is re-emitted in
  place (the playlist URI is unchanged, so the player just reloads the track).
  Returns the updated track, or 404 when no track of that language exists.

The standalone `GET /subtitles/search` stays TMDB-based (no file, so no
moviehash); hash matching and fps/offset correction apply only in the session
auto-attach path. A session's attached tracks are reported in the
`subtitle_tracks` array of the `POST /stream/sessions` response.

## Configuration

Bootstrap settings (port, API key, paths, cache size) live in
[config.example.toml](config.example.toml) or `APP_*` environment variables.
Everything operational — NNTP providers, indexers, TMDB key, release
preferences — is managed through the web UI / API and stored in SQLite.

## Security

A single API key (`auth.api_key` in the config file, or `APP_AUTH__API_KEY`)
protects the whole `/api/v1` surface. Only `/health`, the web admin UI at `/`
and the Swagger docs are unauthenticated — the UI asks for the key before it
can do anything.

The key can be **rotated from the browser** (Security page) or via
`PUT /api/v1/settings/app` with `{"api_key": "..."}` (minimum 16 characters).
The rotated key is stored in the database; from then on **two keys are valid
at the same time**:

1. the rotated key — hand this one to your clients (Apple TV app, …)
2. the bootstrap key from the config file / environment — kept valid on
   purpose as a recovery path: if you lose the rotated key, sign in with the
   config key (visible in your Docker/NAS environment settings) and rotate
   again.

`GET /api/v1/settings/app` reports the active key masked (last 4 characters)
plus whether a rotated key is in effect.

## MVP limitations

- Store-mode (uncompressed) RAR releases only; compressed archives are
  rejected for streaming with a clear error and the next candidate is tried.
- par2 repair is a **download-only fallback**, not on-the-fly: a release too
  damaged to stream but recoverable from its par2 recovery files is classified
  `repairable`. Starting a session on such a release returns `202 repairing`
  with a `download_id` — the server downloads everything, runs `par2 repair`,
  and the finished file then plays from disk. A segment missing mid-stream
  still aborts that stream with an error (streaming cannot repair live).
- Remux only — no video transcoding. Audio is transcoded to AAC when needed
  (e.g. DTS); video is always stream-copied.
- Single user, single API key (plus the recovery key described under
  Security; the data model is multi-user-ready).
- No automation (monitoring, auto-grab, renaming) — on-demand only.

## License

[MIT](LICENSE)

## Legal note

This software is a generic streaming backend for content on Usenet. You are
responsible for complying with the laws of your jurisdiction and the terms of
your Usenet/indexer providers.
