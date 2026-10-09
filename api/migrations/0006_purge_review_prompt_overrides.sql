-- Wipe the per-section review prompt overrides so the new v3 default sticks.
--
-- The review pipeline switched from a per-candidate verdict response to a
-- blind accept-list + server-side diff. Old overrides (saved under the
-- pre-v3 schema) instruct the model to emit `verdicts[]` with action/
-- confidence/flags fields, which no longer parse. Easier to delete them
-- than to migrate the stored text — the user can re-customize from the
-- new default in the settings UI if they want.
DELETE FROM settings
WHERE key IN (
    'review_system_prompt',
    'review_signal_descriptions',
    'review_watch_patterns'
);
