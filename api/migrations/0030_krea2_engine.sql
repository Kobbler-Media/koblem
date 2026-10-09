-- Multi-GPU scheduler — seed the `krea2` service (Krea-2-Turbo text→image).
--
-- A SEPARATE krea2 container running the same sd.cpp fork as `flux2` but a
-- different checkpoint, so it gets its own budget line rather than sharing the
-- `flux` slot: the two would otherwise clobber each other's drain/unload URL on
-- the gate's registry, and their peaks can't co-reside anyway.
--
-- 🔴 BLACKWELL ONLY. The shipped DiT is NVFP4, and the cuBLASLt FP4 GEMM only
-- serves on Blackwell (cc 120) — on an older card it is a cc-gated no-op and the
-- DiT falls back to F32 MMVQ. So `candidate_gpus` must name Blackwell cards only,
-- and relocatable = FALSE (an older card would need a Q8_0 build, which is not
-- what ships).
--
-- peak 12500 MiB: the ~7.75 GiB nvfp4 DiT runs RESIDENT (KREA2_MAX_VRAM=11
-- graph-cut budget) plus the DiT compute buffer, which is linear in tokens —
-- 915 MiB at 1024², 3,607 MiB at 2048² — plus the VAE. Sized so it CANNOT
-- co-reside with `flux` on a 16 GB card (12500 + 7500 > 16311) → the two image engines
-- serialise by budget, which is the intent.
--
-- evictable_by '[]' — a renderer, like flux/avatar/ltx/songgen: never
-- auto-evicted, a competing requester blocks instead. The lights below gain
-- 'krea2' as an evictor, mirroring what 0021 did for the other renderers.

INSERT INTO gpu_engine_config
    (service, peak_mb, home_gpu, candidate_gpus, relocatable, evictable_by, rules_json)
VALUES
    ('krea2', 12500, '',
        '[]'::jsonb, FALSE,
        '[]'::jsonb, '')
ON CONFLICT (service) DO NOTHING;

-- Let krea2 evict the light engines it may need to displace on its card,
-- exactly as flux/avatar/ltx/songgen already can.
--
-- `ltx_2b` is in this list too, and it is NOT optional. 0022 seeds the 2B as
-- WARM-BUT-EVICTABLE: koblem calls seed_resident(Ltx2b) after every render, so a
-- Standing 10500 MiB placement sits on the card between renders. krea2 wants
-- 12500 MiB, so on a 16 GB card 10500 + 12500 > 16311 — with the 2B warm and
-- krea2 absent from its evictable_by, placement.rs::try_place finds no qualifying
-- victim and acquire() parks in its retry loop FOREVER (it blocks; it does not
-- 503). Every krea2 render after any 2B render would hang until an operator
-- intervened. 0022's own rule is "EVERY renderer that may want the 5060 Ti can
-- evict it" — krea2 is one, so it belongs in that set.
--
-- Append-only + idempotent: a row that already names 'krea2' (or that the
-- operator has since edited) is left alone.
UPDATE gpu_engine_config
    SET evictable_by = evictable_by || '["krea2"]'::jsonb
    WHERE service IN ('vision', 'stt', 'tts', 'higgs', 'music', 'sa3', 'matting',
                      'ltx_2b')
      AND NOT (evictable_by @> '["krea2"]'::jsonb);
