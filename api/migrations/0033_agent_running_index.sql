-- The startup sweep (`cancel_stale_running`) and every `request_cancel` select
-- on `status = 'running'`, which is a handful of rows out of the whole message
-- history. Partial, so the index stays the size of the live set rather than the
-- transcript.
CREATE INDEX agent_messages_running
    ON agent_messages (session_id)
    WHERE status = 'running';
