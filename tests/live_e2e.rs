//! Live end-to-end tests against a REAL Newznab indexer (e.g. NZBHydra2) and
//! the official TMDB API — no mocks. They exercise the exact resolution
//! pipeline playback uses: TMDB details → indexer fan-out → parse + rank.
//!
//! The scenarios encode real-world regressions around One Piece, where three
//! distinct bugs bite:
//!   1. Early episodes (S01E01) resolve to the 2023 Netflix live-action
//!      remake instead of the 1999 anime.
//!   2. Mid-series arcs like Dressrosa (TMDB S16/S17) return no candidates
//!      at all, because most releases use absolute episode numbering that a
//!      `t=tvsearch&season=&ep=` query never matches.
//!   3. Late episodes resolve to releases whose numbering does not match the
//!      requested episode.
//!
//! Every test is `#[ignore]`d so `cargo test` stays offline. To run them:
//!
//!   1. `cp tests/live_settings.example.toml tests/live_settings.toml` and
//!      fill in your indexer URL/key and TMDB API key (the file is
//!      gitignored). Alternatively set `LIVE_E2E__*` env vars, e.g.
//!      `LIVE_E2E__TMDB__API_KEY=...`, `LIVE_E2E__INDEXER__BASE_URL=...`.
//!   2. `cargo test --test live_e2e -- --ignored --nocapture`
//!
//! The optional `[provider]` section enables the full playback test, which
//! streams real bytes from your Usenet provider through ffmpeg.

use axum_test::TestServer;
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::Deserialize;
use serde_json::{json, Value};
use usenet_streaming_server::{api, config::AppConfig, state::AppState};

/// TMDB id of "One Piece" (the 1999 Toei anime).
const ONE_PIECE_ANIME: i64 = 37854;
/// TMDB id of "ONE PIECE" (the 2023 Netflix live-action remake).
const ONE_PIECE_LIVE_ACTION: i64 = 111110;

/// The number of top-ranked candidates session creation actually tries, in
/// order (mirrors MAX_ATTEMPTS in the streaming API): if any of these is a
/// wrong-show/wrong-episode release, playback can silently play it.
const PLAYBACK_WINDOW: usize = 5;

const API_KEY: &str = "live-e2e-key";

// ---- Live settings ----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LiveSettings {
    tmdb: TmdbSettings,
    indexer: IndexerSettings,
    /// Optional: enables the full streaming test.
    provider: Option<ProviderSettings>,
}

#[derive(Debug, Deserialize)]
struct TmdbSettings {
    api_key: String,
}

#[derive(Debug, Deserialize)]
struct IndexerSettings {
    #[serde(default = "default_indexer_name")]
    name: String,
    base_url: String,
    api_key: String,
}

fn default_indexer_name() -> String {
    "live-indexer".into()
}

#[derive(Debug, Deserialize)]
struct ProviderSettings {
    host: String,
    #[serde(default = "default_nntp_port")]
    port: u16,
    #[serde(default = "default_true")]
    use_tls: bool,
    username: Option<String>,
    password: Option<String>,
    #[serde(default = "default_max_connections")]
    max_connections: i64,
}

fn default_nntp_port() -> u16 {
    563
}

fn default_true() -> bool {
    true
}

fn default_max_connections() -> i64 {
    10
}

const SETTINGS_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/live_settings.toml");

impl LiveSettings {
    /// Load from `tests/live_settings.toml` merged with `LIVE_E2E__*` env
    /// vars. Returns None (test skips) when neither exists; panics when
    /// configuration is present but incomplete, so a typo cannot silently
    /// turn the suite into a no-op.
    fn load() -> Option<Self> {
        let file_exists = std::path::Path::new(SETTINGS_FILE).exists();
        let env_set = std::env::vars().any(|(k, _)| k.starts_with("LIVE_E2E__"));
        if !file_exists && !env_set {
            eprintln!(
                "SKIP: live settings not configured. Copy tests/live_settings.example.toml \
                 to tests/live_settings.toml (or set LIVE_E2E__* env vars) to run live tests."
            );
            return None;
        }
        let settings = Figment::new()
            .merge(Toml::file(SETTINGS_FILE))
            .merge(Env::prefixed("LIVE_E2E__").split("__"))
            .extract()
            .expect("invalid live E2E settings (tests/live_settings.toml / LIVE_E2E__* env)");
        Some(settings)
    }
}

