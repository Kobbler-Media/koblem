-- Pixal3D stops being hard-pinned and joins the placement fleet.
--
-- 🔴 WHAT CHANGED UPSTREAM, AND WHY THIS ROW COULD NOT MOVE BEFORE IT. 0041 pinned `pixal3d` to a
-- single card because the Compose service hard-pinned it: `CUDA_VISIBLE_DEVICES` named one UUID and
-- `avatar_server` had no way to run anywhere else, so a reservation on any other card would have
-- been a ledger entry for a card the render could never touch. The engine now implements the
-- fleet's device-at-spawn contract (`LONGCAT_RIG_GPUS` + `WORKER_DEFAULT_GPU` + a per-request
-- `gpu`), the supervisor setenv's the chosen UUID immediately before forking the CUDA-owning child,
-- and `/health` publishes the accepted set as `gpus` — which koblem checks its allocation against
-- before submitting. The pin is gone, so the reservation may follow the work.
--
-- 🔴 THE RIG NEEDS A BIG CARD AND THAT IS THE ACTUAL BUG BEING FIXED. The rig stage needs 11,000
-- MiB FREE and dies rather than degrading; a resident LLM holds ~9,000 of a 12 GB card's 12,288.
-- 9,000 + 11,000 does not fit, so every rig depended on evicting the LLM first, and any co-tenant
-- that arrived without taking the flock (llama-server does exactly this) turned a submit into
-- `insufficient_vram` — "only 2913 MiB free on the pinned card, need >= 11000". Give this service a
-- card with real headroom for the stage, and list smaller cards only as spill.
--
-- ⚠️ sm_120. The rig binaries carry `86;120` and the pipeline has been run on Blackwell in testing.
-- Both historic Blackwell failures were SILENT rather than crashes (a DC remesh returning ~13,000
-- disconnected components with no error raised; garbled TTS audio), so a relocated rig is judged by
-- looking at the mesh and `pose_gate_worst` — never by a 200.
--
-- `peak_mb` is unchanged and still the service's refuse-below floor, not its ~8.7 GB measured peak
-- — see 0041 for why that distinction is load-bearing.
--
-- `relocatable` stays FALSE. It governs whether the scheduler may move a RESIDENT pixal3d worker
-- out from under itself to make room for someone else, and a rig is up to ~960 s of GPU with no
-- partial artifact. Choosing a card at admission and moving a running one are different powers;
-- this migration grants only the first.
--
-- 🔴 THIS MIGRATION NO LONGER TOUCHES `home_gpu` OR `candidate_gpus`. 0041 ships those blank (GPU
-- UUIDs are machine-specific, so there is nothing to seed), which means there is no pin here to
-- undo. All that is left to record is the placement RULE, in `rules_json`. Guarded on 0041's exact
-- text so an operator who has already assigned cards or edited the rule keeps their edit.
UPDATE gpu_engine_config
    SET rules_json = 'longcat-rig, placed per request. Prefer a card with headroom for the whole stage and list smaller cards only as spill; the engine relocates its CUDA worker to the reserved card before the rig starts. 11000 is the service''s own refuse-below floor (LONGCAT_RIG_MIN_FREE_MIB), not its ~8.7 GB peak: the rig stage dies rather than degrading, so the floor is what must be FREE.',
        updated_at = NOW()
    WHERE service = 'pixal3d'
      AND rules_json = 'longcat-rig, pinned to the Compose worker GPU. 11000 is the service''s own refuse-below floor (LONGCAT_RIG_MIN_FREE_MIB), not its ~8.7 GB peak: the rig stage dies rather than degrading, so the floor is what must be FREE.';

-- 🔴 THE LLM GRANT FROM 0041 STAYS AND IS STILL LOAD-BEARING. Letting placement choose does not
-- retire it: a rig that lands on a card holding the resident LLM still has to be able to drain it
-- or it parks in `try_place`'s retry loop forever (it blocks, it does not 503). Nothing to do here
-- — 0041's UPDATE is append-only and idempotent — but the reason is written down so a later
-- cleanup does not read it as dead.
