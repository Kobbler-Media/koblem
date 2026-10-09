-- Per-media-kind templated review prompts.
--
-- Two tables replace the three legacy `settings` keys
-- (review_system_prompt / review_signal_descriptions / review_watch_patterns).
-- `prompt_templates` holds the system + user slots; `prompt_variables` holds
-- named blobs (signal_descriptions, watch_patterns, format, plus any user
-- additions). Both are keyed by (media_kind, slot|name) where media_kind is
-- 'default' or one of the WantContext snake_case variants.

CREATE TABLE prompt_templates (
    media_kind TEXT NOT NULL,
    slot       TEXT NOT NULL,
    body       TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (media_kind, slot)
);

CREATE TABLE prompt_variables (
    media_kind TEXT NOT NULL,
    name       TEXT NOT NULL,
    body       TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (media_kind, name)
);

-- Backfill any pre-existing edits the user made via the legacy /settings/prompt
-- endpoints. The settings table stores the body as a JSONB scalar string, so
-- `value #>> '{}'` unwraps it to plain text.

-- Legacy templates used `{patterns}` and `{format}` placeholders substituted
-- by Rust's String::replace. The new engine is minijinja, so any user-edited
-- template that referenced those placeholders gets auto-converted to the
-- equivalent variable references. Templates without the placeholders (the
-- v17c default and most user edits) pass through untouched.
INSERT INTO prompt_templates (media_kind, slot, body)
SELECT
    'default',
    'system',
    REPLACE(
        REPLACE(value #>> '{}', '{patterns}', '{{ vars.signal_descriptions }}' || E'\n\n' || '{{ vars.watch_patterns }}'),
        '{format}',
        '{{ vars.format }}'
    )
FROM settings
WHERE key = 'review_system_prompt';

INSERT INTO prompt_variables (media_kind, name, body)
SELECT 'default', 'signal_descriptions', value #>> '{}'
FROM settings
WHERE key = 'review_signal_descriptions';

INSERT INTO prompt_variables (media_kind, name, body)
SELECT 'default', 'watch_patterns', value #>> '{}'
FROM settings
WHERE key = 'review_watch_patterns';

DELETE FROM settings WHERE key IN (
    'review_system_prompt',
    'review_signal_descriptions',
    'review_watch_patterns'
);

-- The user prompt is now fully template-driven. There is no separate
-- pre-rendered "quality hint" / "disambiguation" / "corrections" segment any
-- more — the entire user message is the rendered output of the user-template
-- slot, already captured in `prompt_sent`. The List view's compact row label
-- (`wanted_summary`) is now derived cheaply from the stored request JSON.
ALTER TABLE review_sessions
    DROP COLUMN IF EXISTS disambiguation,
    DROP COLUMN IF EXISTS quality_hint,
    DROP COLUMN IF EXISTS corrections;
