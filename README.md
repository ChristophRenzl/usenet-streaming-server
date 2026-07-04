# Usenet Streaming Server

Self-hosted backend that lets a client search movies/TV via TMDB and stream the
content **on-the-fly directly from Usenet** — no full download required — with
optional server-side downloads. Backend only: the JSON API is designed for
future tvOS and web clients.

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

Then open Swagger UI at `http://localhost:8080/docs` and configure via the API:

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

## API overview

Full interactive documentation: Swagger UI at `/docs`
(OpenAPI JSON at `/api-docs/openapi.json`). All endpoints are under `/api/v1`
and require the `X-Api-Key` header (or `?apikey=` for media URLs).

| Area | Endpoints |
|---|---|
| Search | `GET /search`, `GET /movies/{id}`, `GET /tv/{id}[/season/{n}[/episode/{e}]]` |
| Releases | `GET /releases?tmdb_id=…` — ranked candidates for manual override |
| Streaming | `POST /stream/sessions`, `GET /stream/{id}/master.m3u8`, `GET /stream/{id}/raw` (byte ranges), `DELETE /stream/{id}` |
| Downloads | `POST /downloads`, `GET /downloads[/{id}]`, `DELETE /downloads/{id}` |
| History | `GET/POST /history`, `DELETE /history/{id}` |
| Settings | `/settings/preferences`, `/settings/providers`, `/settings/indexers`, `/settings/app` |

## Configuration

Bootstrap settings (port, API key, paths, cache size) live in
[config.example.toml](config.example.toml) or `APP_*` environment variables.
Everything operational — NNTP providers, indexers, TMDB key, release
preferences — is managed through the API and stored in SQLite.

## MVP limitations

- Store-mode (uncompressed) RAR releases only; compressed archives are
  rejected for streaming with a clear error and the next candidate is tried.
- No par2 repair: releases failing the segment health check are skipped; a
  segment missing mid-stream aborts with an error.
- Remux only — no video transcoding. Audio is transcoded to AAC when needed
  (e.g. DTS); video is always stream-copied.
- Single user, single API key (data model is multi-user-ready).
- No automation (monitoring, auto-grab, renaming) — on-demand only.

## License

[MIT](LICENSE)

## Legal note

This software is a generic streaming backend for content on Usenet. You are
responsible for complying with the laws of your jurisdiction and the terms of
your Usenet/indexer providers.
