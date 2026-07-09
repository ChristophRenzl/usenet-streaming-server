-- Jellyfin-style user accounts. The schema was multi-user from day one
-- (user_id on watch_history/watchlist/preferences, hardcoded to 1); this
-- adds credentials so additional users can actually log in. User 1 is the
-- server owner/admin: API-key requests act as this user, so existing
-- clients and their data keep working unchanged.
ALTER TABLE users ADD COLUMN password_hash TEXT;
ALTER TABLE users ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0;
UPDATE users SET is_admin = 1 WHERE id = 1;

-- Bearer tokens from POST /auth/login. One row per login/device.
CREATE TABLE user_tokens (
    token TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_used_at TEXT NOT NULL DEFAULT (datetime('now'))
);
