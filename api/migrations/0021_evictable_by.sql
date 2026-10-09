-- Multi-GPU scheduler — NAMED, RELATIONAL eviction (HANDOFF §8a "Service" spec).
--
-- Replaces numeric-`priority` eviction with a per-service `evictable_by`: each
-- service declares, BY SERVICE NAME, who may evict it. "flux can evict the LLM" =
-- llm.evictable_by contains 'flux'. When requester R needs room on a card and none
-- is free, the candidate victims are the residents V there with R ∈ V.evictable_by
-- (relocate the ones that fit elsewhere, else drain/SIGKILL); if no qualifying
-- victim set frees enough, R blocks. There is NO priority-number comparison in the
-- eviction decision any more.
--
-- `evictable_by` is a JSONB array of ServiceKind name strings (llm/flux/songgen/…).
-- An empty array means the service is never auto-evicted (this subsumes `pinned`).
--
-- The numeric `priority` column from 0019 stays for now (so old rows / tooling
-- don't break) but MUST NOT drive eviction — the gate load + UI eviction logic key
-- off `evictable_by` only. `relocatable` (evict = move-not-kill, the LLM) and
-- `rules_json` (reserved for future CONDITIONAL evictability) are unchanged.

ALTER TABLE gpu_engine_config
    ADD COLUMN IF NOT EXISTS evictable_by JSONB NOT NULL DEFAULT '[]'::jsonb;

-- Backfill the sane-default seed (HANDOFF §8a + owner brief):
--   - llm: evictable by the two renderers that may boot it off the 3060.
--   - lights (vision/stt/tts/music/matting): evictable by ANY renderer.
--   - renderers (flux/avatar/ltx/songgen): evictable by nobody → they queue/block.
UPDATE gpu_engine_config SET evictable_by = '["flux","songgen"]'::jsonb
    WHERE service = 'llm';

UPDATE gpu_engine_config
    SET evictable_by = '["flux","avatar","ltx","songgen"]'::jsonb
    WHERE service IN ('vision','stt','tts','music','matting');

UPDATE gpu_engine_config SET evictable_by = '[]'::jsonb
    WHERE service IN ('flux','avatar','ltx','songgen');
