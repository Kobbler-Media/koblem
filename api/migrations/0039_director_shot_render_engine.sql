-- Which ENGINE's store the ids on this row address — the thing that makes a foreign bank unnameable.
--
-- # The failure
--
-- `latent_job_id` (0027) and `bank_key` (0038) are both opaque strings a later render hands back to
-- an engine, and neither carries any statement about WHICH engine. A project's engine is a field on
-- its timeline that the author may change at any time, including after it has rendered — the picker
-- says so in as many words. So `render on ltx → switch to ltx25 → Render this shot` resolved a chain
-- out of these rows and sent an LTX-2.3 job id to LTX-2.5's flat bank, where it is a 404; and
-- `restore_selected_takes` stamped every shot's `bank_job_id` with 2.3 ids in the same request. The
-- two deployments have separate job stores and separate adapter directories, and `client.ts` already
-- said the consequence out loud: "a job id from one is a 404 on the other".
--
-- Resetting the editor's resume target on the switch is necessary and not sufficient: three server
-- reads (`selected_bank_ids`, `first_shot_latent_job_id`, `latent_chains_by_project_tier`) resolve a
-- chain straight out of this table with no client involved, and the unattended critic dispatches
-- through them at night. Recording the engine is what makes a cross-engine chain impossible to name
-- rather than merely unlikely.
--
-- It also fixes the cleanup routing. `DELETE /…/job?id=` was sent to whichever engine the PROJECT is
-- on now, so after a switch the persist-store dirs of the previous engine were asked of a service
-- that has never heard of them — an orphan directory in a store nothing sweeps.
--
-- # The backfill, and what it deliberately does not claim
--
-- It claims one thing, and only where that thing is true of the row in front of it: TODAY only the
-- C++ `ltx-video` engine delivers VP9-in-webm. The H3 shim delivers x265-in-mp4 and `ltx25` delivers
-- h264-in-mp4 (ComfyUI's only audio-carrying save node is `SaveVideo`, whose format enum is
-- `["auto", "mp4"]`), and `LtxJob::clip_ext` — which names the file and, since `ltx25` landed on
-- 2026-08-12, supplies the harvest's fallback mime — states both today. (It arrived with `7349da1`
-- below, four days after H3 itself, so it is evidence about the shape of things NOW rather than a
-- witness to the whole history — which is what the next paragraph is about.)
--
-- 🔴 It was NOT always true, and the exception is in production history. Between 2026-08-02T14:11Z
-- (`2bf7299`, "director: drive MiniMax-H3 from the LTX Director") and 2026-08-06T08:35Z (`7349da1`,
-- which introduced `H3_MIME_TYPE = video/mp4`), a completed H3 job stamped
-- `content_type = "video/webm"` from three hardcoded sites in `routes/minimax_h3.rs`, and the
-- harvest's own fallback for a job that carried none was a hardcoded `"video/webm"` — while H3
-- rendered through this very `maybe_harvest_director` the whole time. So an H3 take written in that
-- window is a `video/webm` row with a non-null `latent_job_id` (H3 sets `server_job_id`), i.e.
-- nameable, and stamping it `ltx` would hand an H3 task id to an LTX engine: a hard 400 on 2.5, and
-- on the C++ engine a silent fall-back that re-renders everything and reports it as a retake. That
-- is the exact direction of error this column exists to prevent, so the backfill WITHHOLDS the claim
-- over that span instead of asserting it.
--
-- The span excluded runs from the start of the first commit's own UTC day to the start of
-- 2026-08-08. The TRAILING ~1.6 days are the one thing slack can buy: a commit time is not a deploy
-- time, the lag runs one way, and one deploy closes it.
--
-- 🔴 The other lag is not the same shape and slack does not cover it. `created_at` is not a render
-- date — `director_shot_renders::upsert` sets `created_at = NOW()` on conflict, so a re-harvested row carries
-- its LAST harvest, and a window-era H3 row re-harvested after 08-08 would land outside this span
-- and be stamped `ltx`. What actually excludes it is the same `ON CONFLICT` clause: it also sets
-- `content_type = EXCLUDED.content_type`, and the value a re-harvest supplies comes from the code
-- running THEN rather than from the row. Post-`7349da1` that value is `video/mp4` on every H3 path
-- — the completion and both re-attach branches stamp `H3_MIME_TYPE` before harvesting
-- (`routes/minimax_h3.rs`), and `maybe_harvest_director`'s own fallback is `video/{clip_ext}` — so
-- such a row leaves the `LIKE 'video/webm%'` filter entirely rather than needing the window to
-- reach it. (The lag is bounded anyway, if not by a deploy: the restart re-attach skips terminal
-- jobs, so only a render still IN FLIGHT is ever harvested a second time.)
--
-- 🔴 The LEADING edge is the commit itself, 14:11Z, not the top of the UTC day. `created_at` is
-- `NOW()` at the write, so it can never precede the render that caused it, and a deploy can never
-- precede its own commit — so no H3 row can exist before 2026-08-02T14:11Z, and withholding the
-- ~14 h before it would only have cost a genuine `ltx` render its chain for no gain. The tight edge
-- is what a leading boundary can honestly claim, so it is the one used.
--
-- 🔴 The mp4 rows are left NULL too, for a different reason: mp4 is what BOTH shim engines deliver,
-- so it separates them from `ltx` and not from each other. Every read below excludes a NULL, which
-- costs an `h3` or `ltx25` project written before this migration one un-pinned render — and now
-- costs a genuine `ltx` render made during those four days the same. That is the whole price of
-- withholding, and it is the cheap side of the trade: a NULL row names no chain, the reads fall back
-- to the project's CURRENT engine, and that fallback is exactly today's pre-migration behaviour. A
-- wrong `ltx` is not recoverable by anything downstream.
--
-- Backfilling the unknowns from the project's current engine was the obvious alternative and is
-- exactly wrong: the projects it would mislabel are the ones that have already switched, i.e.
-- precisely the population this column exists to protect. Excluding "projects currently on h3" from
-- the withheld span is the same mistake in miniature — a project that rendered on H3 in that window
-- and has since moved to LTX is both the dangerous case and invisible to such a test. There is no
-- record of a switch to consult, so "unknown" is the only true value, and an unknown chain is one
-- nothing may name. The first render after this migration stamps every row it writes and the gap
-- closes itself.
--
-- Nullable and additive; no index (every read is already narrowed by the
-- `(project_id, tier, shot_index)` primary-key prefix).

ALTER TABLE director_shot_renders ADD COLUMN IF NOT EXISTS engine TEXT;

UPDATE director_shot_renders
SET engine = 'ltx'
WHERE engine IS NULL
  AND content_type LIKE 'video/webm%'
  AND (
      created_at < TIMESTAMPTZ '2026-08-02 14:11:00+00'
      OR created_at >= TIMESTAMPTZ '2026-08-08 00:00:00+00'
  );
