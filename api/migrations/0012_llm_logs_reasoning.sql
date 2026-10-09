ALTER TABLE llm_logs
    ADD COLUMN response_reasoning_content TEXT,
    ADD COLUMN request_enable_thinking BOOLEAN;
