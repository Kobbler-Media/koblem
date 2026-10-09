/**
 * Which of the two render tiers a client uses — for the renders it starts AND for the clips it
 * streams back.
 *
 * A `final` shot is 1.4–19.5 MB and plays through an auth-gated route; the `proxy` tier's 512×288
 * pass is a fraction of that. So the tier is a property of the CLIENT, not the project: the same
 * project is reviewed at proxy on a phone and finished at final on a desktop, and neither choice
 * belongs in the saved document.
 */

export type RenderTier = 'proxy' | 'final';
/** What the user picked. `auto` is the only value that reads the screen. */
export type TierPreference = 'auto' | RenderTier;

/** The stored preference, or `auto` for anything unrecognised. */
export function parseTierPreference(raw: string | null | undefined): TierPreference {
  return raw === 'proxy' || raw === 'final' ? raw : 'auto';
}

export function resolveTier(pref: TierPreference, narrow: boolean): RenderTier {
  if (pref !== 'auto') return pref;
  return narrow ? 'proxy' : 'final';
}

/** The clip this client would rather play, falling back to the other tier over showing nothing. */
export function preferTier<T>(tier: RenderTier, proxy: T | null, final: T | null): T | null {
  return tier === 'proxy' ? (proxy ?? final) : (final ?? proxy);
}
