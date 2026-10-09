-- Multi-GPU scheduler — seed the `minimax_h3` service (MiniMax-H3 joint audio+video).
--
-- A SEPARATE minimax-h3 container running the same sd.cpp fork as `ltx`, so it gets its
-- own budget line rather than sharing the LTX slot: sharing would clobber ltx-video's
-- drain/unload URL on the gate's HeavyService registry (`register_heavy` replaces by
-- kind), and the gate would evict the wrong container. Same reasoning 0030 gives for
-- splitting krea2 off flux.
--
-- relocatable = FALSE, and it needs a large card: the text encoder alone is bigger than
-- a 12 GB card.
--
-- peak 15000 MiB. MEASURED: the Qwen3-VL text encoder is 14,564 MiB resident, and the
-- DiT is ~11 GB. A job stages them SEQUENTIALLY and never co-resident (encode every
-- shot's prompt in one text-encoder residency → reclaim it → load the DiT once and render
-- every shot), so the true peak is roughly the larger of the two, not their sum. 15000 is
-- deliberately seeded at the top of that range rather than tuned down: a peak that is too
-- HIGH only serialises H3 against work it could in principle have shared the card with,
-- while a peak that is too LOW lets the scheduler co-schedule something that then OOMs
-- mid-render. Re-measure against real weights before lowering it.
--
-- Mutual exclusion with LTX falls out of the budget rather than a list, exactly as it does
-- for ltx/ltx_2b and flux/krea2: 15000 + 7500 (`ltx`) > 16311, so on a 16 GB card the two
-- video engines cannot co-reside and serialise. The same arithmetic excludes it against
-- wan_vace, flux, krea2, songgen and avatar — at a 15000 peak, H3 renders alone.
--
-- evictable_by '[]' — a renderer, like flux/avatar/ltx/krea2/songgen: never auto-evicted,
-- a competing requester blocks instead.

INSERT INTO gpu_engine_config
    (service, peak_mb, home_gpu, candidate_gpus, relocatable, evictable_by, rules_json)
VALUES
    ('minimax_h3', 15000, '',
        '[]'::jsonb, FALSE,
        '[]'::jsonb, '')
ON CONFLICT (service) DO NOTHING;

-- Let minimax_h3 evict the light engines it may need to displace on its card, exactly
-- as flux/avatar/ltx/krea2/songgen already can.
--
-- `ltx_2b` is in this list and it is NOT optional — the same trap 0030 documents for
-- krea2, only worse. 0022 seeds the 2B as WARM-BUT-EVICTABLE: koblem re-seeds a Standing
-- 10500 MiB placement after every 2B render. H3 wants 15000 MiB, so on a 16 GB card with
-- the 2B warm and H3 absent from its evictable_by, `placement.rs::try_place` finds no
-- qualifying victim and `acquire()` parks in its retry loop FOREVER — it blocks, it does
-- not 503. Every H3 render after any 2B render would hang until an operator intervened.
--
-- Append-only + idempotent: a row that already names 'minimax_h3' (or that the operator
-- has since edited) is left alone.
UPDATE gpu_engine_config
    SET evictable_by = evictable_by || '["minimax_h3"]'::jsonb
    WHERE service IN ('vision', 'stt', 'tts', 'higgs', 'music', 'sa3', 'matting',
                      'ltx_2b')
      AND NOT (evictable_by @> '["minimax_h3"]'::jsonb);
