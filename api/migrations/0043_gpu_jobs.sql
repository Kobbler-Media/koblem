-- The global GPU job queue.
--
-- One row per unit of GPU work, replacing five disk-sidecar job stores that were each one slot for
-- one engine and refused rather than queued. Postgres rather than sidecars because this needs
-- ordering, atomic reorder and multiple writers (HTTP handlers, the dispatcher, the reconcile loop)
-- and JSON sidecars get all three wrong.
--
-- 🔴 `payload` MUST survive a deploy. A job queued at 23:00 is deserialized by whatever binary is
-- running at 03:00, so every field the adapters read out of it needs `#[serde(default)]` — the same
-- discipline the sidecars already enforce, but now it is load-bearing overnight rather than for the
-- ten minutes a sidecar used to live.

CREATE TABLE gpu_jobs (
    id              TEXT PRIMARY KEY,

    -- The adapter that runs this row. Not a `ServiceKind`: `ltx25` and `h3` are different kinds
    -- that share one lane, and `pixal3d_rig`/`pixal3d_motion` are different kinds on one service.
    kind            TEXT NOT NULL,

    -- The engine request exactly as the route builds it today.
    payload         JSONB NOT NULL,

    -- queued | running | collecting | done | failed | cancelled | held
    --
    -- `collecting` is not decoration: every runner drops the GPU guard the moment engine work ends
    -- and BEFORE the disk/DB work, because on an upscale that tail is minutes and the card must not
    -- idle through it. A job in `collecting` has already paid for its GPU and is therefore NOT
    -- cancellable — killing it there loses the only copy of the artifact.
    state           TEXT NOT NULL,

    -- 0 interactive, 1 normal, 2 background. Ordering is band, then rank within band.
    band            SMALLINT NOT NULL DEFAULT 1,

    -- Float-midpoint rank. A drag-reorder is ONE row update: rank = (prev + next) / 2. Named
    -- `queue_rank` because `rank` is a window function and reads ambiguously in every query.
    queue_rank      DOUBLE PRECISION NOT NULL,

    -- Operator pin: "run this on the 3060". NULL = the placer's normal ranked candidates.
    target_gpu      TEXT,
    -- With a pin, ignore the residents' `evictable_by` on that card (the "stop the LLM" button).
    -- It never overrides the never-SIGKILL-in-flight rule.
    force_pin       BOOLEAN NOT NULL DEFAULT FALSE,

    -- The engine's own id. This is the single field that makes restart-reattach possible.
    server_job_id   TEXT,

    -- Whatever the kind reports: phase / pct / eta / preview_seq / unit counters.
    progress        JSONB NOT NULL DEFAULT '{}'::jsonb,

    -- Checkpoint state after a preempt (bank keys, segments_done). NULL = start from the top.
    resume          JSONB,

    -- What the run produced, in the shape that kind's own API already serves: track lists, clip
    -- paths, segment files, tier ladders. Deliberately opaque to the dispatcher.
    --
    -- This is what lets the five per-engine job stores retire. They were never only schedulers —
    -- each was also the record its own `GET .../jobs/{id}` reads back — and without somewhere
    -- generic to put that, retiring them would mean rewriting every media route at the same time.
    result          JSONB,

    -- A dependent stays `queued` and is simply not admissible until its parent is `done`; a parent
    -- that fails or is cancelled cascades the dependent to `cancelled` with the reason. No
    -- reservation, no priority inheritance. ON DELETE SET NULL so pruning an ancient parent row
    -- cannot delete a live child.
    depends_on      TEXT REFERENCES gpu_jobs(id) ON DELETE SET NULL,

    -- The principal that enqueued it, so the queue page is not anonymous.
    created_by      TEXT NOT NULL,

    -- 🔴 The 409 was accidentally protecting the user: a double-clicked Render used to be REFUSED.
    -- An enqueue succeeds twice, and you wake up to two identical 40-minute renders.
    idempotency_key TEXT,

    -- Why this row did not start on the last tick, and since when. Never a failure — a job the
    -- gate refuses is stamped, skipped and reconsidered. `blocked_since` is kept across ticks that
    -- report the SAME reason so "waiting 4h" is readable.
    -- JSONB rather than TEXT: it is a tagged enum, and the row, the SSE frame and the CLI must all
    -- read the same value instead of re-parsing a sentence three different ways.
    blocked_reason  JSONB,
    blocked_since   TIMESTAMPTZ,
    -- When the blocked-too-long alert last fired for this row. Once per episode, not once per
    -- sweep: an alert that repeats every two minutes for the rest of the night trains the operator
    -- to ignore it, which is the opposite of the point.
    alerted_at      TIMESTAMPTZ,

    error           TEXT,
    -- 🔴 A render that "succeeded" having silently dropped a generation looks exactly like "I
    -- forgot to press render". Nobody reads docker logs at 3am and they rotate within hours.
    warnings        JSONB NOT NULL DEFAULT '[]'::jsonb,

    -- Three numbers, deliberately NOT one. `gate_wait_ms` keeps its current meaning for the direct
    -- interactive services; a queued kind reports these instead:
    --   queued_ms  enqueue -> admitted. Includes waits that are the user's OWN choice (behind
    --              their own four jobs), so it is only readable next to the dominant blocked_reason.
    --   admit_ms   the EVICTION COST. try_acquire is not instant when it evicts: 30 s drain budget
    --              non-LLM, 900 s LLM, and a force unload is bounded at 300 s because an H3 unload
    --              waits out a diffusion step. This is the eviction-thrash detector and folding it
    --              into queue wait hides it completely.
    --   render_ms  engine time, unchanged.
    queued_ms       BIGINT,
    admit_ms        BIGINT,
    render_ms       BIGINT,

    -- Segments / generations / steps — whatever this kind's unit is. The estimator works per UNIT,
    -- which is what makes "remaining" work for a shape never rendered at that length.
    units_total     INTEGER,
    units_done      INTEGER NOT NULL DEFAULT 0,
    -- The estimator bucket this row was measured into, stamped at enqueue by the adapter.
    shape_key       TEXT,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ
);

