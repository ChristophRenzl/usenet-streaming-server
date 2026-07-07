//! Post-search verification that a release actually belongs to the requested
//! title. Indexers fall back to fuzzy free-text matching (and the raw `q=`
//! search is free-text by design), so a search for the 1999 "One Piece" anime
//! happily returns the 2023 Netflix live-action show of the same name. Every
//! check here is *conflict-based*: missing information passes, only positive
//! contradictions reject.

use crate::indexer::RawRelease;

/// What the search was for, from TMDB. All optional fields degrade to
/// "cannot check" rather than rejecting.
#[derive(Debug, Clone, Default)]
pub struct Expected<'a> {
    /// TMDB title of the movie or show.
    pub title: &'a str,
    /// Release year (movie) / first-air year (show).
    pub year: Option<i32>,
    pub tvdb_id: Option<i64>,
    /// IMDb id, with or without the `tt` prefix.
    pub imdb_id: Option<&'a str>,
    /// Requested season/episode (tv only).
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// Absolute episode number across seasons (anime-style numbering),
    /// when derivable from TMDB season data.
    pub absolute_episode: Option<u32>,
    /// The episode's air year; releases of currently-airing episodes are
    /// sometimes tagged with it, which must not count as a remake claim.
    pub episode_air_year: Option<i32>,
    /// The episode's TMDB title. Same-name shows text-match the same
    /// `SxxEyy` (the 2023 "One Piece" remake, the "ONE PIECE HEROINES"
    /// spin-off), so the episode-title words in the release name are often
    /// the only distinguishing signal; used by [`score_adjustment`].
    pub episode_title: Option<&'a str>,
}

/// Why a release does not match the request, or None when it (plausibly)
/// does. Intended for `RankedRelease::rejected`, so the reason is shown in
/// release pickers and a user can still pin the release to override.
pub fn mismatch_reason(raw: &RawRelease, expected: &Expected) -> Option<String> {
    if let (Some(got), Some(want)) = (raw.tvdb_id, expected.tvdb_id) {
        if got != want {
            return Some(format!(
                "different show (indexer tvdbid {got}, expected {want})"
            ));
        }
    }
    if let (Some(got), Some(want)) = (raw.imdb_id.as_deref(), expected.imdb_id) {
        let want = want.trim_start_matches("tt").trim_start_matches('0');
        if !want.is_empty() && got != want {
            return Some(format!(
                "different title (indexer imdbid {got}, expected {want})"
            ));
        }
    }

    let release_tokens = tokens(strip_leading_tags(&raw.title));
    let expected_tokens = tokens(expected.title);
    // The expected title must be a prefix of the release name; `rest` is
    // everything after it (edition qualifiers, numbering, quality tags).
    let Some(rest) = match_title_prefix(&release_tokens, &expected_tokens) else {
        return Some(format!("release name does not match '{}'", expected.title));
    };

    // A year in the qualifier region (between the title and the first
    // quality/numbering marker) names a specific edition of the title;
    // reject when it contradicts the expected year (± 1 for off-by-one
    // air/release dates). This is what separates "One.Piece.2023.S01E01"
    // from the 1999 anime.
    if let Some(want) = expected.year {
        let qualifier_end = rest
            .iter()
            .position(|t| is_marker(t) && year_token(t).is_none())
            .unwrap_or(rest.len());
        if let Some(got) = rest[..qualifier_end].iter().find_map(|t| year_token(t)) {
            if (got - want).abs() > 1 {
                return Some(format!("year {got} does not match {want}"));
            }
        }
        // Remake years also appear right AFTER the numbering marker
        // ("One.Piece.S01E01.2023.Netflix..."), so scan up to the first
        // *quality* marker (numbering does not end this region). Here only
        // a year NEWER than the show rejects: older years are content
        // (episodes titled "1969" exist), the episode's own air year is
        // legitimate on currently-airing releases, and a bare year deep in
        // the quality tags ("...1080p.2024.WEB") is release metadata.
        let quality_end = rest
            .iter()
            .position(|t| {
                is_marker(t)
                    && year_token(t).is_none()
                    && season_episode_token(t).is_none()
                    && season_pack_token(t).is_none()
            })
            .unwrap_or(rest.len());
        let newest_legit = expected.episode_air_year.unwrap_or(want).max(want);
        if let Some(got) = rest[..quality_end]
            .iter()
            .filter_map(|t| year_token(t))
            .max()
        {
            if got > newest_legit + 1 {
                return Some(format!("year {got} is newer than the show ({want})"));
            }
        }
    }

    // Season/episode: only checked when the release carries SxxEyy-style
    // numbering. Releases without it may use absolute numbering (checked
    // next) or be complete packs.
    if let (Some(season), Some(episode)) = (expected.season, expected.episode) {
        if let Some((got_season, got_episodes)) = season_episodes(rest) {
            if got_season != season {
                return Some(format!("season {got_season} does not match S{season}"));
            }
            if !got_episodes.is_empty() && !got_episodes.contains(&episode) {
                return Some(format!(
                    "episode numbering does not include S{season}E{episode}"
                ));
            }
        } else if let Some(absolute) = expected.absolute_episode {
            // Anime-style absolute numbering: "One Piece - 0901" / "E901".
            // Reject only on a *conflicting* number; a release with no
            // numbering at all could be a pack.
            if let Some(got) = absolute_number(rest) {
                if got != absolute {
                    return Some(format!("absolute episode {got} does not match {absolute}"));
                }
            }
        }
    }

    None
}

