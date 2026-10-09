-- Saved-character library (avatar_assets) — §6.1 of GO-LIVE-PLAN.
-- The kid uploads a reference portrait once, picks it from the avatar page
-- on later renders via `image_source: { kind: "library_asset_id", id }`.
--
-- Files live under `${AVATARS_DIR}/_assets/<id>.<ext>`; the row is just the
-- pointer + original filename + created_at.

CREATE TABLE avatar_assets (
    id TEXT PRIMARY KEY,           -- uuid string (matches MusicTrack convention)
    original_filename TEXT NOT NULL,
    path TEXT NOT NULL,            -- relative to AVATARS_DIR (e.g. "_assets/<id>.png")
    content_type TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX avatar_assets_created_at ON avatar_assets (created_at DESC);
