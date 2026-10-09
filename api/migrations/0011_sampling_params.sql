-- Extend llm_logs with the remaining llama.cpp sampling knobs so A/B runs
-- are fully reproducible from the log detail view. `sampling_preset` carries
-- the named preset that was in effect (if any) alongside the resolved
-- numeric values in the other columns.
ALTER TABLE llm_logs
  ADD COLUMN request_top_p DOUBLE PRECISION,
  ADD COLUMN request_top_k INTEGER,
  ADD COLUMN request_min_p DOUBLE PRECISION,
  ADD COLUMN request_presence_penalty DOUBLE PRECISION,
  ADD COLUMN request_repetition_penalty DOUBLE PRECISION,
  ADD COLUMN sampling_preset TEXT;
