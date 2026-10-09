import { describe, expect, it } from 'vitest';
import {
  continuityOf,
  pendingPinNote,
  pinOf,
  repairPlan,
  type TakeLike,
  type UnitTakes
} from './take-continuity';

const T = (o: Partial<TakeLike> & { variant: number }): TakeLike => ({
  selected: false,
  created_at: '2026-08-08T10:00:00Z',
  ...o
});

/** A full run: one take per unit, all from the same koblem job, all selected. */
const fullRun = (n: number, job = 'run-1'): UnitTakes[] =>
  Array.from({ length: n }, (_, i) => ({
    index: i,
    takes: [
      T({ variant: 0, selected: true, source_job_id: job, created_at: '2026-08-08T10:00:00Z' })
    ],
    join: { carries: i > 0 }
  }));

describe('pinOf', () => {
  it('takes the same-render sibling as definitive', () => {
    const prev = [T({ variant: 0, source_job_id: 'a' }), T({ variant: 1, source_job_id: 'b' })];
    const pin = pinOf(T({ variant: 0, source_job_id: 'a' }), prev);
    expect(pin).toMatchObject({ kind: 'exact', takeNo: 1, basis: 'same-render' });
  });

  it('eliminates predecessor takes that did not exist yet', () => {
    const prev = [
      T({ variant: 0, created_at: '2026-08-08T10:00:00Z' }),
      T({ variant: 1, created_at: '2026-08-08T12:00:00Z' })
    ];
    // A retake at 11:00 — job id matches nothing, but only take 1 existed then.
    const pin = pinOf(
      T({ variant: 0, source_job_id: 'r', created_at: '2026-08-08T11:00:00Z' }),
      prev
    );
    expect(pin).toMatchObject({ kind: 'exact', takeNo: 1, basis: 'only-candidate' });
  });

  it('refuses to guess when a retake had two predecessor takes to choose between', () => {
    const prev = [
      T({ variant: 0, created_at: '2026-08-08T10:00:00Z' }),
      T({ variant: 1, created_at: '2026-08-08T11:00:00Z' })
    ];
    const pin = pinOf(
      T({ variant: 0, source_job_id: 'r', created_at: '2026-08-08T12:00:00Z' }),
      prev
    );
    expect(pin.kind).toBe('unrecorded');
  });

  it('reports none when nothing predates the take', () => {
    expect(pinOf(T({ variant: 0 }), []).kind).toBe('none');
  });
});

