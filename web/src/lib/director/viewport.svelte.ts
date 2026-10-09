/**
 * The width below which the Director drops its desktop layout entirely. This is --bp-tablet
 * (documented in app.css); `@media` cannot read a custom property, so the number is written out
 * here as well as there.
 *
 * 🔴 It is not a chrome tweak. Below it the phone shell REPLACES the app bar, the two-zone work
 * grid and the timeline wholesale — so nothing the desktop shell renders exists at this width, and
 * a `@media (max-width: 768px)` rule aimed at one of its components styles nothing at all.
 */
export const DIRECTOR_COMPACT = '(max-width: 768px)';

/**
 * Reactive `matchMedia`, answered from the first read rather than from the first effect.
 *
 * 🔴 The initial value is the REAL one. It used to start `false` "so a server render sees the
 * desktop layout" — but this build is a pure SPA (`adapter-static` with `fallback: 'index.html'`,
 * nothing prerendered and no `+page.ts`), so there is no server render to agree with and nothing
 * to hydrate against. What the `false` actually bought was every phone load mounting the app bar,
 * the two-zone work grid and the timeline — none of which exist below {@link DIRECTOR_COMPACT} —
 * and destroying them one microtask later.
 *
 * The `typeof window` guard is for a non-browser import (a unit test, a future SSR pass); there the
 * effect does not run either, and `false` is the desktop layout, which is the safe one to guess.
 */
export function mediaQuery(query: string): { readonly current: boolean } {
  let current = $state(typeof window !== 'undefined' && window.matchMedia(query).matches);
  $effect(() => {
    const mql = window.matchMedia(query);
    current = mql.matches;
    const onChange = (e: MediaQueryListEvent) => (current = e.matches);
    mql.addEventListener('change', onChange);
    return () => mql.removeEventListener('change', onChange);
  });
  return {
    get current() {
      return current;
    }
  };
}