// ---- Harness ----------------------------------------------------------------

struct Live {
    server: TestServer,
    indexer_id: i64,
}

/// Boot the full app in-process (in-memory DB, real TMDB base URL) and
/// configure the TMDB key and the live indexer through the public API,
/// exactly as a user would.
async fn live_app() -> Option<Live> {
    let settings = LiveSettings::load()?;
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config).await.expect("test state");
    let server = TestServer::new(api::router(state));

    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_api_key": settings.tmdb.api_key }))
        .await;
    assert_eq!(response.status_code(), 200, "configuring TMDB key failed");

    let response = server
        .post("/api/v1/settings/indexers")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "name": settings.indexer.name,
            "base_url": settings.indexer.base_url,
            "api_key": settings.indexer.api_key,
        }))
        .await;
    assert_eq!(response.status_code(), 200, "configuring indexer failed");
    let indexer: Value = response.json();
    let indexer_id = indexer["id"].as_i64().expect("indexer id");

    Some(Live { server, indexer_id })
}

/// GET /releases for one episode; returns the ranked candidate list.
async fn releases(live: &Live, tmdb_id: i64, season: u32, episode: u32) -> Vec<Value> {
    let response = live
        .server
        .get("/api/v1/releases")
        .add_query_param("tmdb_id", tmdb_id)
        .add_query_param("type", "tv")
        .add_query_param("season", season)
        .add_query_param("episode", episode)
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(
        response.status_code(),
        200,
        "GET /releases S{season:02}E{episode:02} failed: {}",
        response.text()
    );
    let body: Value = response.json();
    body["candidates"].as_array().cloned().unwrap_or_default()
}

/// Candidates playback would consider, in rank order (rejected ones dropped).
fn accepted(candidates: &[Value]) -> Vec<&Value> {
    candidates
        .iter()
        .filter(|c| c["rejected"].is_null())
        .collect()
}

fn title(candidate: &Value) -> &str {
    candidate["raw"]["title"].as_str().unwrap_or("")
}

fn log_top(label: &str, all: &[Value], accepted: &[&Value]) {
    eprintln!(
        "{label}: {} raw / {} accepted candidate(s)",
        all.len(),
        accepted.len()
    );
    for c in accepted.iter().take(PLAYBACK_WINDOW) {
        eprintln!("  [{}] {}", c["score"], title(c));
    }
    if accepted.is_empty() {
        for c in all.iter().take(PLAYBACK_WINDOW) {
            eprintln!("  rejected ({}): {}", c["rejected"], title(c));
        }
    }
}

/// TMDB metadata for the show, fetched through the app's own API.
async fn tv_details(live: &Live, tmdb_id: i64) -> Value {
    let response = live
        .server
        .get(&format!("/api/v1/tv/{tmdb_id}"))
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200, "tv details failed");
    response.json()
}

/// episode_count of a season, from the show's TMDB season list.
fn season_episode_count(show: &Value, season: u32) -> Option<u32> {
    show["seasons"].as_array()?.iter().find_map(|s| {
        (s["season_number"].as_u64()? as u32 == season)
            .then(|| s["episode_count"].as_u64().map(|c| c as u32))
            .flatten()
    })
}

/// Absolute episode number: episodes of all regular seasons before `season`,
/// plus `episode`. For long-running anime this is the number most release
/// titles actually carry ("One Piece - 0629").
fn absolute_episode(show: &Value, season: u32, episode: u32) -> u32 {
    let before: u32 = show["seasons"]
        .as_array()
        .map(|seasons| {
            seasons
                .iter()
                .filter_map(|s| {
                    let n = s["season_number"].as_u64()? as u32;
                    (n >= 1 && n < season).then(|| s["episode_count"].as_u64().unwrap_or(0) as u32)
                })
                .sum()
        })
        .unwrap_or(0);
    before + episode
}

// ---- Title heuristics ---------------------------------------------------------