describe('continuityOf', () => {
  it('calls a clean full run ok, and never flags the first unit', () => {
    const rows = continuityOf(fullRun(4));
    expect(rows.map((r) => r.state)).toEqual(['first', 'ok', 'ok', 'ok']);
    expect(repairPlan(rows)).toEqual([]);
  });

  it('breaks the next unit when a shot moves to a take it was not rendered against', () => {
    const units = fullRun(4);
    // Shot 3 (index 2) gets a retake, and the retake becomes the one that plays.
    units[2].takes = [
      T({ variant: 0, source_job_id: 'run-1', created_at: '2026-08-08T10:00:00Z' }),
      T({
        variant: 1,
        selected: true,
        source_job_id: 'retake-1',
        created_at: '2026-08-08T13:00:00Z'
      })
    ];
    const rows = continuityOf(units);
    expect(rows[2].state).toBe('ok'); // its OWN join to shot 2 is untouched
    expect(rows[3].state).toBe('broken');
    expect(rows[3].reason).toContain('was rendered from take 1');
    expect(rows[3].reason).toContain('take 2 now plays');
  });

  it('carries the cascade forward, because repairing a break re-renders the tail it hands on', () => {
    const units = fullRun(6);
    units[1].takes = [
      T({ variant: 0, source_job_id: 'run-1', created_at: '2026-08-08T10:00:00Z' }),
      T({ variant: 1, selected: true, source_job_id: 'r', created_at: '2026-08-08T13:00:00Z' })
    ];
    const rows = continuityOf(units);
    expect(rows.map((r) => r.state)).toEqual([
      'first',
      'ok',
      'broken',
      'downstream',
      'downstream',
      'downstream'
    ]);
    expect(repairPlan(rows)).toEqual([2, 3, 4, 5]);
  });

  it('stops the cascade at a hard cut — it carries nothing over, so nothing reaches past it', () => {
    const units = fullRun(5);
    units[1].takes = [
      T({ variant: 0, source_job_id: 'run-1', created_at: '2026-08-08T10:00:00Z' }),
      T({ variant: 1, selected: true, source_job_id: 'r', created_at: '2026-08-08T13:00:00Z' })
    ];
    units[3].join = { carries: false };
    const rows = continuityOf(units);
    expect(rows.map((r) => r.state)).toEqual(['first', 'ok', 'broken', 'cut', 'ok']);
    expect(repairPlan(rows)).toEqual([2]);
  });

  it('never flags a hard cut itself, whatever is selected upstream', () => {
    const units = fullRun(3);
    units[1].join = { carries: false };
    units[0].takes = [
      T({ variant: 0, source_job_id: 'run-1' }),
      T({ variant: 1, selected: true, source_job_id: 'r', created_at: '2026-08-08T13:00:00Z' })
    ];
    units[0].takes[0].selected = false;
    expect(continuityOf(units)[1].state).toBe('cut');
  });

  it('leaves an explicitly pinned join alone and stops the cascade there', () => {
    const units = fullRun(4);
    units[1].takes = [
      T({ variant: 0, source_job_id: 'run-1', created_at: '2026-08-08T10:00:00Z' }),
      T({ variant: 1, selected: true, source_job_id: 'r', created_at: '2026-08-08T13:00:00Z' })
    ];
    units[3].join = { carries: true, explicitFrom: 'engine-9:3' };
    const rows = continuityOf(units);
    expect(rows[2].state).toBe('broken');
    expect(rows[3].state).toBe('pinned');
    expect(repairPlan(rows)).toEqual([2]);
  });

  it('claims nothing when the pin was never recorded', () => {
    const units = fullRun(3);
    units[0].takes = [
      T({ variant: 0, selected: true, source_job_id: 'run-1', created_at: '2026-08-08T10:00:00Z' }),
      T({ variant: 1, source_job_id: 'r1', created_at: '2026-08-08T11:00:00Z' })
    ];
    // A retake of unit 1 made AFTER unit 0 already had two takes: unknowable, so not a warning.
    units[1].takes = [
      T({ variant: 1, selected: true, source_job_id: 'r2', created_at: '2026-08-08T12:00:00Z' })
    ];
    const rows = continuityOf(units);
    expect(rows[1].state).toBe('unknown');
    expect(repairPlan(rows)).toEqual([]);
  });

  it('says unrendered rather than inventing a join for a unit with no takes', () => {
    const units = fullRun(3);
    units[1].takes = [];
    expect(continuityOf(units)[1].state).toBe('unrendered');
  });

  it('judges the take that PLAYS, not the newest row', () => {
    const units = fullRun(3);
    units[1].takes = [
      T({ variant: 0, selected: true, source_job_id: 'run-1', created_at: '2026-08-08T10:00:00Z' }),
      // A newer take exists but is not selected — it is not on screen and must not be judged.
      T({ variant: 1, source_job_id: 'r', created_at: '2026-08-08T13:00:00Z' })
    ];
    expect(continuityOf(units)[1].state).toBe('ok');
  });

  it('labels units through the caller, so LTX shot numbers survive', () => {
    const units = fullRun(3);
    units[1].takes = [
      T({ variant: 0, source_job_id: 'run-1', created_at: '2026-08-08T10:00:00Z' }),
      T({ variant: 1, selected: true, source_job_id: 'r', created_at: '2026-08-08T13:00:00Z' })
    ];
    const rows = continuityOf(units, { labelOf: (i) => `shot ${i + 10}` });
    expect(rows[2].reason).toContain('shot 12');
    expect(rows[2].reason).toContain('shot 11');
  });

  it('reports the break but skips the cascade where a retake end-pins to the tail (LTX)', () => {
    const units = fullRun(5);
    units[1].takes = [
      T({ variant: 0, source_job_id: 'run-1', created_at: '2026-08-08T10:00:00Z' }),
      T({ variant: 1, selected: true, source_job_id: 'r', created_at: '2026-08-08T13:00:00Z' })
    ];
    const rows = continuityOf(units, { retakeSplicesTail: true });
    expect(rows.map((r) => r.state)).toEqual(['first', 'ok', 'broken', 'ok', 'ok']);
    expect(rows[2].reason).toContain('end-pins its final frame');
    expect(repairPlan(rows)).toEqual([2]);
  });
});

describe('pendingPinNote', () => {
  it('names the selected take and its bank key', () => {
    const note = pendingPinNote(
      [T({ variant: 0 }), T({ variant: 1, selected: true, bank_key: 'eng-4:2' })],
      'shot 3'
    );
    expect(note).toContain('take 2 of 2 of shot 3');
    expect(note).toContain('eng-4:2');
  });

  it('says so when there is nothing banked to join', () => {
    expect(pendingPinNote([], 'shot 3')).toContain('no banked tail');
  });
});
