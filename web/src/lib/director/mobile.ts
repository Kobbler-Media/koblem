/**
 * Pure rules for the phone shell — the layout that replaces the whole desktop workspace below
 * `DIRECTOR_COMPACT`, rather than squeezing it.
 *
 * Nothing here holds project state. The shell and its screens are presentation: `DirectorWorkspace`
 * still owns every `$state` and hands them down, so there is exactly one copy of the document.
 */

import type { Snippet } from 'svelte';
import type { DirectorH3Reference, H3ReferenceKind } from '#lib/api/client.ts';
import { h3Ordinals } from './h3-prompt';
import { roleLabel } from './h3-roles';

export type MobileTab = 'shots' | 'prompt' | 'render' | 'more';

/**
 * Which sub-view of the Prompt screen is showing.
 *
 * 🔴 Held by `DirectorWorkspace` alongside the rest of the navigation, not by `MobilePrompt`. The
 * shell's body is an `{#if sheet}…{:else if tab}` ladder, so pushing a sheet DESTROYS the prompt
 * screen — and view state living in the component would come back as a fresh 'prompt' every time
 * somebody opened Rework, Takes or the block editor and pressed back.
 */
export type MobilePromptTab = 'prompt' | 'sections' | 'beats' | 'details';

export const MOBILE_TABS: { id: MobileTab; glyph: string; label: string }[] = [
  { id: 'shots', glyph: '▤', label: 'Shots' },
  { id: 'prompt', glyph: '✎', label: 'Prompt' },
  { id: 'render', glyph: '▶', label: 'Render' },
  { id: 'more', glyph: '⋯', label: 'More' }
];

/**
 * A full-screen screen that is not a tab — pushed over the body with a real back button in the app
 * bar, so it is somewhere you LEAVE rather than a modal that has to be dismissed.
 *
 * `bare` hands the sheet the whole body with no padding and no page scroll, which is what the
 * block editor needs: the document scrolls inside it so its dock can stay put over the keyboard.
 */
export interface MobileSheetDef {
  id: string;
  title: string;
  subtitle?: string;
  body: Snippet;
  bare?: boolean;
}

/**
 * One reference as every mobile surface sees it — the strip, the ordinals in the prompt, the
 * editor's docked context bar and the card all read this and nothing else.
 *
 * 🔴 One row per ORDINAL, not per reference: a soundtracked video carries `<Video 2>` and its own
 * `<Audio 1>`, and the prompt addresses those separately. A strip that showed one tile for two
 * numbers would be a map with a missing street.
 */
export interface MobileRef {
  id: string;
  kind: H3ReferenceKind;
  /** `<Picture 1>` — exactly what the prompt writes. */
  label: string;
  /** `P1` — the corner badge, which has room for two characters and no more. */
  badge: string;
  name: string;
  role: string;
  /** Already through `assetSrc`. Empty where there is nothing to show. */
  image: string;
  /** Confined to some render units rather than the whole project (`addScopedReference`). */
  scoped: boolean;
  /** This row is the video's own soundtrack rather than the video. */
  ownAudio: boolean;
}

const BADGE_INITIAL: Record<string, string> = {
  Picture: 'P',
  Video: 'V',
  Audio: 'A',
  Subject: 'S'
};

/** `<Picture 1>` → `P1`. Anything unrecognised comes back verbatim rather than mangled. */
export function mobileOrdinalBadge(label: string): string {
  const m = /^<([A-Za-z]+) (\d+)>$/.exec(label.trim());
  return m ? `${BADGE_INITIAL[m[1]] ?? m[1].charAt(0)}${m[2]}` : label;
}

/**
 * The reference rows for one render unit, in the order that assigns their ordinals.
 *
 * `src` is injected rather than imported so this module stays free of the asset cache — the caller
 * has already been through `assetSrc`, which is a rune store and belongs in a component's `$derived`.
 */
export function mobileRefRows(
  refs: DirectorH3Reference[],
  src: (value: string | null | undefined) => string
): MobileRef[] {
  const ordinals = h3Ordinals(refs);
  const out: MobileRef[] = [];
  for (const r of refs) {
    const o = ordinals.find((x) => x.id === r.id);
    if (!o) continue;
    const name = r.label?.trim() || `unnamed ${r.kind}`;
    const scoped = Array.isArray(r.segments) && r.segments.length > 0;
    const image = r.kind === 'image' ? src(r.image) : r.kind === 'video' ? src(r.frames?.[0]) : '';
    // Audio before the video it belongs to, mirroring the order `h3Ordinals` numbers them in.
    if (o.audioLabel)
      out.push({
        id: `${r.id}:audio`,
        kind: 'audio',
        label: o.audioLabel,
        badge: mobileOrdinalBadge(o.audioLabel),
        name: `${name} · its own track`,
        role: roleLabel(r.role),
        image: '',
        scoped,
        ownAudio: true
      });
    out.push({
      id: r.id,
      kind: r.kind,
      label: o.label,
      badge: mobileOrdinalBadge(o.label),
      name,
      role: roleLabel(r.role),
      image,
      scoped,
      ownAudio: false
    });
  }
  return out;
}

