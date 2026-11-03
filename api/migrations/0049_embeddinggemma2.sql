-- EmbeddingGemma-2 replaces SigLIP2.
--
-- Two llama-servers now serve embeddings, one model in one vector space:
--   `vision`     — `embed-vision`, the GPU instance for images (and text when
--                  `embed-text` is down). Same gate line as before.
--   `text_embed` — `embed-text`, the always-on text instance every local service
--                  embeds queries and documents with. NEW line.

-- `vision` 2000 -> 1300. Measured for EG2 Q8_0 + mmproj Q8_0 at 70 image tokens:
-- 1034 MiB loaded, 1090 peak on the 3060, 1290 peak on the 5060 Ti. SigLIP2 held
-- 1666. Only moves the seeded value: a peak an operator tuned by hand is left alone.
UPDATE gpu_engine_config
SET peak_mb = 1300, updated_at = now()
WHERE service = 'vision' AND peak_mb = 2000;

-- `text_embed`. Its requests never acquire: koblem seeds a STANDING reservation
-- (like the LLM's) and the reconcile loop records it wherever the worker announces
-- itself on `/v1/gpu/status`. It belongs on the card that keeps room for it beside
-- Beefy; set `home_gpu`/`candidate_gpus` to that card (UUIDs are machine-specific,
-- so they ship blank).
--
-- 600: measured on the 5060 Ti for the text-only Q8_0 at `--parallel 1 -c 1024
-- -b 1024 -ub 1024`: +411 MiB loaded, +513 MiB peak. Beside Beefy that card holds
-- 15,370 / 16,311 MiB after a cold 195k-token request (the 27B ~15,240, an idle
-- ComfyUI 128; kobbler's NVDEC is pinned to the 3060) and exl3 never gives it back, so ~430 MiB stays free with the embedder
-- loaded. `-ub 2048` OOMs there on its first full request; keep TEXT_EMBED_MAX_TOKENS
-- at the deployed `-ub` (1024). Beefy must reserve ~15,250 on this card
-- (`BEEFY_RESERVE_MB`) rather than all of it, or the embedder is unloaded on entry.
--
-- Evictable only by the heavy renderers, and the gate takes it last among the
-- victims it may evict (`placement.rs`), so it goes only when nothing else frees
-- enough. Not relocatable: koblem sends it no `X-Worker-Gpu`.
INSERT INTO gpu_engine_config
    (service, peak_mb, home_gpu, candidate_gpus, relocatable, evictable_by, rules_json)
VALUES
    ('text_embed', 600, '',
        '[]'::jsonb,
        FALSE,
        '["flux", "avatar", "songgen", "krea2", "minimax_h3", "ltx_2b", "wan_vace", "pixal3d"]'::jsonb,
        'embed-text, the always-on EmbeddingGemma-2 text instance. Standing reservation; evicted only as a last resort. 600 = measured 513 MiB peak at -ub 1024 on the 5060 Ti, beside Beefy.')
ON CONFLICT (service) DO NOTHING;

-- Point a deployment still on the documented SigLIP2 URL at the new image server.
-- Any other value is an operator's choice and is left alone.
UPDATE settings
SET value = '"http://embed-vision:8080"'::jsonb, updated_at = now()
WHERE key = 'vision_service_url' AND value = '"http://siglip2:8890"'::jsonb;
