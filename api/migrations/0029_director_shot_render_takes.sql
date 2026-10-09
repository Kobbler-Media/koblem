-- Non-destructive TAKES for Director shot renders.
--
-- Until now a retake overwrote its shot's row (and its webm) in place: the PK was
-- (project_id, tier, shot_index), so the previous take was gone the moment a better-or-worse one
-- landed. That makes an unattended overnight run useless — the whole point of letting the vision
-- critic re-roll a shot is waking up to SEVERAL takes and picking the one you actually like.
--
-- A take is identified by the koblem job that produced it (`source_job_id`), which is what makes
-- harvesting idempotent: re-harvesting the same job (the restart-reattach path) updates that take
-- in place, while a retake — always a new job — allocates the next `variant` and leaves the older
-- takes alone. `variant` is the display ordinal (0 = the first take of that shot).
--
-- Exactly one take per (project, tier, shot) is `selected`: the one the editor plays and the one a
-- rebuilt cut concatenates. The partial unique index enforces that rather than trusting callers.
--
-- `seed` and `critic` are per-take provenance: the seed that produced it (so a take can be
-- reproduced/described) and the vision critic's verdict on it
-- (`{"score": 0-1, "verdict": "pass"|"fail", "note": "...", "beats": [...], "model": "...",
--   "at": "<rfc3339>"}`), which is what an auto-retake loop decides on.

ALTER TABLE director_shot_renders
    ADD COLUMN variant       INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN selected      BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN source_job_id TEXT    NOT NULL DEFAULT '',
    ADD COLUMN seed          BIGINT,
    ADD COLUMN critic        JSONB;

-- Existing rows are each their own shot's only take, so they are already variant 0 + selected.
ALTER TABLE director_shot_renders DROP CONSTRAINT director_shot_renders_pkey;
ALTER TABLE director_shot_renders
    ADD CONSTRAINT director_shot_renders_pkey PRIMARY KEY (project_id, tier, shot_index, variant);

-- One take per producing job: the idempotency key for harvesting.
CREATE UNIQUE INDEX director_shot_renders_source_job
    ON director_shot_renders (project_id, tier, shot_index, source_job_id);

-- At most one selected take per shot.
CREATE UNIQUE INDEX director_shot_renders_selected
    ON director_shot_renders (project_id, tier, shot_index)
    WHERE selected;
