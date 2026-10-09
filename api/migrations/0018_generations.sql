-- User-facing generation gallery. One row per generation request across every
-- GPU service (avatar | music | images | tts | stt | matting). This is the
-- asset record the UI lists + deletes per page; gpu_logs stays pure telemetry.
--
-- `outputs` is the per-request file set: [{filename, mime, size_bytes, kind}].
-- avatar = 1 webm, music/images = N, tts = 1 audio, stt = 1 transcript file.
-- The file root is resolved BY SERVICE at serve/delete time: avatar->AVATARS_DIR,
-- music->SONGS_DIR, everything else->the configurable `generations_dir` setting.
-- Generations own their files: deleting a row unlinks them (§decision-a).

CREATE TABLE generations (
    id           TEXT PRIMARY KEY,        -- uuid (matches avatar_assets / MusicTrack)
    service      TEXT NOT NULL,           -- 'avatar'|'music'|'images'|'tts'|'stt'|'matting'
    endpoint     TEXT NOT NULL,
    request_meta JSONB NOT NULL,          -- prompt/knobs (the "request written to DB")
    outputs      JSONB NOT NULL,          -- [{filename, mime, size_bytes, kind}]
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX generations_created_at ON generations (created_at DESC);
CREATE INDEX generations_service ON generations (service, created_at DESC);
