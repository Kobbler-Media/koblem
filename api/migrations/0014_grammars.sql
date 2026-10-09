-- Grammar selection moved from a single `review_grammar` settings key to a
-- first-class `grammars` table. The sampling layer now picks among "off"
-- (no grammar), "review" (the built-in koblem accept-list grammar baked
-- into koblem-client), or any user-defined grammar referenced by name.
-- See koblem_client::types::GrammarMode.

CREATE TABLE grammars (
    name TEXT PRIMARY KEY,
    gbnf TEXT NOT NULL,
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The old single-grammar override no longer exists. Anyone who'd customised
-- the review GBNF needs to re-set it as a user-defined grammar in the new
-- table and pick it via the sampling block.
DELETE FROM settings WHERE key = 'review_grammar';

-- Capture the resolved grammar mode in llm_logs alongside `sampling_preset`
-- so a log row tells you which grammar (if any) constrained the run.
ALTER TABLE llm_logs ADD COLUMN request_grammar TEXT;
