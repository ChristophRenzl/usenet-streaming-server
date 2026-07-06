-- Audio-fingerprint intro detection (Jellyfin-style), best-effort. Episodes
-- are fingerprinted as they play; siblings of the same season are compared to
-- locate the shared opening. The chapter-based intro (0001/ffprobe) is still
-- the first source and takes priority — these tables only feed the common case
-- where a release has no chapters at all.

-- One chromaprint fingerprint per episode (the first ~240s of audio), stored
-- as the little-endian bytes of its u32 sub-fingerprint list.
CREATE TABLE episode_fingerprints (
    tmdb_id INTEGER NOT NULL,
    season INTEGER NOT NULL,
    episode INTEGER NOT NULL,
    fingerprint BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (tmdb_id, season, episode)
);

-- The detected intro for a whole season (openings are ~identical across a
-- season), cached so the next episode played gets its Skip-Intro immediately.
CREATE TABLE season_intros (
    tmdb_id INTEGER NOT NULL,
    season INTEGER NOT NULL,
    intro_start_secs REAL NOT NULL,
    intro_end_secs REAL NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (tmdb_id, season)
);
