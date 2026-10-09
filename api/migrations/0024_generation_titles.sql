-- A tiny human-readable title per generation, written asynchronously by the LLM
-- from the request inputs (prompt/lyrics/knobs) after the row lands. Best-effort:
-- stays NULL when the LLM is unavailable, and the UI falls back to summarising
-- request_meta. Existing rows are not backfilled.
ALTER TABLE generations ADD COLUMN title TEXT;
