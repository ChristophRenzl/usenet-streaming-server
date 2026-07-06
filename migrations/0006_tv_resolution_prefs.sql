-- Separate quality preferences for series: when set, TV episodes are ranked
-- against these instead of the movie/global preferred/max resolution. NULL
-- means "same as movies", which keeps existing installs behaving unchanged.
ALTER TABLE preferences ADD COLUMN preferred_resolution_tv TEXT;
ALTER TABLE preferences ADD COLUMN max_resolution_tv TEXT;