pub const EPISODE_TITLE_MATCH_BONUS: i64 = 400;
pub const EPISODE_TITLE_MISMATCH_PENALTY: i64 = -600;
pub const UNNUMBERED_PENALTY: i64 = -300;

/// Words too generic to prove an episode-title match.
const TITLE_STOPWORDS: &[&str] = &[
    "the", "a", "an", "of", "and", "to", "in", "is", "im", "its", "no", "der", "die", "das", "le",
    "la", "les",
];

/// Ranking adjustment from episode-level evidence in the release name, for
/// episode searches. Complements [`mismatch_reason`], which only rejects on
/// hard contradictions — these cases are soft signals:
///
/// - The episode-title segment (words between the numbering marker and the
///   first quality tag) shares a distinctive word with the TMDB episode
///   title: strong evidence for the right episode of the right show (bonus).
///   `...S01E01.Im.Luffy.The.Man.Whos...` vs the anime's "I'm Luffy! ...".
/// - The segment is present but shares nothing: usually a same-name show
///   whose episode carries the same number (`...S01E01.ROMANCE.DAWN...` is
///   the Netflix remake, `...S01E01.HEROINES...` a spin-off). Penalized, not
///   rejected — translated titles legitimately share no words.
/// - No numbering at all (recaps, specials, packs): cannot be verified, so
///   it must rank below verified candidates while staying available.
pub fn score_adjustment(raw: &RawRelease, expected: &Expected) -> i64 {
    if expected.season.is_none() || expected.episode.is_none() {
        return 0;
    }
    let release_tokens = tokens(strip_leading_tags(&raw.title));
    let expected_tokens = tokens(expected.title);
    let Some(rest) = match_title_prefix(&release_tokens, &expected_tokens) else {
        return 0; // rejected by mismatch_reason anyway
    };

    let numbering = rest.iter().position(|t| {
        season_episode_token(t).is_some()
            || season_pack_token(t).is_some()
            || absolute_token(t).is_some()
    });
    let Some(numbering) = numbering else {
        return UNNUMBERED_PENALTY;
    };

    let Some(expected_title) = expected.episode_title else {
        return 0;
    };
    let distinctive = |word: &String| word.len() >= 3 && !TITLE_STOPWORDS.contains(&word.as_str());
    let expected_words: Vec<String> = tokens(expected_title)
        .into_iter()
        .filter(distinctive)
        .collect();
    if expected_words.is_empty() {
        return 0;
    }
    let segment: Vec<&String> = rest[numbering + 1..]
        .iter()
        .take_while(|t| !ends_title_segment(t))
        .filter(|t| distinctive(t))
        .collect();
    if segment.iter().any(|w| expected_words.contains(w)) {
        EPISODE_TITLE_MATCH_BONUS
    } else if !segment.is_empty() {
        EPISODE_TITLE_MISMATCH_PENALTY
    } else {
        0
    }
}

