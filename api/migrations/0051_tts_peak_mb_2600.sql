-- Multi-GPU scheduler — lower `tts`'s `peak_mb` to 2600.
--
-- The forced aligner now always runs on the CPU (tts_breeze.rs), and the engine
-- batches two streams on one set of weights. Measured peak of the TTS process, two
-- concurrent 400-word passages (the largest request koblem's callers send), streamed,
-- aligner on the CPU: 2,408 MiB on the 3060, 2,508 MiB on the 5060 Ti. With the
-- aligner on the GPU the same run was 2,936 MiB, which this value does NOT cover.
--
-- 2600 covers either card with ~90-190 MiB to spare, and leaves the 27B's co-tenant
-- room on the 3060 (~3,129 MiB after the measured-free clamp) more slack than 3000 did.
--
-- ⚠️ BUFFERED synthesis is still NOT covered (measured 4,309 MiB, grows with length).
--
-- Moves the seeded 3000 and the hand-set 2800; any other operator-tuned value stays.
UPDATE gpu_engine_config
SET peak_mb = 2600, updated_at = now()
WHERE service = 'tts' AND peak_mb IN (2800, 3000);
