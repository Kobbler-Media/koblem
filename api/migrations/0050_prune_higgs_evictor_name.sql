-- Drop the retired `higgs` service from every eviction set.
--
-- 0047 deleted the `higgs` row but left "higgs" in the other rows' `evictable_by`. The
-- gate ignores names it cannot parse, but the settings page sent it back on save and
-- the PUT rejected it ("unknown evictable_by service: higgs"), so no row could be edited.
UPDATE gpu_engine_config
SET evictable_by = (
    SELECT COALESCE(jsonb_agg(name), '[]'::jsonb)
    FROM jsonb_array_elements_text(evictable_by) AS t(name)
    WHERE t.name <> 'higgs'
)
WHERE evictable_by @> '["higgs"]'::jsonb;