/** `▣ ▶ ♫` — the tile glyph for a reference kind. One map, two strips and a card. */
export const MOBILE_REF_GLYPH: Record<H3ReferenceKind, string> = {
  image: '▣',
  video: '▶',
  audio: '♫'
};

/**
 * Why an `<Picture 3>` in the prompt resolves to nothing, in one sentence.
 *
 * 🔴 The same fact was phrased three ways — an editor hover card, its "only P1, P2 exist here"
 * variant, and the reference card's warning — so what the model would do with a dead ordinal
 * depended on which surface the author happened to tap. It reads them as ordinary words wherever
 * they appear.
 *
 * `badges` is what IS in scope, in ordinal order; empty means nothing is.
 */
export function deadOrdinalNote(label: string, badges: readonly string[]): string {
  const has = badges.length
    ? `only ${badges.join(', ')} ${badges.length === 1 ? 'exists' : 'exist'} here`
    : 'no references are in scope';
  return `${label} names nothing — ${has}, so the model reads it as ordinary words.`;
}

/**
 * The smallest gap two beats may be apart, in seconds.
 *
 * A block writes its times as `MM:SS.mmm`, so a millisecond is the finest distinction that survives
 * the round trip; 50 ms is the smallest one a reader can act on.
 */
export const MOBILE_BEAT_GAP = 0.05;

/**
 * Where an added beat lands, or `null` when this render unit has no room for another one.
 *
 * 🔴 `h3-prompt`'s validator refuses a block whose cut times do not STRICTLY increase, and refuses
 * one whose last cut is at or past the unit's own length. Both are checked at render, which is far
 * too late for a tap: repeated adds on a short unit used to pile up on the ceiling and mint a time
 * equal to its predecessor, so the phone produced a block it would later refuse. Computed in
 * integer milliseconds because that is the resolution the block is written at — rounding a
 * seconds-space result is what would put two beats back on the same millisecond.
 */
export function nextBeatSeconds(
  existing: readonly number[],
  unitSeconds: number,
  gap: number = MOBILE_BEAT_GAP
): number | null {
  const ms = (n: number) => Math.round(n * 1000);
  const gapMs = Math.max(1, ms(gap));
  // The MAX rather than the last: a hand-typed "lands at" can leave the list out of order, and the
  // new beat has to clear every one of them to be strictly last.
  const lastMs = existing.length ? Math.max(0, ...existing.map(ms)) : 0;
  const ceilingMs = ms(unitSeconds) - gapMs;
  const floorMs = lastMs + gapMs;
  if (ceilingMs < floorMs) return null;
  // Spread the beats over what is left, which is what makes an empty unit fill evenly rather than
  // stacking every new beat against the one before it.
  const spreadMs = Math.max(gapMs, ms(Math.max(1, unitSeconds / Math.max(2, existing.length + 1))));
  return Math.min(ceilingMs, Math.max(floorMs, lastMs + spreadMs)) / 1000;
}

/**
 * `[2, 7, 9]` → `"2, 7 and 9"`; `[1, 3, 4, 5, 6]` → `"1, 3–6"`.
 *
 * The render button on a phone has no tooltip and no confirm dialog to explain its scope in, so the
 * scope is printed above it as prose — and a twelve-shot cut has to fit on one line to be read.
 */
export function formatShotRanges(nos: number[]): string {
  const sorted = [...new Set(nos)].sort((a, b) => a - b);
  if (sorted.length === 0) return '';
  const runs: string[] = [];
  let start = sorted[0];
  let prev = sorted[0];
  const flush = () => runs.push(start === prev ? `${start}` : `${start}–${prev}`);
  for (const n of sorted.slice(1)) {
    if (n === prev + 1) prev = n;
    else {
      flush();
      start = n;
      prev = n;
    }
  }
  flush();
  if (runs.length === 1) return runs[0];
  return `${runs.slice(0, -1).join(', ')} and ${runs[runs.length - 1]}`;
}
