-- Downloaded OpenSubtitles files, keyed by their OpenSubtitles file id.
-- Every session re-attaches subtitles (plays, replays, autoplay pre-created
-- sessions, retries), and the OpenSubtitles download quota is 20/day on free
-- accounts — caching makes repeat attaches free.
CREATE TABLE IF NOT EXISTS subtitle_cache (
    file_id INTEGER PRIMARY KEY,
    srt TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_used_at TEXT NOT NULL DEFAULT (datetime('now'))
);