/// Token that terminates the episode-title segment of a release name:
/// quality/numbering markers plus source/codec/language boilerplate that
/// [`is_marker`] (tuned for ending the *title* part) does not need to know.
fn ends_title_segment(token: &str) -> bool {
    if is_marker(token) || token.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    matches!(
        token,
        "episode"
            | "ep"
            | "cr"
            | "nf"
            | "amzn"
            | "dsnp"
            | "ger"
            | "eng"
            | "jpn"
            | "sub"
            | "subs"
            | "msubs"
            | "multisub"
            | "multisubs"
            | "vostfr"
            | "subbed"
            | "dubbed"
            | "raw"
            | "batch"
            | "remastered"
            | "uncensored"
            | "anime"
            | "hevc"
            | "avc"
            | "xvid"
            | "10bit"
            | "8bit"
    ) || token.starts_with("aac")
        || token.starts_with("ddp")
        || token.starts_with("dd5")
        || token.starts_with("dd2")
        || token.starts_with("dd7")
        || token.starts_with("h26")
        || token.starts_with("x26")
        || token.starts_with("hdr")
}

/// Release names from fansub groups lead with one or more bracketed tags
/// (`[SubsPlease] One Piece - 1162 (1080p)`, `[df68] One Piece ...`); strip
/// them so the title-prefix check sees the actual title.
fn strip_leading_tags(name: &str) -> &str {
    let mut rest = name.trim_start();
    while let Some(stripped) = rest.strip_prefix('[') {
        match stripped.find(']') {
            Some(end) => rest = stripped[end + 1..].trim_start(),
            None => break,
        }
    }
    rest
}

/// Lowercased alphanumeric tokens of a release name or title.
fn tokens(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Match the expected title tokens as a prefix of the release tokens and
/// return the remaining tokens after the title. A leading "the" is ignored
/// on both sides. None when the release does not start with the title.
fn match_title_prefix<'a>(tokens: &'a [String], expected: &[String]) -> Option<&'a [String]> {
    fn strip_the(t: &[String]) -> &[String] {
        match t.first() {
            Some(first) if first == "the" => &t[1..],
            _ => t,
        }
    }
    let name = strip_the(tokens);
    let expected = strip_the(expected);
    if expected.is_empty() {
        return Some(name);
    }
    if name.len() >= expected.len() && name[..expected.len()] == expected[..] {
        Some(&name[expected.len()..])
    } else {
        None
    }
}

/// Token that ends the title part of a release name: episode numbering,
/// a year, or a well-known quality marker.
fn is_marker(token: &str) -> bool {
    if season_episode_token(token).is_some() || season_pack_token(token).is_some() {
        return true;
    }
    if year_token(token).is_some() {
        return true;
    }
    matches!(
        token,
        "480p"
            | "576p"
            | "720p"
            | "1080p"
            | "1080i"
            | "2160p"
            | "4k"
            | "uhd"
            | "bluray"
            | "brrip"
            | "bdrip"
            | "remux"
            | "web"
            | "webdl"
            | "webrip"
            | "hdtv"
            | "dvdrip"
            | "hdrip"
            | "proper"
            | "repack"
            | "internal"
            | "complete"
            | "multi"
            | "dual"
            | "german"
            | "french"
            | "italian"
            | "spanish"
            | "japanese"
            | "korean"
    )
}

fn year_token(token: &str) -> Option<i32> {
    let year: i32 = token.parse().ok()?;
    (1900..=2100).contains(&year).then_some(year)
}

