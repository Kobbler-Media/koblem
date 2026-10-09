-- Content-addressed store for Director editor assets.
--
-- Storyboard stills, i2v scene images, keyframe pins, uploaded v2v source clips
-- and per-shot audio all used to live as inline base64 INSIDE the project's
-- `director_projects.timeline` JSONB. That made the document carry every byte in
-- the project, so each autosave (2.5s after any edit) re-serialised and rewrote
-- the whole row, each render request re-inlined it, each undo snapshot copied it,
-- and the localStorage AI-revert copy blew its quota outright. The cost tracked a
-- project's ASSETS rather than its shot count, which is why small projects with a
-- couple of stills and one uploaded clip felt slow.
--
-- Assets now live here once, keyed by the SHA-256 of their bytes, and the timeline
-- stores an `asset:<hash>` reference instead. Content addressing makes the store
-- self-deduplicating (the same still reused across shots is one row + one file)
-- and every asset URL immutable, so the editor can cache them permanently.
--
-- Bytes live on disk under DIRECTOR_ASSETS_DIR (bind-mounted like the render
-- store); this table is the metadata + the GC ledger. `last_used_at` is bumped on
-- read so the sweep can retire assets no surviving project references.
CREATE TABLE director_assets (
    hash         TEXT PRIMARY KEY,                 -- lowercase hex SHA-256 of the bytes
    content_type TEXT NOT NULL,
    bytes        BIGINT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX director_assets_last_used_idx ON director_assets (last_used_at);
