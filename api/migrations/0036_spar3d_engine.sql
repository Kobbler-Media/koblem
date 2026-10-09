-- Image-to-rig scheduler entry.
--
-- 🔴 THE OPERATOR MUST PIN THIS ONE.  The SPAR3D + RigAnything Compose service
-- is hard-pinned to a single card by its own deployment.  It is not a
-- relocatable per-request CUDA server: accepting a gate reservation on one card
-- while the supervisor actually loads on another would make the scheduler's
-- occupancy ledger unsafe.  So `candidate_gpus` must name exactly the one UUID
-- the Compose service is pinned to — set it alongside the deployment, and
-- update the client-side GPU identity check at the same time.  It ships blank
-- because the UUID is specific to the machine.
--
-- Measured physical peak is 2,155 MiB for the standard FP16 worker.  Reserve
-- 2,600 MiB: that absorbs CUDA-context/workspace variation without pretending
-- it can coexist with the 10,000 MiB standing LLM reservation on the 12 GB
-- card.  A render is one-shot and its gate acquisition stays held for the
-- entire worker lifetime, so it is not seeded warm and is never auto-evicted.

INSERT INTO gpu_engine_config
    (service, peak_mb, home_gpu, candidate_gpus, relocatable, evictable_by, rules_json)
VALUES
    ('spar3d', 2600, '',
        '[]'::jsonb,
        FALSE, '[]'::jsonb,
        'Pinned to the Compose SPAR3D worker GPU; change with the service deployment, never by spill.')
ON CONFLICT (service) DO NOTHING;

-- The 3060's standing LLM (10,000 MiB) and a SPAR3D job (2,600 MiB) cannot
-- fit together.  Allow this one-shot renderer to drain the LLM, as flux and
-- songgen already can.  It may similarly evict light services if one has
-- spilled onto the 3060; otherwise a full card would wait forever despite the
-- worker being physically pinned there.  The updates are append-only so an
-- operator's existing custom rule is preserved.
UPDATE gpu_engine_config
    SET evictable_by = evictable_by || '["spar3d"]'::jsonb
    WHERE service IN ('llm', 'vision', 'stt', 'tts', 'higgs', 'music', 'sa3', 'matting', 'ltx_2b')
      AND NOT (evictable_by @> '["spar3d"]'::jsonb);
