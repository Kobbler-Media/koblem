-- Retire the `ltx` line from the operator-editable GPU config. LTX-2.3 is gone.
--
-- # What this is, and what it is not
--
-- ⚠️ It is HYGIENE, not a fix, and the distinction is worth stating because the reverse was assumed
-- while planning the removal: that a leftover `('ltx', 7500, …)` row with `evictable_by = '[]'`
-- would keep reserving 7500 MiB on the 5060 Ti, so a later heavy would queue against a ghost and
-- `acquire()` would park in its retry loop instead of admitting. It does not. `Placer` bills
-- `placements` — a live ledger — and a service joins it exactly two ways: something acquires a
-- guard for it, or `seed_resident` gives it a Standing reservation. `seed_resident` is called for
-- `Llm` (`gpu_scheduler.rs`, `gpu_status.rs`) and `Ltx2b` (`routes/ltx_2b.rs`) and for nothing
-- else, and the sole caller of `acquire_for_ltx()` was the `LtxBackend::Cpp` arm of
-- `LtxVideoClient::acquire`, which is deleted alongside this migration. `engines` — where this row
-- lands — is a descriptor map read WHEN a placement is made. Nothing makes one, so nothing is
-- billed. Dropping the row does not free VRAM, because the row was never holding any.
--
-- What it actually removes is a row on the settings page's engine table: an operator-editable
-- budget, home card and candidate list for a service that no request can reach, sitting in the
-- table where every other row is live. Editing it does nothing and says nothing about doing
-- nothing.
--
-- # Why `ServiceKind::Ltx` survives this
--
-- The enum variant stays. It is index 8 of `ServiceKind::ALL`, `service_enabled` indexes a
-- `[AtomicBool; 17]` by `kind as usize`, and the enum carries a NOTE saying to append rather than
-- insert — so removing it silently renumbers `Matting`, `WanVace`, `Ltx2b`, `Higgs`, `Sa3`,
-- `Krea2`, `MinimaxH3` and `Spar3d`, and every operator's disabled-service flags land on the wrong
-- engines. kobbler's `gpu_defaults.rs` also names it. An unused variant costs one discriminant;
-- reclaiming it costs a three-repo lockstep deploy.
--
-- Note also that LTX-2.5 does not want this row back under a new name: the `ltx25` plugin runs on
-- the `comfy-api` shim and acquires the **`minimax_h3`** budget line (`LtxBackend::Comfy` →
-- `acquire_for_minimax_h3`), because it shares one container and one resident weights budget with
-- its co-tenant. Giving 2.5 its own line would double-count that budget.

DELETE FROM gpu_engine_config WHERE service = 'ltx';

-- The eviction sets named it too. `evictable_by` is a set of service NAMES, edited on the settings
-- page and matched by name when a heavy looks for something to boot off a card. "ltx" can no longer
-- match anything, so leaving it there is an operator-visible claim that a light peer is evictable by
-- an engine that cannot run — the one kind of entry that costs an afternoon to disprove.
UPDATE gpu_engine_config
SET evictable_by = (
    SELECT COALESCE(jsonb_agg(name), '[]'::jsonb)
    FROM jsonb_array_elements_text(evictable_by) AS t(name)
    WHERE t.name <> 'ltx'
)
WHERE evictable_by @> '["ltx"]'::jsonb;
