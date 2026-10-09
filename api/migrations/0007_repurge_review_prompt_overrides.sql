-- Re-purge the per-section review prompt overrides so the new v6 default
-- sticks. The v6 prompt (accept-first framing + 5-gate pre-emit checklist +
-- grammar field reorder with notes-before-accept) is a material behavior
-- change from v5, and any customizations someone saved via the settings UI
-- would lock in the old reject-first framing. Easier to wipe than migrate.
--
-- 0006 already did this purge once; this migration repeats it for the
-- v5→v6 transition. If the table has no matching rows (nobody customized
-- since the last deploy), this is a no-op.
DELETE FROM settings
WHERE key IN (
    'review_system_prompt',
    'review_signal_descriptions',
    'review_watch_patterns'
);
