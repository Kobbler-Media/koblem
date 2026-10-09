export const POLL_MS = 1200;
export const MAX_LOAD_FAILS = 20;
const MAX_BACKOFF_MS = 15_000;

export function retryDelay(fails: number): number {
  return Math.min(POLL_MS * 2 ** Math.max(0, fails - 1), MAX_BACKOFF_MS);
}

export function outOfRetries(fails: number): boolean {
  return fails >= MAX_LOAD_FAILS;
}
