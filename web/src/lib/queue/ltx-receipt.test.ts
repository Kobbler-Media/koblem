import { describe, expect, it } from 'vitest';
import { ltxReceipt } from './ltx-receipt';
import { enqueueReceipt } from './options';

describe('ltxReceipt', () => {
  it('reads a sync dispatch as started and everything else as queued', () => {
    expect(enqueueReceipt(ltxReceipt('rendering')).text).toBe('started');
    expect(enqueueReceipt(ltxReceipt('queued')).text).toBe('queued');
    expect(enqueueReceipt(ltxReceipt(undefined)).text).toBe('queued');
  });

  it('never claims a position the wire did not carry', () => {
    expect(ltxReceipt('queued').position).toBeUndefined();
    expect(enqueueReceipt(ltxReceipt('queued')).attention).toBe(false);
  });
});
