-- Granular progress phase for download jobs, primarily for the par2
-- download-and-repair fallback. The coarse `status` column keeps its existing
-- five values (pending/downloading/complete/failed/cancelled); `phase` adds a
-- finer, human-readable step within a running job.
--
-- Values: 'downloading' (fetching articles), 'repairing' (running par2),
-- 'extracting' (unpacking a repaired store-mode RAR), plus the terminal
-- 'complete'/'failed'. NULL for legacy rows and plain (non-repair) jobs that
-- never set a phase.
ALTER TABLE downloads ADD COLUMN phase TEXT;
