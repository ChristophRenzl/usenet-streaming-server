-- Opt-out for Dolby Vision releases: DV profile 5 has no HDR10/SDR-compatible
-- base layer and various player/display combos mishandle it. Default allowed.
ALTER TABLE preferences ADD COLUMN allow_dolby_vision INTEGER NOT NULL DEFAULT 1;
