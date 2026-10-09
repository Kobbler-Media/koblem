-- Multi-GPU scheduler — split the shared TTS and Music budget lines into one
-- line PER ENGINE, so no two services are gated as a single slot.
--
--   - `tts`  (4500) stays = qwen3. New `higgs` (4500) is its own budget line.
--   - `music` (was 3800, shared) now = ACE-Step ALONE, bumped to 5500 (it got
--     heavier). New `sa3` (2800) is Stable-Audio-3's own, much smaller line
--     (all-Q8 weights ~2.67 GB + margin).
--
-- Effect: qwen3/higgs and acestep/sa3 now co-reside when their peaks fit, or
-- place on different cards, instead of serialising through one shared slot. The
-- gate keys these off `ServiceKind::{Tts,Higgs,Music,Sa3}` (as_str: tts/higgs/
-- music/sa3). The light-engine `evictable_by` set mirrors the rows they split
-- from (renderers may evict them; they never queue-block a renderer). All values
-- are runtime-editable from the GPU scheduler UI.

-- ACE-Step is now its own (heavier) line.
UPDATE gpu_engine_config SET peak_mb = 5500 WHERE service = 'music';

INSERT INTO gpu_engine_config
    (service, peak_mb, home_gpu, candidate_gpus, relocatable, evictable_by, rules_json)
VALUES
    ('higgs', 4500, '',
        '[]'::jsonb,
        FALSE, '["flux","avatar","ltx","songgen"]'::jsonb, ''),
    ('sa3', 2800, '',
        '[]'::jsonb,
        FALSE, '["flux","avatar","ltx","songgen"]'::jsonb, '')
ON CONFLICT (service) DO NOTHING;