/// Detect releases that text-match "One Piece S01E01" but are NOT the 1999
/// anime. Title-only classification is inherently fuzzy (that fuzziness is
/// the bug these tests pin down), so this checks the strong markers seen in
/// the wild:
///   - an explicit 2023 / "live action" tag (the Netflix remake),
///   - "Romance Dawn" as the episode title (the remake's E01; the anime's
///     E01 is "I'm Luffy! ..."),
///   - "Heroines" (the "ONE PIECE HEROINES" spin-off shorts),
///   - HDR / Dolby Vision in the parsed attributes — no release of the 1999
///     TV anime is HDR, only the remake is.
fn wrong_show_reason(candidate: &Value) -> Option<String> {
    let title = title(candidate);
    let t = title.to_lowercase().replace(['.', '_'], " ");
    for marker in ["2023", "live action", "romance dawn", "heroines"] {
        if t.contains(marker) {
            return Some(format!("'{title}' (marker: '{marker}')"));
        }
    }
    if candidate["parsed"]["hdr"] == json!(true)
        || candidate["parsed"]["dolby_vision"] == json!(true)
    {
        return Some(format!(
            "'{title}' (HDR/DV — the 1999 anime has no HDR releases)"
        ));
    }
    None
}

/// Extract an SxxEyy pair from a release title.
fn season_episode(title: &str) -> Option<(u32, u32)> {
    let re = regex::Regex::new(r"(?i)\bS(\d{1,2})[\s._-]*E(\d{1,4})").unwrap();
    let caps = re.captures(title)?;
    Some((caps[1].parse().ok()?, caps[2].parse().ok()?))
}

/// Standalone 3-4 digit tokens that plausibly are absolute episode numbers
/// (years and bare resolution values are excluded; "1080p"/"x264" never match
/// because of the word boundary).
fn absolute_numbers(title: &str) -> Vec<u32> {
    let re = regex::Regex::new(r"\b(\d{3,4})\b").unwrap();
    re.captures_iter(title)
        .filter_map(|c| c[1].parse::<u32>().ok())
        .filter(|n| !(1900..=2099).contains(n))
        .filter(|n| ![264, 265, 480, 576, 720, 1080, 1440, 2160].contains(n))
        .collect()
}

/// Episode-only marker like `E574` / `EP1162` (absolute-style numbering).
fn episode_only(title: &str) -> Option<u32> {
    let re = regex::Regex::new(r"(?i)\bEp?[\s._]?(\d{2,4})\b").unwrap();
    re.captures(title).and_then(|c| c[1].parse().ok())
}

/// Human-readable reason the title's numbering contradicts the requested
/// episode, or None when it matches / carries no numbering at all.
fn episode_mismatch(title: &str, season: u32, episode: u32, absolute: u32) -> Option<String> {
    if let Some((s, e)) = season_episode(title) {
        if (s == season && e == episode) || e == absolute {
            return None;
        }
        return Some(format!(
            "'{title}' carries S{s:02}E{e:02}, requested S{season:02}E{episode:02} (absolute {absolute})"
        ));
    }
    // Standalone numbering follows the anime absolute convention.
    if let Some(e) = episode_only(title) {
        if e != absolute {
            return Some(format!(
                "'{title}' carries E{e}, requested absolute {absolute} (S{season:02}E{episode:02})"
            ));
        }
        return None;
    }
    let numbers = absolute_numbers(title);
    if !numbers.is_empty() && !numbers.contains(&absolute) {
        return Some(format!(
            "'{title}' carries absolute number(s) {numbers:?}, requested {absolute} (S{season:02}E{episode:02})"
        ));
    }
    None
}

/// Positive proof: the title carries numbering AND it matches the requested
/// episode. Unnumbered names (recaps, packs, specials) do not count.
fn episode_verified(title: &str, season: u32, episode: u32, absolute: u32) -> bool {
    (season_episode(title).is_some()
        || episode_only(title).is_some()
        || !absolute_numbers(title).is_empty())
        && episode_mismatch(title, season, episode, absolute).is_none()
}

// ---- Tests --------------------------------------------------------------------

