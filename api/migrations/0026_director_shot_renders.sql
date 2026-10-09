-- Durable per-project Director renders, in two quality TIERS.
--
-- The rendered webms a Director run produces live only in the koblem LTX job
-- store (`/var/lib/koblem/ltx-jobs`), which sweeps finished jobs after a 1h TTL —
-- so reopening a project later showed every shot as "not rendered" with no clip
-- to play. This table (plus the owned, non-swept `director-renders/<project_id>/`
-- dir on disk) is where a run's output is copied out to survive that sweep.
--
-- `tier` splits two independent, incrementally-built render streams of the same
-- cut: `proxy` (small/few-steps, WITH audio + LoRAs — a cheap "is the prompt /
-- lipsync / music-reactivity landing?" pass) and `final` (full res + upscale).
-- The two never share latents (different resolution), so each is its own chain;
-- persisting both lets the editor rehydrate whichever tier on load and show a
-- per-tier status (proxy ahead, final behind).
--
-- One row per persisted clip per tier: the stitched final (`shot_index = -1`) and
-- each progressively-delivered shot (`shot_index >= 0`, the PROMPT-FILTERED segment
-- index the engine/UI use — the Nth shot WITH a prompt, empty-prompt clips
-- dropped). `prompt` is the shot text AS RENDERED, so a reopened editor can tell a
-- still-valid shot ("ok") from one edited since ("stale") without trusting the
-- saved timeline to match the last render.
CREATE TABLE director_shot_renders (
    project_id   TEXT NOT NULL REFERENCES director_projects (id) ON DELETE CASCADE,
    tier         TEXT NOT NULL DEFAULT 'final',  -- 'proxy' | 'final'
    shot_index   INTEGER NOT NULL,               -- -1 = stitched final, >=0 = prompt-filtered shot
    prompt       TEXT NOT NULL DEFAULT '',        -- shot prompt as rendered ('' for the stitched final)
    file_name    TEXT NOT NULL,                   -- basename within director-renders/<project_id>/
    content_type TEXT NOT NULL DEFAULT 'video/webm',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (project_id, tier, shot_index)
);
