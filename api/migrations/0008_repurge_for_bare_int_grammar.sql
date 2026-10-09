-- Third purge of review prompt / grammar overrides so the deploy cleanly
-- picks up the bare-integer grammar + v11 "no reason field" prompt baked
-- into source this round. The DB currently holds overrides from the v11
-- tuning session (object-form grammar, object-form prompt text) which still
-- work correctly but don't benefit from the ~20% output-token reduction of
-- the bare-integer form. Wiping forces a fresh start on the baked defaults.
--
-- No-op for anyone who hasn't customized — the DELETE matches zero rows.
DELETE FROM settings
WHERE key IN (
    'review_system_prompt',
    'review_signal_descriptions',
    'review_watch_patterns',
    'review_grammar'
);
