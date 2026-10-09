-- Link review_sessions to the underlying llm_logs row so the History page can
-- pull the raw chain-of-thought trace (response_reasoning_content) without
-- duplicating it. Nullable: legacy sessions and any non-streaming review
-- paths (playground search/compare, testbed) won't have a paired log.
ALTER TABLE review_sessions
    ADD COLUMN llm_log_id UUID REFERENCES llm_logs(id) ON DELETE SET NULL;

CREATE INDEX idx_review_sessions_llm_log ON review_sessions(llm_log_id);
