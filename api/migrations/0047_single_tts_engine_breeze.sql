-- Collapse TTS onto one engine: BreezeTTS 2 (`tts-breeze`).
--
-- `higgs` and `qwen3` are gone. The `tts` budget line now describes tts-breeze,
-- whose measured peak is 2243 MiB for four and a half minutes of continuous
-- audio (Q4_K_M, streamed, BREEZE_TEXT_ENC_CPU=1 — all three are the shipped
-- defaults). 2500 leaves headroom on that.
--
-- The figure only holds for the STREAMED path: buffering a long render grows
-- peak with output length (~4.1 GB measured), which is why every koblem caller
-- streams upstream even when it hands its own caller a single blob. The GPU
-- aligner adds ~634 MiB while resident, but idle-unloads after 20 s, so 2500 is
-- the steady state rather than the instantaneous worst case.

UPDATE gpu_engine_config SET peak_mb = 2500, updated_at = NOW() WHERE service = 'tts';

DELETE FROM gpu_engine_config WHERE service = 'higgs';

-- Retired service URLs + the engine selector. `tts_service_url` was the
-- kokoro/chatterbox-era key and has been dead for longer than the other two.
DELETE FROM settings WHERE key IN (
    'tts_qwen3_service_url',
    'tts_higgs_service_url',
    'tts_service_url',
    'default_tts_engine'
);

-- Normalise historical rows onto the single service name. The engine has been
-- called four things (`tts`, `tts_qwen3`, `tts_higgs`, and the `higgs` gate
-- tag); leaving old rows under the retired names makes them invisible to the
-- one filter the UI still offers.
UPDATE gpu_logs SET service = 'tts' WHERE service IN ('tts_qwen3', 'tts_higgs');
UPDATE gpu_logs
SET endpoint = '/api/v1/tts/generate'
WHERE endpoint IN ('/api/v1/tts/qwen3/generate', '/api/v1/tts/higgs/generate');
UPDATE generations SET service = 'tts' WHERE service IN ('tts_qwen3', 'tts_higgs');
