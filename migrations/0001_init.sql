CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO users (id, name) VALUES (1, 'default');

CREATE TABLE nntp_providers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    host TEXT NOT NULL,
    port INTEGER NOT NULL DEFAULT 563,
    use_tls INTEGER NOT NULL DEFAULT 1,
    username TEXT,
    password TEXT,
    max_connections INTEGER NOT NULL DEFAULT 10,
    priority INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE indexers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    base_url TEXT NOT NULL,
    api_key TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    priority INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE preferences (
    user_id INTEGER PRIMARY KEY REFERENCES users(id),
    preferred_resolution TEXT NOT NULL DEFAULT '1080p',
    max_resolution TEXT NOT NULL DEFAULT '2160p',
    preferred_video_codecs TEXT NOT NULL DEFAULT '["h264","hevc"]',
    preferred_audio_codecs TEXT NOT NULL DEFAULT '["aac","ac3","eac3"]',
    max_size_bytes INTEGER,
    language TEXT NOT NULL DEFAULT 'en',
    allowed_terms TEXT NOT NULL DEFAULT '[]',
    blocked_terms TEXT NOT NULL DEFAULT '["CAM","TS","TELESYNC","HDCAM"]',
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO preferences (user_id) VALUES (1);

CREATE TABLE watch_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    tmdb_id INTEGER NOT NULL,
    media_type TEXT NOT NULL CHECK (media_type IN ('movie', 'tv')),
    season INTEGER,
    episode INTEGER,
    release_title TEXT,
    indexer_id INTEGER,
    nzb_url TEXT,
    position_secs REAL NOT NULL DEFAULT 0,
    duration_secs REAL,
    watched_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (user_id, tmdb_id, media_type, season, episode)
);

CREATE TABLE downloads (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    tmdb_id INTEGER NOT NULL,
    media_type TEXT NOT NULL CHECK (media_type IN ('movie', 'tv')),
    season INTEGER,
    episode INTEGER,
    release_title TEXT NOT NULL,
    nzb_url TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'downloading', 'complete', 'failed', 'cancelled')),
    progress_bytes INTEGER NOT NULL DEFAULT 0,
    total_bytes INTEGER,
    file_path TEXT,
    error TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
