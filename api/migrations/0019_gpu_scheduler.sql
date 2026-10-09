-- Multi-GPU scheduler config.
--
-- Two config tables the koblem layer loads into kob-gpu-gate's placement layer
-- at startup and on every edit, so the load-bearing peak/priority table is a
-- form field rather than a recompile:
--
--   gpu_cards          — one row per physical GPU (uuid, total VRAM, fast flag,
--                        operator enable flag). NOT SEEDED: a fresh install has
--                        no cards until the operator registers their own.
--   gpu_engine_config  — one row per gate-tracked service (peak_mb, home GPU,
--                        candidate GPUs, priority, relocatable). Seeded with
--                        starting peak/priority values, but with NO card
--                        assignment — `home_gpu` and `candidate_gpus` are left
--                        unset for the operator to fill in once their cards
--                        exist.
--
-- `candidate_gpus` is a JSONB array of UUID strings (home first, then spill).

CREATE TABLE gpu_cards (
    uuid TEXT PRIMARY KEY,          -- GPU UUID (switch by UUID, never index)
    label TEXT NOT NULL DEFAULT '', -- human name for the UI (e.g. "RTX 3060 12GB")
    total_mb INTEGER NOT NULL,      -- reported memory.total in MiB
    fast BOOLEAN NOT NULL DEFAULT FALSE,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    sort_order INTEGER NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE gpu_engine_config (
    service TEXT PRIMARY KEY,        -- ServiceKind::as_str(): llm/tts/avatar/...
    peak_mb INTEGER NOT NULL,        -- tested worst-case peak (+margin) in MiB
    home_gpu TEXT NOT NULL DEFAULT '',
    candidate_gpus JSONB NOT NULL DEFAULT '[]'::jsonb,
    priority SMALLINT NOT NULL DEFAULT 0,
    relocatable BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- No card seed. GPU UUIDs are specific to one physical machine, so there is
-- nothing sensible to put here — the operator registers their own cards.
--
-- Seed the per-service budget table. The peak_mb and priority values are real
-- starting points; the card assignment is deliberately blank.
INSERT INTO gpu_engine_config (service, peak_mb, home_gpu, candidate_gpus, priority, relocatable) VALUES
    ('flux',    7500,  '',
        '[]'::jsonb, 50, FALSE),
    ('songgen', 7500,  '',
        '[]'::jsonb, 50, FALSE),
    ('avatar',  7500,  '',
        '[]'::jsonb, 40, FALSE),
    ('ltx',     7500,  '',
        '[]'::jsonb, 40, FALSE),
    ('llm',     10000, '',
        '[]'::jsonb, 35, TRUE),
    ('tts',     4500,  '',
        '[]'::jsonb, 20, FALSE),
    ('music',   3800,  '',
        '[]'::jsonb, 15, FALSE),
    ('vision',  2000,  '',
        '[]'::jsonb, 10, FALSE),
    ('stt',     1500,  '',
        '[]'::jsonb, 8, FALSE);
