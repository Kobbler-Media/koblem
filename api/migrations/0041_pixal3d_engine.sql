-- Pixal3D (longcat-rig `avatar_server`) scheduler entry.
--
-- 🔴 WITHOUT THIS ROW THE FEATURE IS DEAD ON ARRIVAL, AND SILENTLY SO.
-- `GpuLock::acquire` short-circuits to a NO-OP guard when `has_engine(kind)` is false, so an
-- unconfigured `pixal3d` reserves nothing, evicts nothing and admits instantly — straight into the
-- engine's own `insufficient_vram` 503, with the scheduler free to drop a co-tenant onto the card
-- mid-rig. The failure reads as "the rig service is broken", not "koblem never told the scheduler
-- this engine exists".
--
-- 🔴 THE OPERATOR MUST PIN THIS ONE, exactly like spar3d (0036) and for the same reason: the
-- Compose service hard-pins a card, so a reservation on any other card would make the occupancy
-- ledger lie. It ships with a blank candidate set because the UUID is machine-specific — set it to
-- the one card the Compose service is pinned to. Blackwell is unvalidated for this pipeline and
-- both known Blackwell failures in it were SILENT (a DC remesh returning ~13,000 disconnected
-- components with no error), so do not widen the candidate set without re-running the eye test.
--
-- 🔴 THE RESERVATION IS THE SERVICE'S REFUSE-BELOW FLOOR, NOT ITS MEASURED PEAK. Measured peaks are
-- 6,055 MiB (four-tier ladder), 6,959 (re-rig from cached geometry) and 8,700 (a longer chain in
-- one process). But `LONGCAT_RIG_MIN_FREE_MIB` is 11,000 and the rig stage DIES RATHER THAN
-- DEGRADING: below that floor the request is refused with a 503 naming the number. Reserving the
-- peak would let the scheduler admit a co-tenant that pushes free VRAM under the floor, and the rig
-- would then refuse a card the scheduler believes it granted. So the number that has to be free is
-- the number reserved. On a 12 GB card that means nothing else fits during a rig, which is true.
--
-- Never auto-evicted (`evictable_by = []`): a rig is up to ~960 s of GPU with no partial artifact,
-- and its gate acquisition is held for the whole ladder.

INSERT INTO gpu_engine_config
    (service, peak_mb, home_gpu, candidate_gpus, relocatable, evictable_by, rules_json)
VALUES
    ('pixal3d', 11000, '',
        '[]'::jsonb,
        FALSE, '[]'::jsonb,
        'longcat-rig, pinned to the Compose worker GPU. 11000 is the service''s own refuse-below floor (LONGCAT_RIG_MIN_FREE_MIB), not its ~8.7 GB peak: the rig stage dies rather than degrading, so the floor is what must be FREE.')
ON CONFLICT (service) DO NOTHING;

-- 🔴 AND THE HALF THAT ACTUALLY MAKES IT RUN. A card carrying the 10,000 MiB standing LLM
-- reservation has no room for this too — 10,000 + 11,000 does not fit a 12 GB card — and `placement.rs::try_place` preempts ONLY
-- residents that name the requester in their own `evictable_by`. With `pixal3d` absent from that
-- set there is no qualifying victim, so `acquire()` parks in its retry loop **forever** — it
-- blocks, it does not 503, and the character sits at `queued` holding koblem's single Pixal3D slot
-- with no error and no timeout. Every later submit is then refused by a job nobody is watching.
--
-- Same grant spar3d (0036) and minimax_h3 (0035) already carry, and the same reasoning: a rig is a
-- renderer, and the LLM is the always-resident service designed to be drained for one.
--
-- Append-only + idempotent: a row that already names 'pixal3d', or that an operator has since
-- edited, is left alone.
UPDATE gpu_engine_config
    SET evictable_by = evictable_by || '["pixal3d"]'::jsonb
    WHERE service IN ('llm', 'vision', 'stt', 'tts', 'higgs', 'music', 'sa3', 'matting', 'ltx_2b')
      AND NOT (evictable_by @> '["pixal3d"]'::jsonb);
