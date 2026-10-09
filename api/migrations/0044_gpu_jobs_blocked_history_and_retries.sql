-- Two things 0043 left out: why a job waited, and what happens after it fails.

-- 🔴 `blocked_reason` alone answers "why is it waiting NOW". It is overwritten in place on every
-- change and NULLed when the job starts, so by morning every row says it waited four hours and none
-- says what for — which is the one question `queued_ms` cannot answer on its own, because that
-- number includes waits that are the user's own choice.
--
-- One entry per EPISODE, not per tick: reason, when the wait started, how long it lasted. Bounded,
-- oldest dropped first, because a job parked all night on a flapping lane would otherwise grow an
-- array without limit.
ALTER TABLE gpu_jobs ADD COLUMN blocked_history JSONB NOT NULL DEFAULT '[]'::jsonb;

-- Which attempt this row is on. An automatic retry reuses the row, so without this a job that has
-- already burned two attempts is indistinguishable from a fresh one.
ALTER TABLE gpu_jobs ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;

-- The terminal row a manual retry was requeued from. A fresh row rather than resurrecting the old
-- one: the finished row is the record of what happened, `finished_at` and `render_ms` describe that
-- run, and overwriting them would destroy the history this migration exists to keep.
ALTER TABLE gpu_jobs ADD COLUMN retry_of TEXT;

-- Close the open waiting episode, if there is one. Called from every place a wait ends: the reason
-- changes, the job starts, the job is cancelled, the job finishes.
CREATE FUNCTION gpu_jobs_close_episode(hist JSONB, reason JSONB, since TIMESTAMPTZ)
RETURNS JSONB
LANGUAGE SQL STABLE AS $$
    SELECT CASE
        WHEN reason IS NULL OR since IS NULL THEN hist
        ELSE (CASE WHEN jsonb_array_length(hist) >= 20 THEN hist - 0 ELSE hist END)
             || jsonb_build_array(jsonb_build_object(
                    'reason', reason,
                    'since', to_jsonb(since),
                    'ms', (EXTRACT(EPOCH FROM (NOW() - since)) * 1000)::bigint))
    END
$$;