/// Sanity: TMDB and the indexer are reachable with the configured keys. Run
/// this first when the other tests fail — it separates config problems from
/// real regressions.
#[tokio::test]
#[ignore = "live: requires tests/live_settings.toml (real indexer + TMDB)"]
async fn live_setup_tmdb_and_indexer_are_reachable() {
    let Some(live) = live_app().await else { return };

    // TMDB: searching "one piece" must surface both the anime and the
    // live-action remake — the ambiguity the other tests are about.
    let response = live
        .server
        .get("/api/v1/search")
        .add_query_param("query", "one piece")
        .add_query_param("type", "tv")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body: Value = response.json();
    let ids: Vec<i64> = body["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|r| r["tmdb_id"].as_i64())
        .collect();
    assert!(
        ids.contains(&ONE_PIECE_ANIME),
        "TMDB search must find the One Piece anime ({ONE_PIECE_ANIME}); got {ids:?}"
    );
    assert!(
        ids.contains(&ONE_PIECE_LIVE_ACTION),
        "TMDB search must find the live-action remake ({ONE_PIECE_LIVE_ACTION}); got {ids:?}"
    );

    // Indexer connectivity through the app's own test endpoint.
    let response = live
        .server
        .post(&format!(
            "/api/v1/settings/indexers/{}/test",
            live.indexer_id
        ))
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let result: Value = response.json();
    assert_eq!(
        result["ok"],
        json!(true),
        "indexer test failed: {}",
        result["error"]
    );
}

/// Bug 1: playing an early anime episode must not pick the 2023 Netflix
/// live-action remake. Asserts over the whole window of candidates session
/// creation would try, since any of them can end up on screen.
#[tokio::test]
#[ignore = "live: requires tests/live_settings.toml (real indexer + TMDB)"]
async fn early_one_piece_episode_picks_the_anime_not_the_live_action() {
    let Some(live) = live_app().await else { return };

    let candidates = releases(&live, ONE_PIECE_ANIME, 1, 1).await;
    let accepted = accepted(&candidates);
    log_top("One Piece S01E01", &candidates, &accepted);
    assert!(
        !accepted.is_empty(),
        "no accepted candidates for One Piece S01E01 ({} total, all rejected?)",
        candidates.len()
    );

    let offenders: Vec<String> = accepted
        .iter()
        .take(PLAYBACK_WINDOW)
        .filter_map(|c| wrong_show_reason(c))
        .collect();
    assert!(
        offenders.is_empty(),
        "playback window for the ANIME S01E01 contains wrong-show releases: {offenders:#?}"
    );

    // The top pick is what actually plays: its numbering must fit episode 1.
    let top = title(accepted[0]);
    assert_eq!(
        episode_mismatch(top, 1, 1, 1),
        None,
        "top candidate numbering is wrong"
    );
}

/// Bug 2: mid-series arcs (Dressrosa, TMDB S16/S17) must return candidates.
/// Today the tvsearch season/ep query comes back empty because releases for
/// these arcs use absolute numbering.
#[tokio::test]
#[ignore = "live: requires tests/live_settings.toml (real indexer + TMDB)"]
async fn dressrosa_arc_episodes_have_release_candidates() {
    let Some(live) = live_app().await else { return };
    let show = tv_details(&live, ONE_PIECE_ANIME).await;

    let mut missing = Vec::new();
    for season in [16u32, 17u32] {
        let count = season_episode_count(&show, season)
            .unwrap_or_else(|| panic!("TMDB lists no season {season} for One Piece"));
        let episode = (count / 2).max(1);
        let absolute = absolute_episode(&show, season, episode);
        eprintln!(
            "Dressrosa-era check: S{season:02}E{episode:02} (absolute episode {absolute}, season has {count} episodes)"
        );

        let candidates = releases(&live, ONE_PIECE_ANIME, season, episode).await;
        let accepted = accepted(&candidates);
        log_top(
            &format!("One Piece S{season:02}E{episode:02}"),
            &candidates,
            &accepted,
        );
        if accepted.is_empty() {
            missing.push(format!(
                "S{season:02}E{episode:02} (absolute {absolute}): {} raw candidates, 0 accepted",
                candidates.len()
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "no playable candidates for Dressrosa-era episodes: {missing:#?}"
    );
}

/// Bug 3: late episodes must resolve to releases whose numbering actually
/// matches the requested episode, despite absolute-numbered release titles.
#[tokio::test]
#[ignore = "live: requires tests/live_settings.toml (real indexer + TMDB)"]
async fn late_episode_candidates_match_the_requested_episode() {
    let Some(live) = live_app().await else { return };
    let show = tv_details(&live, ONE_PIECE_ANIME).await;

    // Latest regular season with a meaningful episode count, mid-season.
    let (season, count) = show["seasons"]
        .as_array()
        .expect("seasons")
        .iter()
        .filter_map(|s| {
            let n = s["season_number"].as_u64()? as u32;
            let count = s["episode_count"].as_u64()? as u32;
            (n >= 1 && count >= 10).then_some((n, count))
        })
        .max_by_key(|(n, _)| *n)
        .expect("a late season with episodes");
    let episode = (count / 2).max(1);
    let absolute = absolute_episode(&show, season, episode);
    eprintln!(
        "Late-episode check: S{season:02}E{episode:02} (absolute episode {absolute}, season has {count} episodes)"
    );

    let candidates = releases(&live, ONE_PIECE_ANIME, season, episode).await;
    let accepted = accepted(&candidates);
    log_top(
        &format!("One Piece S{season:02}E{episode:02}"),
        &candidates,
        &accepted,
    );
    assert!(
        !accepted.is_empty(),
        "no accepted candidates for S{season:02}E{episode:02} ({} total)",
        candidates.len()
    );

    let mismatches: Vec<String> = accepted
        .iter()
        .take(PLAYBACK_WINDOW)
        .filter_map(|c| episode_mismatch(title(c), season, episode, absolute))
        .collect();
    assert!(
        mismatches.is_empty(),
        "playback window contains wrong-episode releases: {mismatches:#?}"
    );
}

/// The user-level guarantee across the whole show: every season must resolve
/// at least one correctly-numbered 1080p release, and season 1's playback
/// window must be free of Netflix live-action releases. One episode per
/// season (mid-season, to avoid premiere/finale specials skew).
#[tokio::test]
#[ignore = "live: requires tests/live_settings.toml (real indexer + TMDB)"]
async fn every_season_resolves_a_correct_1080p_release() {
    let Some(live) = live_app().await else { return };
    let show = tv_details(&live, ONE_PIECE_ANIME).await;

    let seasons: Vec<(u32, u32)> = show["seasons"]
        .as_array()
        .expect("seasons")
        .iter()
        .filter_map(|s| {
            let n = s["season_number"].as_u64()? as u32;
            let count = s["episode_count"].as_u64()? as u32;
            (n >= 1 && count > 0).then_some((n, count))
        })
        .collect();
    assert!(
        seasons.len() >= 20,
        "One Piece has 20+ seasons: {seasons:?}"
    );

    let mut failures = Vec::new();
    for &(season, count) in &seasons {
        let episode = (count / 2).max(1);
        let absolute = absolute_episode(&show, season, episode);
        let candidates = releases(&live, ONE_PIECE_ANIME, season, episode).await;
        let accepted = accepted(&candidates);

        // A verified 1080p must exist AND sit in the playback window, so
        // pressing play actually reaches it.
        let good_1080p = accepted.iter().take(PLAYBACK_WINDOW).position(|c| {
            c["parsed"]["resolution"] == json!("1080p")
                && episode_verified(title(c), season, episode, absolute)
        });
        match good_1080p {
            Some(i) => eprintln!(
                "S{season:02}E{episode:02} (abs {absolute}): OK — {} accepted, 1080p pick at #{}: {}",
                accepted.len(),
                i + 1,
                title(accepted[i])
            ),
            None => {
                log_top(
                    &format!("S{season:02}E{episode:02} (abs {absolute})"),
                    &candidates,
                    &accepted,
                );
                failures.push(format!(
                    "S{season:02}E{episode:02} (abs {absolute}): no verified 1080p candidate \
                     in the playback window ({} raw, {} accepted)",
                    candidates.len(),
                    accepted.len()
                ));
            }
        }

        if season == 1 {
            let offenders: Vec<String> = accepted
                .iter()
                .take(PLAYBACK_WINDOW)
                .filter_map(|c| wrong_show_reason(c))
                .collect();
            if !offenders.is_empty() {
                failures.push(format!(
                    "S01 playback window contains wrong-show releases: {offenders:?}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} seasons failed:\n{}",
        failures.len(),
        seasons.len(),
        failures.join("\n")
    );
}

// ---- Full playback (optional, needs a [provider] section) ---------------------

fn ffmpeg_available() -> bool {
    let works = |bin: &str| {
        std::process::Command::new(bin)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    works("ffmpeg") && works("ffprobe")
}

/// The user's literal repro: press play on One Piece episode 1 and check what
/// comes out. Streams real bytes from the configured Usenet provider through
/// ffmpeg, so it needs the optional `[provider]` settings plus ffmpeg/ffprobe
/// on PATH. The session must start, expose an HLS playlist, and the chosen
/// release must be the anime episode that was asked for.
#[tokio::test]
#[ignore = "live: requires tests/live_settings.toml with [provider] + ffmpeg"]
async fn playing_the_first_episode_streams_the_anime() {
    let Some(settings) = LiveSettings::load() else {
        return;
    };
    let Some(provider) = &settings.provider else {
        eprintln!("SKIP: no [provider] section in live settings; playback test needs one.");
        return;
    };
    if !ffmpeg_available() {
        eprintln!("SKIP: ffmpeg/ffprobe not on PATH.");
        return;
    }

    // Real socket (not TestServer): ffmpeg/ffprobe read the virtual file
    // back through the server's loopback URL.
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config).await.expect("test state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let state = state.with_loopback_base(&base);
    let router = api::router(state);
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("serve");
    });

    // Session creation downloads the NZB, health-checks articles and probes
    // the stream — give it a generous timeout.
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .expect("http client");
    let put = |path: &str, body: Value, method: reqwest::Method| {
        let http = http.clone();
        let url = format!("{base}/api/v1{path}");
        async move {
            http.request(method, url)
                .header("x-api-key", API_KEY)
                .json(&body)
                .send()
                .await
                .expect("request")
        }
    };

    let r = put(
        "/settings/app",
        json!({ "tmdb_api_key": settings.tmdb.api_key }),
        reqwest::Method::PUT,
    )
    .await;
    assert!(r.status().is_success(), "configuring TMDB key failed");
    let r = put(
        "/settings/indexers",
        json!({
            "name": settings.indexer.name,
            "base_url": settings.indexer.base_url,
            "api_key": settings.indexer.api_key,
        }),
        reqwest::Method::POST,
    )
    .await;
    assert!(r.status().is_success(), "configuring indexer failed");
    let r = put(
        "/settings/providers",
        json!({
            "name": "live-provider",
            "host": provider.host,
            "port": provider.port,
            "use_tls": provider.use_tls,
            "username": provider.username,
            "password": provider.password,
            "max_connections": provider.max_connections,
        }),
        reqwest::Method::POST,
    )
    .await;
    assert!(r.status().is_success(), "configuring provider failed");

    let response = put(
        "/stream/sessions",
        json!({
            "tmdb_id": ONE_PIECE_ANIME,
            "media_type": "tv",
            "season": 1,
            "episode": 1,
        }),
        reqwest::Method::POST,
    )
    .await;
    let status = response.status();
    let body: Value = response.json().await.expect("session response json");
    assert_eq!(
        status, 200,
        "session creation for One Piece S01E01 failed: {body}"
    );

    let chosen = &body["chosen_release"];
    let chosen_title = title(chosen).to_string();
    eprintln!("Playing: {chosen_title}");
    assert_eq!(
        wrong_show_reason(chosen),
        None,
        "pressed play on the ANIME S01E01 but got a different show"
    );
    assert_eq!(
        episode_mismatch(&chosen_title, 1, 1, 1),
        None,
        "chosen release numbering is wrong"
    );

    // The HLS entry point must be servable.
    let session_id = body["session_id"].as_str().expect("session id");
    let playlist = http
        .get(format!("{base}/api/v1/stream/{session_id}/master.m3u8"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("playlist request");
    assert_eq!(playlist.status(), 200);
    let playlist = playlist.text().await.expect("playlist body");
    assert!(
        playlist.starts_with("#EXTM3U"),
        "master playlist is not HLS: {playlist}"
    );

    // Clean up the session so background readahead stops.
    let _ = http
        .delete(format!("{base}/api/v1/stream/{session_id}"))
        .header("x-api-key", API_KEY)
        .send()
        .await;
}
