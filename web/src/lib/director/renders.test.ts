import { describe, expect, it } from 'vitest';
import {
  adoptCompletedBank,
  bankedResumeTarget,
  clipKey,
  dispatchIsProjectEngine,
  harvestSettled,
  jobClipKey,
  mediaIsFragment,
  planCutHydration,
  planHydration,
  type HeldClip,
  type RenderRowLike
} from './renders';

function row(
  tier: string,
  shot_index: number,
  source_job_id: string | null = 'job-1',
  created_at = '2026-07-31T10:00:00Z'
): RenderRowLike {
  return { tier, shot_index, source_job_id, created_at, url: `/renders/${tier}/${shot_index}` };
}

function held(index: number, key: string): HeldClip {
  return { index, url: `blob:${key}`, key };
}

/** A clip that was DOWNLOADED from a row (rather than streamed off a live render). */
function fetched(r: RenderRowLike): HeldClip {
  return { index: r.shot_index, url: `blob:${clipKey(r)}`, key: clipKey(r) };
}

describe('planHydration', () => {
  /** The whole point of the tier: a phone opening a project with an existing final render must not
   *  pull 12 shots at 1.4–19.5 MB each to display 512×288 previews. */
  it('downloads only the tier this client renders in', () => {
    const rows = [row('final', 0), row('final', 1), row('proxy', 0), row('proxy', 1)];

    const proxy = planHydration(rows, 'proxy', []);
    expect(proxy.fetch.map((r) => r.url)).toEqual(['/renders/proxy/0', '/renders/proxy/1']);

    const final = planHydration(rows, 'final', []);
    expect(final.fetch.map((r) => r.url)).toEqual(['/renders/final/0', '/renders/final/1']);
  });

  /** The post-render re-read: the tab that watched the render already holds those exact bytes, so
   *  it must cost rows and nothing else. */
  it('keeps the clips it streamed live off the render that produced them', () => {
    const rows = [row('proxy', 0), row('proxy', 1), row('proxy', 2, 'job-2')];
    const plan = planHydration(rows, 'proxy', [
      held(0, jobClipKey('job-1', 0)),
      held(1, jobClipKey('job-1', 1))
    ]);

    expect(plan.fetch.map((r) => r.shot_index)).toEqual([2]);
    expect(plan.keep.map((h) => h.index)).toEqual([0, 1]);
    expect(plan.drop).toEqual([]);
  });

  /** A re-roll is a NEW take of that shot: same index, different producing render, new bytes. */
  it('re-fetches a shot whose take changed, and revokes the old blob', () => {
    const plan = planHydration([row('proxy', 0, 'job-retake')], 'proxy', [
      held(0, jobClipKey('job-1', 0))
    ]);
    expect(plan.fetch.map((r) => r.shot_index)).toEqual([0]);
    expect(plan.keep).toEqual([]);
    expect(plan.drop).toEqual(['blob:job:job-1:0']);
  });

  /** A full run replaces the tier; shots the new run does not cover are gone, not stale. */
  it('drops held clips no row backs any more', () => {
    const plan = planHydration([row('proxy', 0)], 'proxy', [
      held(0, jobClipKey('job-1', 0)),
      held(1, jobClipKey('job-1', 1))
    ]);
    expect(plan.fetch).toEqual([]);
    expect(plan.drop).toEqual(['blob:job:job-1:1']);
  });

  /** Switching tier and back must not re-download what the first switch already pulled. */
  it('keeps clips it downloaded from unchanged rows', () => {
    const rows = [row('final', 0), row('final', 1)];
    const plan = planHydration(rows, 'final', rows.map(fetched));
    expect(plan.fetch).toEqual([]);
    expect(plan.keep.map((h) => h.index)).toEqual([0, 1]);
    expect(plan.drop).toEqual([]);
  });

  /** A row from before takes existed names no render, so a clip streamed off one cannot stand in
   *  for it — it is pulled rather than assumed. */
  it('never matches a live-streamed clip against an unattributable row', () => {
    const plan = planHydration([row('proxy', 0, null)], 'proxy', [held(0, jobClipKey('job-1', 0))]);
    expect(plan.fetch.map((r) => r.shot_index)).toEqual([0]);
    expect(plan.drop).toEqual(['blob:job:job-1:0']);
  });
});

