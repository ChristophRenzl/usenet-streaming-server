-- Usernames must be unique (case-insensitively): logins look accounts up by
-- name, so duplicates would make sign-in pick an arbitrary row. Existing
-- duplicates (possible until now) are renamed with an id suffix so the
-- index can be created; the earliest account keeps the plain name.
UPDATE users SET name = name || ' (' || id || ')'
WHERE id NOT IN (SELECT MIN(id) FROM users GROUP BY LOWER(name));

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_name ON users (name COLLATE NOCASE);
