-- Opt-in ranking bonus for bigger releases (higher bitrate at equal quality
-- tier). Off by default: bigger is not better on constrained networks.
ALTER TABLE preferences ADD COLUMN prefer_larger_releases INTEGER NOT NULL DEFAULT 0;