describe('planCutHydration', () => {
  it('keeps the stitched cut it just streamed', () => {
    const rows = [row('proxy', -1), row('proxy', 0)];
    expect(
      planCutHydration(rows, 'proxy', { url: 'blob:cut', key: jobClipKey('job-1', -1) })
    ).toEqual({ fetch: null, keep: true, drop: null });
  });

  /** Picking a take rebuilds the cut IN PLACE: same producing id, same file name, new bytes. Only
   *  the write time tells them apart, so a key without it would pin the first rebuild forever. */
  it('re-fetches a cut rebuilt in place', () => {
    const first = row('proxy', -1, 'rebuild:proxy', '2026-07-31T10:00:00Z');
    const second = row('proxy', -1, 'rebuild:proxy', '2026-07-31T10:05:00Z');
    expect(planCutHydration([first], 'proxy', fetched(first)).keep).toBe(true);

    const again = planCutHydration([second], 'proxy', fetched(first));
    expect(again.keep).toBe(false);
    expect(again.fetch?.shot_index).toBe(-1);
    expect(again.drop).toBe(`blob:${clipKey(first)}`);
  });

  it('has nothing to play when the tier has no stitched cut', () => {
    expect(planCutHydration([row('final', -1)], 'proxy', null)).toEqual({
      fetch: null,
      keep: false,
      drop: null
    });
  });
});

describe('bankedResumeTarget', () => {
  const banked = (
    tier: string,
    shot_index: number,
    latent_job_id: string | null,
    engine: string | null = 'ltx'
  ) => ({
    tier,
    shot_index,
    latent_job_id,
    engine
  });

  /** 🔴 THE BLOCKER: a second continue used to be a hard 400. The engine resolves `resume_job_id`
   *  by walking `"<job>:<segment>"` from segment 0, and a continue banks only the shots it rendered
   *  under a fresh id — so job B's bank holds 5-6 and no segment 0 at all. LTX-2.3 masked it by
   *  aliasing the fresh id back onto the bank it continued; LTX-2.5 has no alias. */
  it('names the chain that banked shot 0, not the newest chain in the tier', () => {
    const rows = [
      banked('final', -1, 'srv-b'),
      banked('final', 0, 'srv-a'),
      banked('final', 4, 'srv-a'),
      banked('final', 5, 'srv-b'),
      banked('final', 6, 'srv-b')
    ];
    expect(bankedResumeTarget(rows, 'final', 'ltx')).toBe('srv-a');
  });

  /** Three continues deep the tier spans three banks, and the answer must not drift with the last
   *  one — that is the chain length the fix has to survive. */
  it('holds still across a chain of continues', () => {
    const rows = [banked('final', 0, 'srv-a')];
    for (const [shot, job] of [
      [5, 'srv-b'],
      [7, 'srv-c'],
      [9, 'srv-d']
    ] as const) {
      rows.push(banked('final', shot, job));
      expect(bankedResumeTarget(rows, 'final', 'ltx')).toBe('srv-a');
    }
  });

  /** The two tiers are independent chains with independent banks. */
  it('answers per tier', () => {
    const rows = [banked('final', 0, 'srv-final'), banked('proxy', 0, 'srv-proxy')];
    expect(bankedResumeTarget(rows, 'proxy', 'ltx')).toBe('srv-proxy');
    expect(bankedResumeTarget(rows, 'final', 'ltx')).toBe('srv-final');
  });

  /** A row from before the column existed says nothing — the caller keeps the id it holds rather
   *  than downgrading a working resume to a full re-render of the prefix. */
  it('is null when shot 0 names no chain', () => {
    expect(bankedResumeTarget([banked('final', 0, null)], 'final', 'ltx')).toBeNull();
    expect(bankedResumeTarget([banked('final', 3, 'srv-a')], 'final', 'ltx')).toBeNull();
    expect(bankedResumeTarget([], 'final', 'ltx')).toBeNull();
  });

  /** 🔴 The engine picker sits above a project that has already rendered, and the two LTX
   *  deployments keep separate job stores — so after a switch every id in these rows is a 404 at
   *  the far end. A row that cannot say which store it belongs to is in the same position. */
  it('never names a chain another engine banked', () => {
    const rows = [banked('final', 0, 'srv-23', 'ltx')];
    expect(bankedResumeTarget(rows, 'final', 'ltx25')).toBeNull();
    expect(bankedResumeTarget(rows, 'final', 'h3')).toBeNull();
    expect(bankedResumeTarget([banked('final', 0, 'srv-x', null)], 'final', 'ltx')).toBeNull();
    // Switching back reaches it again: the rows keep their own engine, so nothing was lost.
    expect(bankedResumeTarget(rows, 'final', 'ltx')).toBe('srv-23');
  });

  /** Both engines' rows coexist once a project has rendered on each, and each engine takes its own. */
  it('picks this engine`s chain out of a tier that holds two', () => {
    const rows = [banked('final', 0, 'srv-23', 'ltx'), banked('final', 0, 'srv-25', 'ltx25')];
    expect(bankedResumeTarget(rows, 'final', 'ltx')).toBe('srv-23');
    expect(bankedResumeTarget(rows, 'final', 'ltx25')).toBe('srv-25');
  });
});

