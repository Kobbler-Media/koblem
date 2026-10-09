-- Multi-GPU scheduler — raise `tts`'s `peak_mb` 2500 -> 3000.
--
-- Measured for Breeze (the only TTS engine since 0047), streamed long-form with the
-- aligner on: 2,243 MiB (4.5 min of audio, text encoder on the CPU) + 634 MiB aligner
-- = 2,877 MiB. 2500 was under that — the destructive direction for a peak, since the
-- gate then places a second engine into room that is not there.
--
-- 3000 is also what lets TTS run beside Beefy: the 27B leaves the 3060 about 3,129 MiB
-- after the measured-free clamp, so 3000 fits alone and a wrong-low number would be
-- the one thing standing between a co-tenant and a poisoned 27B.
--
-- ⚠️ BUFFERED synthesis is NOT covered: measured 4,309 MiB and it grows with output
-- length. Only koblem's manual generation uses it.
--
-- Only moves the seeded value: a peak an operator has already tuned by hand is left alone.
UPDATE gpu_engine_config
SET peak_mb = 3000, updated_at = now()
WHERE service = 'tts' AND peak_mb = 2500;
