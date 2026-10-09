-- The shape nats-jobs' query macros compile against. koblem only uses the
-- crate's message types today, but `cargo sqlx prepare` builds the dependency
-- against this database, so the table has to exist for the cache to regenerate.
CREATE TABLE job_status (
    id UUID PRIMARY KEY,
    job_type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    payload JSONB,
    result JSONB,
    error TEXT,
    progress SMALLINT DEFAULT 0,
    entity_type TEXT,
    entity_id UUID,
    description TEXT,
    display JSONB,
    scheduled BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_job_status_entity ON job_status (entity_type, entity_id)
    WHERE entity_type IS NOT NULL AND entity_id IS NOT NULL;

CREATE INDEX idx_job_status_active ON job_status (status)
    WHERE status IN ('pending', 'running', 'retrying');

CREATE INDEX idx_job_status_type_active ON job_status (job_type, status)
    WHERE status IN ('pending', 'running', 'retrying');

CREATE INDEX idx_job_status_created ON job_status (created_at DESC);