describe('adoptCompletedBank', () => {
  const completion = (over: Partial<Parameters<typeof adoptCompletedBank>[0]> = {}) =>
    adoptCompletedBank({
      dispatchEngine: 'ltx',
      projectEngine: 'ltx',
      pending: null,
      banked: 'srv-23',
      held: null,
      ...over
    });

  /** The ordinary render: a fresh chain adopts the dir the engine banked into. */
  it('adopts the fresh chain a render banked', () => {
    expect(completion()).toEqual({ resumeJobId: 'srv-23', adopt: true });
  });

  /** A resume keeps the id it resumed FROM — the fresh one a continue mints holds no segment 0. */
  it('keeps the resumed target over the id the continue minted', () => {
    expect(completion({ pending: 'srv-a', banked: 'srv-b' })).toEqual({
      resumeJobId: 'srv-a',
      adopt: true
    });
  });

  /** An engine that reports no dir of its own leaves the editor holding what it had. */
  it('falls back to the held id when the job names no bank', () => {
    expect(completion({ banked: null, held: 'srv-old' })).toEqual({
      resumeJobId: 'srv-old',
      adopt: true
    });
  });

  /**
   * 🔴 THE BLOCKER, and it is one click: render on `ltx`, open Settings mid-render, switch to
   * `ltx25`. `changeEngine` correctly clears the persisted targets, and then the 2.3 render finishes
   * and puts its 2.3 dir id back — onto a project now on 2.5, made durable by the autosave beside
   * it. Nothing downstream rescues that: `bankedResumeTarget` rightly answers `null` against
   * `ltx`-stamped rows, and `null` never clears a held id.
   */
  it('banks nothing when the project left the engine the render was dispatched to', () => {
    expect(completion({ projectEngine: 'ltx25', pending: 'srv-a' })).toEqual({
      resumeJobId: null,
      adopt: false
    });
    expect(completion({ projectEngine: 'h3', held: 'srv-old' })).toEqual({
      resumeJobId: 'srv-old',
      adopt: false
    });
  });

  /** A render adopted from a session that recorded no engine cannot claim one: unknown is not a
   *  match, exactly as a row with no `engine` names no chain. The next read of the durable rows
   *  supplies the id when it really is this engine's. */
  it('treats an unknown dispatch engine as foreign', () => {
    expect(completion({ dispatchEngine: null, held: 'srv-old' })).toEqual({
      resumeJobId: 'srv-old',
      adopt: false
    });
  });
});

describe('dispatchIsProjectEngine', () => {
  /** The failure branch of a render reads this too: a render that no longer speaks for the project
   *  may not clear the tier's resume target any more than it may install one. Table-driven because
   *  the point is that all three answers are the SAME rule the completion takes. */
  it.each([
    ['ltx', 'ltx', true],
    ['ltx', 'ltx25', false],
    [null, 'ltx', false]
  ] as const)('dispatch %s against project %s → %s', (dispatch, project, expected) => {
    expect(dispatchIsProjectEngine(dispatch, project)).toBe(expected);
  });
});

describe('harvestSettled', () => {
  /** `/active` holds a finished render until its harvest lands — that signal is what replaces the
   *  client's blind 2s sleep. */
  it('waits while a render of this project is still persisting', () => {
    expect(harvestSettled([{ project_id: 'p1', harvesting: true }], 'p1')).toBe(false);
    expect(harvestSettled([{ project_id: 'p2', harvesting: true }], 'p1')).toBe(true);
    expect(harvestSettled([], 'p1')).toBe(true);
  });

  /** A render still RUNNING is not rewriting the durable rows, so opening the project must not sit
   *  and wait on it — that would stall every load made while another device renders. */
  it('does not wait on a render that is merely in flight', () => {
    expect(harvestSettled([{ project_id: 'p1', harvesting: false }], 'p1')).toBe(true);
    expect(harvestSettled([{ project_id: 'p1' }], 'p1')).toBe(true);
  });
});

describe('mediaIsFragment', () => {
  /** A resume/retake on an engine that does not splice the tail: known one dispatch before the
   *  first poll, which is the whole reason the prediction is kept. */
  it('takes the dispatcher word before any job body exists', () => {
    expect(mediaIsFragment(true, undefined)).toBe(true);
    expect(mediaIsFragment(true, null)).toBe(true);
  });

  /** THE GAP THIS EXISTS FOR: a FULL render whose concatenation failed. Nothing about the engine
   *  predicts it, so the dispatcher says "not a fragment" and only the job says otherwise. */
  it('adopts a partial reported by a render nobody predicted would be one', () => {
    expect(mediaIsFragment(false, true)).toBe(true);
  });

  /** Not all engines publish the flag — the C++ `ltx-video` engine publishes neither it nor a
   *  render window — so a false/absent one must read as "nothing to add", never as a correction.
   *  ⚠️ An H3 single-generation retake is NOT an example any more: koblem ORs the job's shot offset
   *  into the status body (`LtxJob::delivers_fragment`), so a client that only watched it is told. */
  it('never lets a silent engine unset the dispatcher', () => {
    expect(mediaIsFragment(true, false)).toBe(true);
  });

  it('leaves an ordinary full render alone', () => {
    expect(mediaIsFragment(false, false)).toBe(false);
    expect(mediaIsFragment(false, undefined)).toBe(false);
  });
});
