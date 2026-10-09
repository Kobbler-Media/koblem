-- Agentic chat: the local LLM orchestrates every GPU engine via real OpenAI
-- tool calls. One row per session, one per message.
--
-- A turn's work runs in a background task, NOT in the browser's request, so the
-- assistant row is the durable record the browser polls: `status` ticks
-- running → complete|error|cancelled and `steps` grows as each tool call is
-- dispatched. A disconnect is a non-event (same posture as `LtxJobStore`).
--
-- `attachments` holds `{kind, asset_ref, mime}` — a content-addressed
-- `asset:<hash>` ref into the director asset store, never the bytes, so
-- replaying a conversation into the LLM does not re-read megabytes out of
-- Postgres. `steps` holds the wire `Step` shape with output URLs stored
-- token-less; the read path appends the caller's own token.

CREATE TABLE agent_sessions (
    id         UUID PRIMARY KEY,
    title      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE agent_messages (
    id               UUID PRIMARY KEY,
    session_id       UUID NOT NULL REFERENCES agent_sessions(id) ON DELETE CASCADE,
    seq              INTEGER NOT NULL,
    role             TEXT NOT NULL,
    content          TEXT NOT NULL DEFAULT '',
    attachments      JSONB NOT NULL DEFAULT '[]'::jsonb,
    steps            JSONB NOT NULL DEFAULT '[]'::jsonb,
    status           TEXT NOT NULL DEFAULT 'complete',
    cancel_requested BOOLEAN NOT NULL DEFAULT FALSE,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (session_id, seq)
);

CREATE INDEX agent_messages_session_seq ON agent_messages (session_id, seq);

-- Sweep any turn left `running` by a crashed process — it would otherwise tick
-- forever in the UI and the frontend would poll it until the heat death.
UPDATE agent_messages SET status = 'cancelled' WHERE status = 'running';
