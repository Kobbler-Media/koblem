-- GPU logs: tier-2 request history + (opt-in) sample file retention.
-- Rolling retention runs in a background task; sweeper deletes oldest when
-- max_entries / max_age_days / max_sample_bytes thresholds trip.

CREATE TABLE gpu_samples (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL,            -- 'audio_in' | 'audio_out' | 'image_in'
    mime TEXT NOT NULL,
    path TEXT NOT NULL,            -- relative to the samples root (bind-mount)
    size_bytes BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX gpu_samples_created_at ON gpu_samples (created_at DESC);

CREATE TABLE gpu_logs (
    id BIGSERIAL PRIMARY KEY,
    service TEXT NOT NULL,         -- 'tts' | 'stt' | 'vision'
    endpoint TEXT NOT NULL,        -- route path
    request_meta JSONB NOT NULL,   -- knobs used; never raw audio/image bytes
    response_meta JSONB NOT NULL,  -- duration, byte sizes, segment counts, etc.
    sample_id BIGINT REFERENCES gpu_samples(id) ON DELETE SET NULL,
    gate_wait_ms BIGINT NOT NULL,
    duration_ms BIGINT NOT NULL,
    status INT NOT NULL,
    caller TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX gpu_logs_created_at ON gpu_logs (created_at DESC);
CREATE INDEX gpu_logs_service ON gpu_logs (service, created_at DESC);
