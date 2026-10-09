-- Purge DB overrides so the new baked v17c prompt + object-with-reason grammar
-- become active on next deploy. Users can re-override via the settings API.
DELETE FROM settings WHERE key IN (
    'review_system_prompt',
    'review_grammar',
    'review_signal_descriptions',
    'review_watch_patterns'
);
