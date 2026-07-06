-- Denormalized media metadata on watch-history rows, captured best-effort at
-- session start so history consumers (continue watching) can show the real
-- movie/show title and artwork instead of the technical release name.
ALTER TABLE watch_history ADD COLUMN title TEXT;
ALTER TABLE watch_history ADD COLUMN poster_url TEXT;
ALTER TABLE watch_history ADD COLUMN backdrop_url TEXT;
ALTER TABLE watch_history ADD COLUMN episode_title TEXT;
ALTER TABLE watch_history ADD COLUMN still_url TEXT;