/// "s01e03" → (1, [3]); "s01e01e02" / multi-episode variants → (1, [1, 2]).
fn season_episode_token(token: &str) -> Option<(u32, Vec<u32>)> {
    let rest = token.strip_prefix('s')?;
    let episode_start = rest.find('e')?;
    let season: u32 = rest[..episode_start].parse().ok()?;
    let episodes: Vec<u32> = rest[episode_start..]
        .split('e')
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<u32>())
        .collect::<Result<_, _>>()
        .ok()?;
    if episodes.is_empty() {
        return None;
    }
    Some((season, episodes))
}

/// "s01" (a season pack) → 1. Not confused with "s01e03".
fn season_pack_token(token: &str) -> Option<u32> {
    let rest = token.strip_prefix('s')?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) || rest.len() > 2 {
        return None;
    }
    rest.parse().ok()
}

/// "16x25" → (16, [25]) — the alternate scene form of SxxEyy. Resolutions
/// like "1920x1080" don't fit the 1-2 digit season shape.
fn season_x_episode_token(token: &str) -> Option<(u32, Vec<u32>)> {
    let (season, episode) = token.split_once('x')?;
    if season.is_empty()
        || season.len() > 2
        || episode.len() < 2
        || episode.len() > 3
        || !season.chars().all(|c| c.is_ascii_digit())
        || !episode.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some((season.parse().ok()?, vec![episode.parse().ok()?]))
}

/// First SxxEyy (with optional multi-episode suffix), NNxNN or season-pack
/// marker in the post-title tokens: (season, episodes); episodes is empty
/// for a pack.
fn season_episodes(rest: &[String]) -> Option<(u32, Vec<u32>)> {
    for token in rest {
        if let Some((season, episodes)) = season_episode_token(token) {
            return Some((season, episodes));
        }
        if let Some((season, episodes)) = season_x_episode_token(token) {
            return Some((season, episodes));
        }
        if let Some(season) = season_pack_token(token) {
            return Some((season, Vec::new()));
        }
    }
    None
}

/// A single token that plausibly is an absolute episode number: a bare 2-4
/// digit number ("One Piece - 0901") or an exx token with 3+ digits
/// ("E0901"). Year-looking numbers are skipped — they are handled by the
/// year check — and single digits are ignored (channel-layout debris like
/// "DD5.1").
fn absolute_token(token: &str) -> Option<u32> {
    let digits = token.strip_prefix('e').unwrap_or(token);
    if digits.is_empty() || digits.len() > 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if year_token(digits).is_some() {
        return None;
    }
    let minimum_len = if token.starts_with('e') { 3 } else { 2 };
    if digits.len() < minimum_len {
        return None; // exx below 100 is ordinary episode numbering; 1-digit bare is noise
    }
    digits.parse().ok()
}

