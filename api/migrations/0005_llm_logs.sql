CREATE TABLE llm_logs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Request
    caller TEXT,
    request_messages JSONB NOT NULL,
    request_tools JSONB,
    request_temperature DOUBLE PRECISION,
    request_max_tokens INTEGER,
    streaming BOOLEAN NOT NULL DEFAULT FALSE,
    -- Response
    response_content TEXT,
    response_tool_calls JSONB,
    finish_reason TEXT,
    -- Tool rewriting metadata
    tool_rewrite_applied BOOLEAN NOT NULL DEFAULT FALSE,
    -- Timing
    duration_ms BIGINT,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    -- Metadata
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_llm_logs_created ON llm_logs(created_at DESC);
