CREATE TABLE review_sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Input
    request JSONB NOT NULL,
    -- Context assembled
    wanted_summary TEXT NOT NULL,
    disambiguation TEXT,
    quality_hint TEXT,
    corrections TEXT,
    context_used TEXT[] NOT NULL DEFAULT '{}',
    -- Prompt + LLM
    prompt_sent TEXT NOT NULL,
    llm_response TEXT,
    -- Results
    verdicts JSONB,
    notes TEXT,
    -- Timing
    context_assembly_ms BIGINT,
    llm_inference_ms BIGINT,
    -- Metadata
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_review_sessions_created ON review_sessions(created_at DESC);
