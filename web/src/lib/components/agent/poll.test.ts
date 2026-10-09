import { describe, expect, it } from 'vitest';
import { MAX_LOAD_FAILS, POLL_MS, outOfRetries, retryDelay } from './poll';

describe('retryDelay', () => {
  it('retries the first failure at the normal poll cadence', () => {
    expect(retryDelay(1)).toBe(POLL_MS);
  });

  it('backs off so a longer outage does not hammer the API', () => {
    expect(retryDelay(2)).toBeGreaterThan(retryDelay(1));
    expect(retryDelay(5)).toBeGreaterThan(retryDelay(4));
  });

  it('caps the backoff so a recovered API is picked up promptly', () => {
    expect(retryDelay(MAX_LOAD_FAILS)).toBe(retryDelay(MAX_LOAD_FAILS + 50));
    expect(retryDelay(MAX_LOAD_FAILS)).toBeLessThanOrEqual(15_000);
  });

  it('never returns a zero or negative delay for a bogus count', () => {
    expect(retryDelay(0)).toBe(POLL_MS);
    expect(retryDelay(-3)).toBe(POLL_MS);
  });
});

describe('outOfRetries', () => {
  it('keeps retrying right up to the limit', () => {
    expect(outOfRetries(1)).toBe(false);
    expect(outOfRetries(MAX_LOAD_FAILS - 1)).toBe(false);
  });

  it('gives up once the limit is reached', () => {
    expect(outOfRetries(MAX_LOAD_FAILS)).toBe(true);
    expect(outOfRetries(MAX_LOAD_FAILS + 1)).toBe(true);
  });
});

describe('the retry budget as a whole', () => {
  it('spans long enough to ride out an API restart rather than latching', () => {
    let total = 0;
    for (let f = 1; !outOfRetries(f); f += 1) total += retryDelay(f);
    expect(total).toBeGreaterThan(120_000);
  });
});
