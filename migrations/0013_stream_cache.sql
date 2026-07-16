-- Persistent stream cache: everything streamed from NNTP is also written to a
-- dedicated cache directory as an auto-created download job.
--
-- `origin` distinguishes user-requested downloads ('user') from
-- cache-originated ones ('cache'). `last_played_at` is touched whenever a
-- cache entry is played back from disk and drives LRU eviction (falling back
-- to created_at for entries never played from the cache yet).
ALTER TABLE downloads ADD COLUMN origin TEXT NOT NULL DEFAULT 'user';
ALTER TABLE downloads ADD COLUMN last_played_at TEXT;
