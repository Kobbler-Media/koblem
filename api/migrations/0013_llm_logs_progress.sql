ALTER TABLE llm_logs
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'complete',
    ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ADD COLUMN IF NOT EXISTS error_message TEXT;

CREATE INDEX IF NOT EXISTS idx_llm_logs_running ON llm_logs(updated_at DESC) WHERE status = 'running';