/// Absolute episode number among the post-title tokens.
fn absolute_number(rest: &[String]) -> Option<u32> {
    rest.iter().find_map(|t| absolute_token(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(title: &str) -> RawRelease {
        RawRelease {
            title: title.into(),
            guid: format!("guid-{title}"),
            nzb_url: "https://x/a.nzb".into(),
            size_bytes: None,
            posted_at: None,
            indexer_id: 1,
            indexer_name: "test".into(),
            tvdb_id: None,
            imdb_id: None,
        }
    }

    fn anime_one_piece(season: u32, episode: u32, absolute: Option<u32>) -> Expected<'static> {
        Expected {
            title: "One Piece",
            year: Some(1999),
            tvdb_id: Some(81797),
            season: Some(season),
            episode: Some(episode),
            absolute_episode: absolute,
            ..Default::default()
        }
    }

    #[test]
    fn group_tag_prefixed_names_match_the_title() {
        // Fansub releases lead with bracketed group tags; the title check
        // must see past them.
        for name in [
            "[SubsPlease] One Piece - 1100 (1080p) [ABCD12EF]",
            "[Erai-raws] One Piece - 1100 [1080p CR WEB-DL AVC AAC][MultiSub]",
            "[df68] One Piece Season 06 - Part 2 [1080p][x264][JPN][SUB]",
        ] {
            let raw = release(name);
            let expected = anime_one_piece(2, 1, Some(1100));
            assert_ne!(
                mismatch_reason(&raw, &expected),
                Some(format!("release name does not match 'One Piece'")),
                "group tag must be skipped for: {name}"
            );
        }
        // The SubsPlease/Erai names above carry absolute 1100 and must pass
        // outright.
        let raw = release("[SubsPlease] One Piece - 1100 (1080p) [ABCD12EF]");
        assert_eq!(
            mismatch_reason(&raw, &anime_one_piece(2, 1, Some(1100))),
            None
        );
    }

    #[test]
    fn season_x_episode_form_is_verified() {
        let expected = anime_one_piece(16, 25, Some(667));
        let right = release("One Piece - 16x25 - The Aim is Building R!");
        assert_eq!(mismatch_reason(&right, &expected), None);
        let wrong_season = release("One Piece - 3x25 - Something Else");
        assert!(mismatch_reason(&wrong_season, &expected).is_some());
    }

    #[test]
    fn episode_title_match_scores_up_mismatch_scores_down() {
        let expected = Expected {
            episode_title: Some("I'm Luffy! The Man Who Will Become the Pirate King!"),
            ..anime_one_piece(1, 1, None)
        };
        let matching = release(
            "One.Piece.S01E01.Im.Luffy.The.Man.Whos.Gonna.Be.King.of.the.Pirates.REPACK.1080p.CR.WEB-DL.DUAL.DDP2.0.H.264-Kitsune",
        );
        assert_eq!(
            score_adjustment(&matching, &expected),
            EPISODE_TITLE_MATCH_BONUS
        );
        // The Netflix remake episode is titled "Romance Dawn", the spin-off
        // tags itself "HEROINES" — same SxxEyy, different show.
        let remake = release("ONE.PIECE.S01E01.ROMANCE.DAWN.1080p.NF.WEB-DL.AAC5.1.H.264-OldT");
        assert_eq!(
            score_adjustment(&remake, &expected),
            EPISODE_TITLE_MISMATCH_PENALTY
        );
        let spinoff = release("One.Piece.S01E01.HEROINES.1080p.CR.WEB-DL.AAC2.0.H.264-VARYG");
        assert_eq!(
            score_adjustment(&spinoff, &expected),
            EPISODE_TITLE_MISMATCH_PENALTY
        );
        // No episode-title segment at all: neutral.
        let bare = release("ONE.PIECE.S01E01.1080p.NF.WEB-DL.DDP5.1.H.264-BdC");
        assert_eq!(score_adjustment(&bare, &expected), 0);
    }

    #[test]
    fn unnumbered_releases_are_penalized_for_episode_searches() {
        let expected = anime_one_piece(3, 7, Some(84));
        // A recap special has no numbering: cannot be rejected, must sink.
        let recap = release("[Erai-raws] One Piece - Egghead (RECAP) [1080p CR WEB-DL AVC AAC]");
        assert_eq!(mismatch_reason(&recap, &expected), None);
        assert_eq!(score_adjustment(&recap, &expected), UNNUMBERED_PENALTY);
        // A numbered release of the right episode is not penalized.
        let numbered = release("[Erai-raws] One Piece - 084 [1080p CR WEB-DL AVC AAC]");
        assert_eq!(score_adjustment(&numbered, &expected), 0);
    }

    #[test]
    fn episode_boilerplate_is_not_an_episode_title() {
        // "Episode.1162" after the numbering marker is boilerplate, not an
        // episode title — no mismatch penalty for the correct release.
        let expected = Expected {
            episode_title: Some("A Fierce Battle!"),
            ..anime_one_piece(23, 7, Some(1162))
        };
        let raw = release(
            "One.Piece.EP1162.Episode.1162.1080p.NF.WEB-DL.JPN.AAC2.0.H.264.MSubs-ToonsHub",
        );
        assert_eq!(mismatch_reason(&raw, &expected), None);
        assert_eq!(score_adjustment(&raw, &expected), 0);
    }

    #[test]
    fn foreign_translation_with_shared_word_gets_the_bonus() {
        let expected = Expected {
            episode_title: Some("I'm Luffy! The Man Who Will Become the Pirate King!"),
            ..anime_one_piece(1, 1, None)
        };
        // French translation still shares the distinctive word "Luffy".
        let raw = release(
            "One Piece - S01E01 - Je suis Luffy ! Celui qui deviendra Roi des Pirates ! x265-Amen",
        );
        assert_eq!(score_adjustment(&raw, &expected), EPISODE_TITLE_MATCH_BONUS);
    }

    #[test]
    fn movie_searches_get_no_episode_adjustment() {
        let expected = Expected {
            title: "Some Movie",
            year: Some(2020),
            ..Default::default()
        };
        let raw = release("Some.Movie.2020.1080p.BluRay.x264-GRP");
        assert_eq!(score_adjustment(&raw, &expected), 0);
    }

    #[test]
    fn year_after_the_numbering_marker_still_flags_a_remake() {
        // The remake year can sit AFTER SxxEyy: "One.Piece.S01E01.2023...".
        let raw = release("One.Piece.S01E01.2023.Netflix.WEB-DL.1080p.x264.DDP-AREY");
        let reason = mismatch_reason(&raw, &anime_one_piece(1, 1, None)).expect("must reject");
        assert!(reason.contains("2023"), "reason was: {reason}");
    }

    #[test]
    fn older_year_in_title_is_episode_content_not_a_remake() {
        // Stargate SG-1 (1997) has an episode literally titled "1969".
        let expected = Expected {
            title: "Stargate SG-1",
            year: Some(1997),
            season: Some(2),
            episode: Some(21),
            episode_air_year: Some(1999),
            ..Default::default()
        };
        let raw = release("Stargate.SG-1.S02E21.1969.DVDRip.x264-GRP");
        assert_eq!(mismatch_reason(&raw, &expected), None);
    }

    #[test]
    fn episode_air_year_tag_is_not_a_remake_claim() {
        // Currently-airing episodes of an old show are sometimes tagged
        // with their air year.
        let expected = Expected {
            episode_air_year: Some(2026),
            ..anime_one_piece(23, 7, Some(1162))
        };
        let raw = release("One.Piece.S23E07.2026.1080p.WEB.x264-GRP");
        assert_eq!(mismatch_reason(&raw, &expected), None);
    }

    #[test]
    fn conflicting_tvdb_attr_rejects() {
        let mut raw = release("One.Piece.S01E01.1080p.NF.WEB-DL.DDP5.1.x264-GRP");
        raw.tvdb_id = Some(422090); // the live-action show
        let reason = mismatch_reason(&raw, &anime_one_piece(1, 1, None)).unwrap();
        assert!(reason.contains("tvdbid 422090"));
    }

    #[test]
    fn matching_or_absent_tvdb_attr_passes() {
        let mut raw = release("One.Piece.S01E01.1080p.WEB-DL.x264-GRP");
        assert_eq!(mismatch_reason(&raw, &anime_one_piece(1, 1, None)), None);
        raw.tvdb_id = Some(81797);
        assert_eq!(mismatch_reason(&raw, &anime_one_piece(1, 1, None)), None);
    }

    #[test]
    fn conflicting_imdb_attr_rejects_movies() {
        let mut raw = release("Movie.2026.1080p.BluRay.x264-GRP");
        raw.imdb_id = Some("123".into());
        let expected = Expected {
            title: "Movie",
            imdb_id: Some("tt0000456"),
            ..Default::default()
        };
        assert!(mismatch_reason(&raw, &expected).is_some());
        raw.imdb_id = Some("456".into());
        assert_eq!(mismatch_reason(&raw, &expected), None);
    }

    #[test]
    fn year_in_release_name_must_match() {
        // The Netflix live action tagged with its year.
        let raw = release("One.Piece.2023.S01E01.1080p.NF.WEB-DL-GRP");
        let reason = mismatch_reason(&raw, &anime_one_piece(1, 1, None)).unwrap();
        assert!(reason.contains("year 2023"));

        // Off-by-one years pass (air-date vs production-year confusion).
        let raw = release("Show.2019.S01E01.1080p.WEB-GRP");
        let expected = Expected {
            title: "Show",
            year: Some(2020),
            season: Some(1),
            episode: Some(1),
            ..Default::default()
        };
        assert_eq!(mismatch_reason(&raw, &expected), None);
    }

    #[test]
    fn different_title_rejects() {
        let expected = Expected {
            title: "Rick and Morty",
            ..Default::default()
        };
        assert!(mismatch_reason(&release("Solar.Opposites.S01E01.1080p-GRP"), &expected).is_some());
        assert_eq!(
            mismatch_reason(&release("Rick.and.Morty.S09E01.1080p.WEB-GRP"), &expected),
            None
        );
    }

    #[test]
    fn leading_the_is_ignored() {
        let expected = Expected {
            title: "The Office",
            ..Default::default()
        };
        assert_eq!(
            mismatch_reason(&release("Office.US.S01E01.720p-GRP"), &expected),
            None
        );
    }

    #[test]
    fn wrong_season_or_episode_rejects() {
        assert!(mismatch_reason(
            &release("One.Piece.S02E05.1080p.WEB-GRP"),
            &anime_one_piece(1, 1, None)
        )
        .is_some());
        assert!(mismatch_reason(
            &release("One.Piece.S01E07.1080p.WEB-GRP"),
            &anime_one_piece(1, 1, None)
        )
        .is_some());
    }

    #[test]
    fn multi_episode_release_containing_requested_episode_passes() {
        let raw = release("Show.S01E01E02.1080p.WEB-GRP");
        let expected = Expected {
            title: "Show",
            season: Some(1),
            episode: Some(2),
            ..Default::default()
        };
        assert_eq!(mismatch_reason(&raw, &expected), None);
    }

    #[test]
    fn matching_season_pack_passes_wrong_season_pack_rejects() {
        let expected = Expected {
            title: "Show",
            season: Some(1),
            episode: Some(3),
            ..Default::default()
        };
        assert_eq!(
            mismatch_reason(&release("Show.S01.1080p.WEB-GRP"), &expected),
            None
        );
        assert!(mismatch_reason(&release("Show.S03.1080p.WEB-GRP"), &expected).is_some());
    }

    #[test]
    fn absolute_numbering_matches_anime_episodes() {
        // "One Piece - 0901" style fansub naming.
        assert_eq!(
            mismatch_reason(
                &release("One.Piece.E0901.1080p.WEB.x264-GRP"),
                &anime_one_piece(19, 10, Some(901))
            ),
            None
        );
        assert_eq!(
            mismatch_reason(
                &release("One Piece - 901 [1080p]"),
                &anime_one_piece(19, 10, Some(901))
            ),
            None
        );
        // A different absolute number is a different episode.
        assert!(mismatch_reason(
            &release("One.Piece.E0777.1080p.WEB.x264-GRP"),
            &anime_one_piece(19, 10, Some(901))
        )
        .is_some());
    }

    #[test]
    fn resolution_numbers_are_not_mistaken_for_absolute_episodes() {
        // 1080/720 tokens carry a "p" suffix and never parse as numbers;
        // a bare year is skipped by the year guard.
        let raw = release("One.Piece.E0901.1080p.2024.WEB-GRP");
        assert_eq!(
            mismatch_reason(&raw, &anime_one_piece(19, 10, Some(901))),
            None
        );
    }

    #[test]
    fn movie_year_check() {
        let expected = Expected {
            title: "Movie",
            year: Some(2020),
            ..Default::default()
        };
        assert_eq!(
            mismatch_reason(&release("Movie.2020.1080p.BluRay-GRP"), &expected),
            None
        );
        assert!(mismatch_reason(&release("Movie.1994.1080p.BluRay-GRP"), &expected).is_some());
    }
}
