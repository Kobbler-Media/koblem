-- Director 2 saved projects: an editable, re-runnable timeline of video shots.
--
-- One row = one storyboard/timeline the user built in the /ltx-director editor.
-- `timeline` holds the whole editor doc as JSON: the compiled render request
-- (segments/prompts/keyframes/model/fps/audio), inline base64 assets, per-clip
-- layout (in/out, duration, x-position, v2v/i2v/t2v mode), and the cached lyric
-- alignment. The backend persists it opaquely and, on /run, extracts the
-- embedded `request` (an LtxVideoGenerateRequest) + assets and dispatches the
-- SAME chain render the /ltx-video/generate path uses. Rendered clips are NOT
-- owned by this row — they land in the `generations` gallery like any render.
--
-- id is a caller-supplied uuid string (matches generations/avatar_assets).
CREATE TABLE director_projects (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    timeline   JSONB NOT NULL,               -- {request, assets, editor, lyrics}
    metadata   JSONB NOT NULL DEFAULT '{}',  -- thumbnails, last_job_id, notes
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Listing is newest-edited first.
CREATE INDEX director_projects_updated_at ON director_projects (updated_at DESC);