-- The dispatcher's scan: band then rank, pending rows only. Partial, because the pending set stays
-- small while the terminal set grows for a fortnight.
CREATE INDEX gpu_jobs_pending ON gpu_jobs (band, queue_rank)
    WHERE state IN ('queued', 'held');

-- Boot reattach + the lane census both want the in-flight set.
CREATE INDEX gpu_jobs_live ON gpu_jobs (kind)
    WHERE state IN ('running', 'collecting');

-- The queue page and the retention sweep read newest-first.
CREATE INDEX gpu_jobs_created ON gpu_jobs (created_at DESC);

CREATE INDEX gpu_jobs_depends_on ON gpu_jobs (depends_on) WHERE depends_on IS NOT NULL;

-- Dedupe is a UNIQUE index, not a SELECT-then-INSERT: two clicks land in two connections and the
-- check-then-act loses that race exactly when it matters.
--
-- Scoped to the NON-TERMINAL states on purpose. The same key may not be pending or running twice —
-- that is the double-click — but once a render has finished, asking for it again is a legitimate
-- second take, and an index over all states would refuse it forever.
CREATE UNIQUE INDEX gpu_jobs_idempotency ON gpu_jobs (idempotency_key)
    WHERE idempotency_key IS NOT NULL
      AND state NOT IN ('done', 'failed', 'cancelled');

-- Measured durations, per (kind, shape). Tiny, permanent, and read in one query at estimate time.
--
-- 🔴 Deliberately NOT a query over `gpu_logs`. That table is pruned by default
-- (`gpu_log_retention_max_entries` / `max_age_days`), so estimating from it means the estimates
-- silently get worse the moment an operator tightens retention. `gpu_logs` stays what it is:
-- forensics and replay.
--
-- 🔴 And measured history BEATS the engine's published ETA. H3's is ONE measured point extrapolated
-- linearly in frames where attention is quadratic, with reference rows charged at a documented
-- lower bound and a flat 100 s cold load per generation; its own comment asks to be replaced with a
-- measured table. LTX-2.5 publishes `eta_seconds: null` and always has. The engine number is a
-- cold-start prior only, used while `samples` is small and dropped as soon as there is history.
CREATE TABLE gpu_job_stats (
    kind        TEXT NOT NULL,
    shape_key   TEXT NOT NULL,
    -- Lifetime count. Reported alongside the estimate, because a confident-looking ETA built from
    -- three samples is worse than one that admits it is guessing.
    samples     INTEGER NOT NULL DEFAULT 0,
    p50_ms      BIGINT NOT NULL DEFAULT 0,
    p90_ms      BIGINT NOT NULL DEFAULT 0,
    -- Seconds per segment / per generation / per step. This is the field that makes a length never
    -- rendered before estimable at all.
    unit_p50_ms BIGINT NOT NULL DEFAULT 0,
    -- A bounded window of raw whole-job durations, so p50/p90 can be recomputed honestly instead of
    -- decayed. Capped in code; 50 samples is a few hundred bytes per shape.
    recent_ms   BIGINT[] NOT NULL DEFAULT '{}',
    -- The same window for per-unit durations. Kept apart because a job's unit count varies.
    recent_unit_ms BIGINT[] NOT NULL DEFAULT '{}',
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (kind, shape_key)
);
