-- Multi-GPU scheduler — seed the `ltx_2b` service (LTX-Video 0.9.8 2B — the fast,
-- video-only "fun toy"). A SEPARATE ltx-2b container running the identical
-- ltx-video-server binary as `ltx`, so it gets its own budget line on the gate.
--
-- Peak 10500 MiB (the 2B peaks ~9.7 GB) — sized so it CANNOT co-reside with the
-- LTX-2.3 `ltx` DiT on a 16 GB card (10500 + 7500 > 16311) → the two LTX services
-- serialise by budget. relocatable = FALSE: give it a single-card candidate set.
--
-- WARM-BUT-EVICTABLE ("last thing stays warm; something else kicks the idle toy"):
-- unlike the other renderers (evictable_by = []), the 2B is the lowest-priority fun toy
-- and is kept warm-resident between renders (koblem calls gate.seed_resident(Ltx2b) after
-- each render → a Standing placement that survives the guard drop but still counts against
-- budget). So EVERY renderer that may want the same card can evict it: ltx / wan_vace /
-- avatar + flux / songgen. Eviction posts /v1/admin/unload →
-- kills the warm worker → frees the card; the 2B's next render re-forks cold + re-seeds.

INSERT INTO gpu_engine_config
    (service, peak_mb, home_gpu, candidate_gpus, relocatable, evictable_by, rules_json)
VALUES
    ('ltx_2b', 10500, '',
        '[]'::jsonb, FALSE,
        '["ltx","wan_vace","avatar","flux","songgen"]'::jsonb, '')
ON CONFLICT (service) DO NOTHING;
