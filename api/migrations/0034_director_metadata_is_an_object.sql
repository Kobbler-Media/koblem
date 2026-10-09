-- `director_projects.metadata` must be a JSON OBJECT, enforced by the database.
--
-- Every writer merges into it with `||`, and Postgres happily evaluates
-- `'null'::jsonb || '{"a":1}'::jsonb` as the ARRAY `[null, {"a":1}]` rather than raising. So one
-- `PUT {"metadata": null}` turned a row's metadata into a value where every later render stamp
-- appended another element and every `.get(key)` returned nothing, permanently — no error, ever.
--
-- The route layer rejects a non-object body with a 400, but that is the second line: a CHECK is what
-- makes the shape true for every writer, including a hand-run UPDATE.
UPDATE director_projects SET metadata = '{}'::jsonb WHERE jsonb_typeof(metadata) <> 'object';

ALTER TABLE director_projects
    ADD CONSTRAINT director_projects_metadata_is_object
    CHECK (jsonb_typeof(metadata) = 'object');
