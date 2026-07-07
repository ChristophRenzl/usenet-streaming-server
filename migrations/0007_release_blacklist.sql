-- Releases the user marked as bad from the player (broken A/V sync, wrong
-- content, unwatchable quality). Keyed by the exact release title: the same
-- underlying release is commonly listed by several indexers under different
-- guids, but its title is stable across them. Blacklisted titles are rejected
-- during ranking so automatic selection picks the next-best candidate; a
-- manual guid pin still overrides.
CREATE TABLE release_blacklist (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL UNIQUE,
    -- What was being played when it was flagged, for context in listings.
    tmdb_id INTEGER,
    media_type TEXT,
    season INTEGER,
    episode INTEGER,
    reason TEXT,
    added_at TEXT NOT NULL DEFAULT (datetime('now'))
);
