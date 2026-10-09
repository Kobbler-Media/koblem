-- Seed `gpu_job_stats` from whatever `gpu_logs` still holds. Run ONCE after deploying the queue.
--
-- 🔴 NOT a migration, and deliberately not in `api/migrations/`: `sqlx::migrate!` scans that
-- directory and would try to version this. It must never run automatically anyway — it reads a table
-- whose retention is on by default, so what it can recover shrinks every day, and re-running it is
-- only ever a no-op or an improvement, never a repair.
--
--   docker compose exec -T koblem-db psql -U koblem -d koblem < scripts/seed-gpu-job-stats.sql
--
-- Idempotent: `ON CONFLICT DO UPDATE` only where the log has MORE samples than the table already
-- has, so it can never overwrite real measurements the queue has since accumulated with a smaller
-- historical guess.
--
-- 🔴 What this is worth, from one reference deployment: `gpu_logs` held 60 rows across six days. The
-- retention defaults (`gpu_log_retention_max_entries` 1000, `max_age_days` 7) had already eaten
-- everything else. Only `minimax-h3` (41 usable) and `ltx25` (1) survive with the metadata needed to
-- bucket them, and `flux` records no canvas at all so its rows cannot be keyed. That is the whole
-- argument for `gpu_job_stats` existing as a separate permanent rollup rather than the estimator
-- querying `gpu_logs` live — but it also means the queue starts nearly cold whatever this recovers,
-- and the real seeding is the first week of running it.

BEGIN;

-- h3 — key `h3/{W}x{H}/{ref|noref}`, matching `queue::adapters::h3::shape_key`.
-- Units are generations, which is why the per-unit number is the one that matters: a 16-generation
-- cut and a 1-generation retake share a bucket and share nothing else.
WITH j AS (
    SELECT duration_ms,
           (request_meta ->> 'width') || 'x' || (request_meta ->> 'height') AS canvas,
           jsonb_array_length(request_meta -> 'generations')                AS units,
           EXISTS (
               SELECT 1 FROM jsonb_array_elements(request_meta -> 'generations') g
               WHERE jsonb_array_length(COALESCE(g -> 'references', '[]'::jsonb)) > 0
           ) AS has_ref
    FROM gpu_logs
    WHERE service = 'minimax-h3' AND status = 200 AND duration_ms > 0
      AND request_meta ? 'generations' AND request_meta ? 'width'
), agg AS (
    SELECT 'h3'::text AS kind,
           'h3/' || canvas || '/' || CASE WHEN has_ref THEN 'ref' ELSE 'noref' END AS shape_key,
           count(*)::int AS samples,
           round(percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms))::bigint AS p50_ms,
           round(percentile_cont(0.9) WITHIN GROUP (ORDER BY duration_ms))::bigint AS p90_ms,
           round(percentile_cont(0.5) WITHIN GROUP (
               ORDER BY duration_ms::numeric / NULLIF(units, 0)))::bigint AS unit_p50_ms
    FROM j GROUP BY canvas, has_ref
)
INSERT INTO gpu_job_stats (kind, shape_key, samples, p50_ms, p90_ms, unit_p50_ms, updated_at)
SELECT kind, shape_key, samples, p50_ms, p90_ms, unit_p50_ms, NOW() FROM agg
ON CONFLICT (kind, shape_key) DO UPDATE
    SET samples     = EXCLUDED.samples,
        p50_ms      = EXCLUDED.p50_ms,
        p90_ms      = EXCLUDED.p90_ms,
        unit_p50_ms = EXCLUDED.unit_p50_ms,
        updated_at  = NOW()
    WHERE gpu_job_stats.samples < EXCLUDED.samples;

-- ltx25 — key `ltx25/{W}x{H}/{frames}f` off segment 0, matching `queue::adapters::ltx25::shape_key`.
-- Segment COUNT is deliberately absent from the key: it is `units_total`, and folding it in would
-- give every cut length its own sample of one.
WITH j AS (
    SELECT duration_ms,
           (request_meta ->> 'width') || 'x' || (request_meta ->> 'height')  AS canvas,
           (request_meta -> 'segments' -> 0 ->> 'frames')                    AS frames,
           jsonb_array_length(request_meta -> 'segments')                    AS units
    FROM gpu_logs
    WHERE service = 'ltx25' AND status = 200 AND duration_ms > 0
      AND request_meta ? 'segments' AND request_meta ? 'width'
      AND (request_meta -> 'segments' -> 0 ->> 'frames') IS NOT NULL
), agg AS (
    SELECT 'ltx25'::text AS kind,
           'ltx25/' || canvas || '/' || frames || 'f' AS shape_key,
           count(*)::int AS samples,
           round(percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms))::bigint AS p50_ms,
           round(percentile_cont(0.9) WITHIN GROUP (ORDER BY duration_ms))::bigint AS p90_ms,
           round(percentile_cont(0.5) WITHIN GROUP (
               ORDER BY duration_ms::numeric / NULLIF(units, 0)))::bigint AS unit_p50_ms
    FROM j GROUP BY canvas, frames
)
INSERT INTO gpu_job_stats (kind, shape_key, samples, p50_ms, p90_ms, unit_p50_ms, updated_at)
SELECT kind, shape_key, samples, p50_ms, p90_ms, unit_p50_ms, NOW() FROM agg
ON CONFLICT (kind, shape_key) DO UPDATE
    SET samples     = EXCLUDED.samples,
        p50_ms      = EXCLUDED.p50_ms,
        p90_ms      = EXCLUDED.p90_ms,
        unit_p50_ms = EXCLUDED.unit_p50_ms,
        updated_at  = NOW()
    WHERE gpu_job_stats.samples < EXCLUDED.samples;

COMMIT;

SELECT kind, shape_key, samples, p50_ms, p90_ms, unit_p50_ms FROM gpu_job_stats ORDER BY kind, shape_key;
