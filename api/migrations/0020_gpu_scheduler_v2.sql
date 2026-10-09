-- Multi-GPU scheduler v2 — pure budget scheduler.
--
-- Evolves the 0019 config to the v2 ServiceDesc shape:
--   - `pinned`     — never auto-evicted to make room for a higher-priority
--                    requester (the requester blocks instead). Card-disable still
--                    drains a pinned service.
--   - `rules_json` — reserved for future conditional eviction rules (stored +
--                    shown in the UI; minimally interpreted now).
--
-- Also adds the `matting` service (previously ungated — RMBG2, ~4 GB peak) to the
-- budget model. Like every other row, it ships with no card assignment.

ALTER TABLE gpu_engine_config
    ADD COLUMN IF NOT EXISTS pinned BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS rules_json TEXT NOT NULL DEFAULT '';

-- The LLM is intentionally NOT pinned: the design requires flux/songgen to be able
-- to BOOT it off a card when that card is full, and a card-disable to MOVE it
-- elsewhere. Both rely on the LLM being an evictable victim. Its
-- `relocatable=TRUE` (from 0019) makes that eviction a relocation (drain+respawn on
-- the target card) rather than a kill, and its priority (35) sits below flux/songgen
-- (50) so only they preempt it. Pin it from the UI if you want it protected.

-- Bring matting into the budget model.
INSERT INTO gpu_engine_config (service, peak_mb, home_gpu, candidate_gpus, priority, relocatable, pinned, rules_json)
VALUES
    ('matting', 4000, '',
        '[]'::jsonb, 12, FALSE, FALSE, '')
ON CONFLICT (service) DO NOTHING;
