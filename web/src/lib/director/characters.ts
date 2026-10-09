import { uid } from '@koblibs/ui';
import {
  DIRECTOR_FIRST_SOURCE_ID,
  LTX_LORA_FACEID_SHEET,
  LTX_LORA_MSR_V2,
  LTX_REFERENCE_SPEC,
  ltxIdentityRegime,
  ltxModelIsIdentityTrained,
  type DirectorCharacter,
  type DirectorClip,
  type DirectorReferenceRole,
  type LtxCastMember,
  type LtxCharacterRef,
  type LtxIdentityRegime,
  type LtxRuntimeLora
} from '$lib/api/client';
import { effectiveClipLoras, hasLora, shortLoraName } from '$lib/director/loras';

/**
 * The project cast and the identity tags they own — the piece that makes character references work
 * across a scene rather than inside one shot.
 *
 * LTX conditions identity on a per-reference rotary phase keyed by `source_id`. The tag IS the
 * identity as far as the model is concerned, so the same character has to present the same number
 * in every shot it appears in. Allocating tags per shot ("this shot's first reference is 2") looks
 * equivalent and is not: the moment two shots have different casts, character A is 2 in one shot
 * and 3 in the next, and the model is being told they are different people.
 *
 * So tags are allocated ONCE, here, at project level, and persisted on the character. The workspace
 * and the compiler share this module so they cannot drift apart on the one field that silently
 * breaks the feature.
 *
 * The tag is only *read* in the `sheet` regime (the Best-Face-ID adapter) — Echo and MSR run
 * untagged — but it is allocated and kept stable either way, so changing the model or swapping the
 * identity adapter never renumbers the cast.
 */

/** Copy for the UI. Multi-character is real but untrained — say so rather than sell it. */
export const MULTI_CHARACTER_CAVEAT =
  'One character per shot is the trained case. A second is architecturally supported (each gets ' +
  'its own identity tag) but was never trained — the upstream training was single-subject and a ' +
  'sheet depicts one person — so expect the two to bleed into each other.';

/**
 * What a reference has to be for this render to get what it expects.
 *
 * Takes the adapter set, not just the model: the sheet regime is reachable ONLY through the
 * Best-Face-ID adapter now, so `base` alone and `base` + that adapter want different images.
 */
export function referenceSpec(
  model: string | undefined,
  loras: LtxRuntimeLora[] | undefined = undefined
): string {
  return LTX_REFERENCE_SPEC[ltxIdentityRegime(model, loras)];
}

export function frameDropAdvice(): string {
  return (
    'Nothing on this engine leaks a reference into frame 0, so there is nothing here that needs ' +
    'trimming. Auto is still safe: the engine checks the shot before it cuts anything.'
  );
}

/**
 * The next free identity tag. Ascending from 2 (0 is the render target itself, phase 0, an exact
 * no-op), and always ABOVE every tag ever issued in this project rather than "first gap": a reused
 * tag re-labels a face on every shot that still references it, whereas a gap costs nothing.
 */
export function nextSourceId(characters: DirectorCharacter[]): number {
  return characters.reduce(
    (max, c) => Math.max(max, (c.source_id ?? 0) + 1),
    DIRECTOR_FIRST_SOURCE_ID
  );
}

/** A new cast member holding the next free tag. Add via this, never by hand. */
export function newCharacter(
  characters: DirectorCharacter[],
  name = '',
  role: DirectorReferenceRole = 'character'
): DirectorCharacter {
  return {
    id: uid(),
    name,
    sheet_b64: null,
    source_id: nextSourceId(characters),
    identity: '',
    role
  };
}

/** What a reference depicts. Absent means `character` — everything predating the field was one. */
export function characterRole(c: Pick<DirectorCharacter, 'role'>): DirectorReferenceRole {
  return c.role ?? 'character';
}

/** Every role, in the order the picker offers them. */
export const REFERENCE_ROLES: readonly DirectorReferenceRole[] = ['character', 'location', 'prop'];

/** Editor copy for each role, kept next to the rule the role feeds. */
export const REFERENCE_ROLE_LABEL: Record<DirectorReferenceRole, string> = {
  character: 'Character',
  location: 'Location',
  prop: 'Prop / object'
};

export const REFERENCE_ROLE_HINT: Record<DirectorReferenceRole, string> = {
  character: 'A person. Both memory-regime mechanisms hold these equally well.',
  location:
    'A place or set. Both memory-regime mechanisms hold these equally well — measured identical to characters.',
  prop:
    'A thing: a vehicle, an instrument, an object the character handles. This is the one role the ' +
    'two mechanisms DISAGREE on — Echo fuses a prop into whoever is holding it (a rider welded ' +
    'into a bike, a drone absorbed into its frame), MSR keeps them separate. A shot carrying one ' +
    'is flagged as wanting the MSR LoRA; add it yourself under Runtime LoRAs.'
};

/**
 * Repair a loaded roster in place-ish (returns a new array): fill in ids and any missing/duplicate
 * `source_id`. A duplicate is the one corruption that must never survive a load — two characters
 * sharing a tag is exactly the failure the whole design exists to prevent — so the later one is
 * pushed to a fresh tag rather than left alone.
 */
export function normaliseCharacters(
  characters: DirectorCharacter[] | undefined
): DirectorCharacter[] {
  const out: DirectorCharacter[] = [];
  for (const c of characters ?? []) {
    const fixed: DirectorCharacter = { ...c, id: c.id || uid() };
    const clash =
      !Number.isFinite(fixed.source_id) ||
      fixed.source_id < DIRECTOR_FIRST_SOURCE_ID ||
      out.some((o) => o.source_id === fixed.source_id);
    if (clash) fixed.source_id = nextSourceId(out);
    out.push(fixed);
  }
  return out;
}

/**
 * The cast IN one shot, in roster order (so tags come out ascending).
 *
 * ## `character_ids` absent means EVERY character, not none
 *
 * The cast is a per-shot ENABLE list with an all-on default, and the three states are distinct:
 *
 * | `character_ids` | meaning |
 * |---|---|
 * | `undefined` | every character in the project (the default a new shot gets) |
 * | `[]`        | explicitly none — the user turned them all off |
 * | `[a, c]`    | exactly those |
 *
 * Absent → all is also what the engine already does today: `character_refs` is one chain-level array
 * applied to EVERY segment, so before per-shot scoping existed, every shot was already conditioned
 * on every reference. Defaulting to all-on therefore describes what actually renders, and the toggle
 * is what's new — a shot that reads `undefined` is not a shot someone deselected.
 *
 * The editor materialises the list on first toggle, so anything explicit stays explicit.
 */
export function clipCharacters(
  clip: Pick<DirectorClip, 'character_ids'>,
  characters: DirectorCharacter[]
): DirectorCharacter[] {
  const ids = clip.character_ids;
  if (ids === undefined) return characters;
  if (ids.length === 0) return [];
  return characters.filter((c) => ids.includes(c.id));
}

/** True when `id` is enabled on this shot. The all-on default applies. */
export function clipHasCharacter(clip: Pick<DirectorClip, 'character_ids'>, id: string): boolean {
  return clip.character_ids === undefined || clip.character_ids.includes(id);
}

/**
 * Flip one character on/off for one shot, materialising the implicit all-on default first.
 *
 * Always writes an ARRAY, never back to `undefined`: `[]` is "the user turned everything off" and
 * must not decay into `undefined`, which reads as all-on. Shared by both Director UIs so the two
 * cannot disagree about what an empty selection means.
 */
export function toggleClipCharacter(
  clip: DirectorClip,
  characters: DirectorCharacter[],
  id: string
): void {
  const current = clipCharacters(clip, characters).map((c) => c.id);
  clip.character_ids = current.includes(id) ? current.filter((x) => x !== id) : [...current, id];
}

/** Put a shot back on the all-on default (and drop the stored list). */
export function resetClipCast(clip: DirectorClip): void {
  clip.character_ids = undefined;
}

/**
 * A shot's cast selection as an undo/dirty signature. `*` for the implicit default, so materialising
 * "all" into an explicit list is still seen as a change and isn't coalesced away.
 */
export function clipCastSignature(clip: Pick<DirectorClip, 'character_ids'>): string {
  return clip.character_ids === undefined ? '*' : clip.character_ids.join('+');
}

/**
 * Does this shot OPEN a scene, i.e. is its first frame its own rather than the previous shot's?
 *
 * 🔴 MUST MIRROR `shot_scene` IN `routes/ltx_video.rs`, INCLUDING ITS PRECEDENCE. That function
 * decides whether the engine trims a seam; this one decides whether a reference is needed to hold
 * identity. They are asking the same question — "does anything carry over into this shot" — and if
 * they disagree, a shot is conditioned as a continuation and scoped as a fresh shot or the reverse.
 * The engine's order: shot 0, then v2v, then keyframes (which suppress both scene-cut forms), then
 * a per-shot scene image, then the explicit flag.
 */
export function opensScene(
  clip: Pick<DirectorClip, 'mode' | 'image_b64' | 'keyframes' | 'new_scene'>,
  index: number
): boolean {
  if (index === 0) return true;
  // By MODE, not by `v2v_source_kind`: the source-kind field is absent on a v2v clip that has not
  // picked one yet (it defaults to `upload`), and such a clip is still a v2v shot.
  if (clip.mode === 'v2v_restyle' || clip.mode === 'v2v_edit') return true;
  const pins = clip.keyframes ?? [];
  // A keyframe shot is fresh only if it pins frame 0; a mid/late-only pin CONTINUES prior motion.
  if (pins.length) return pins.some((p) => p.seconds <= 0.001);
  if (clip.mode === 'i2v' && clip.image_b64) return true;
  return clip.new_scene === true;
}

/**
 * For each shot, the character ids whose identity its CARRY already supplies.
 *
 * A plain continuation inherits the previous shot's last `8K + 1` frames as frozen conditioning, so
 * anyone who was in that shot arrives with it. A shot that opens a scene inherits nothing.
 *
 * ⚠️ THIS IS A DEFAULT, NOT A TRUTH. It cannot know that the camera tilted off the subject and
 * ended the shot on the sky — measured on 2026-08-15, that case dissolves the character when the
 * reference is withheld. `reference_mode: 'always'` is the override, and the UI has to make it
 * reachable or the failure looks like the model drifting.
 */
export function carriedCharacterIds(
  clips: DirectorClip[],
  characters: DirectorCharacter[]
): Set<string>[] {
  const out: Set<string>[] = [];
  let previous = new Set<string>();
  clips.forEach((clip, i) => {
    out.push(opensScene(clip, i) ? new Set<string>() : previous);
    previous = new Set(clipCharacters(clip, characters).map((c) => c.id));
  });
  return out;
}

/**
 * Why a character enabled on this shot still contributes no reference to it, or `null` when it does.
 * Surfaced in the UI so an excluded character reads as excluded-with-a-reason rather than as a
 * toggle that silently did nothing.
 *
 * `carried` is the set from {@link carriedCharacterIds} for THIS shot. Omitting it keeps the old
 * behaviour exactly — every enabled character with a sheet sends a reference — so a caller that has
 * no clip list (a details pane rendering one shot) is not forced to invent one.
 */
export function refExclusionReason(
  clip: Pick<DirectorClip, 'id' | 'reference_mode'>,
  character: DirectorCharacter,
  carried?: Set<string>
): string | null {
  if (clip.reference_mode === 'never') {
    return 'This shot is set to send no character references.';
  }
  if (character.introduced_by === clip.id) {
    return 'This shot introduced them — the reference was taken from its own footage, so conditioning it on that frame would be circular. Later shots hold it.';
  }
  if (!character.sheet_b64) {
    return 'No reference image yet. Render the shot that introduces them, then promote a frame of it under Takes.';
  }
  // 🔴 OPT-IN, so an absent `reference_mode` behaves exactly as it did before this existed.
  // Re-sending a reference the carry already supplies restores the reference's own state — a robot
  // asked to stay headless got its head back — and doubles the motion step at the cut (15.20
  // against 6.83). But a shot that does NOT end on a well-framed subject has no identity in its
  // carry at all, and withholding the reference there loses the character outright. Only `auto`
  // takes that trade.
  if (clip.reference_mode === 'auto' && carried?.has(character.id)) {
    return 'The previous shot carries them into this one, so the join already holds the identity — re-sending the sheet here re-imposes the reference’s own state. Set this shot to “always” if the camera left them, or if they changed and need re-establishing.';
  }
  return null;
}

/** How many references this shot will actually send. */
export function clipRefCount(
  clip: Pick<DirectorClip, 'id' | 'character_ids' | 'reference_mode'>,
  characters: DirectorCharacter[],
  carried?: Set<string>
): number {
  return clipCharacters(clip, characters).filter(
    (c) => refExclusionReason(clip, c, carried) === null
  ).length;
}

/**
 * A shot's compiled `character_refs`, or `undefined` when it has none (so the compiled request stays
 * byte-identical for every existing project).
 *
 * Two characters are dropped here rather than sent:
 *
 *  - one with no reference image yet — it steers nothing, and an empty `image` is the single input
 *    the engine answers with a body-less 400;
 *  - one whose reference was PROMOTED FROM THIS SHOT (`introduced_by`). Conditioning a shot on a
 *    still taken out of its own render is circular: at best a no-op, at worst the shot fights a
 *    frame of itself. The introduction shot is the one that defines the look; every LATER shot is
 *    what the reference is for.
 *
 * `resize_mode` is deliberately NOT set: the backend fills it from the selected model's regime
 * (`native_resolution` for a sheet, `match_target` for an Echo memory slot). Pinning it here would
 * mean the editor had to know the model, and getting it wrong is invisible in a successful render.
 */
export function characterRefsForClip(
  clip: Pick<DirectorClip, 'id' | 'character_ids' | 'reference_mode'>,
  characters: DirectorCharacter[],
  carried?: Set<string>
): LtxCharacterRef[] | undefined {
  const refs = clipCharacters(clip, characters)
    .filter((c) => refExclusionReason(clip, c, carried) === null)
    .map((c): LtxCharacterRef => ({
      image_b64: c.sheet_b64!,
      source_id: c.source_id,
      name: c.name || undefined,
      identity: c.identity?.trim() || undefined
    }));
  return refs.length ? refs : undefined;
}

/** What one shot resolves to, plus any advice about it. */
export interface ResolvedClipRouting {
  /** The DiT variant, present only when the shot PINS one. Absent ⇒ the project model stands. */
  model?: string;
  /**
   * An adapter this shot would BENEFIT from, as advice for the editor to show. **Never sent** — it
   * carries no strength and no ordering, and the compilers deliberately do not read it.
   *
   * 🔴 It is advice and not an action on purpose. See the note on {@link resolveClipVariants}.
   */
  suggestedLora?: string;
  /** True when this entry is automatic ADVICE; false when the author pinned the shot themselves. */
  automatic: boolean;
  reason: string;
}

/**
 * How each shot should actually render, given what its references DEPICT.
 *
 * ## Why this is per-shot and not per-project
 *
 * Echo and MSR take the SAME references through the SAME request shape (`character_refs`,
 * `tass_phase_scale: 0`) and are measured identical on characters and locations. They differ on
 * exactly one thing: only MSR composes a PROP as a separate entity — Echo fuses multiple
 * heterogeneous subjects into one. So a project whose shot 2 hands someone a guitar does not need to
 * change anything about its OTHER shots; it needs MSR on shot 2 and can leave the rest alone.
 *
 * Because the assets are interchangeable, that costs nothing to set up: the same cast serves both.
 *
 * ## 🔴 [2026-07-30] THIS ADVISES. IT NO LONGER CHANGES A RENDER. DO NOT RE-AUTOMATE IT.
 *
 * Two behaviours have already been removed here, in this order:
 *
 * 1. It moved a prop shot onto the `msr-v2` DiT. That checkpoint is deleted (MSR is a runtime LoRA
 *    now), so the routing named a variant the engine rejects with `unknown model variant`.
 * 2. It was then changed to attach {@link LTX_LORA_MSR_V2} on top of the project model — which is
 *    coherent, cheap and preserves the regime, but **stacks an adapter onto Echo weights, and that
 *    combination has never been rendered.** MSR-as-runtime-LoRA is proven on `base` (480/480
 *    modules merged, and it passes the behavioural assay against a leaking negative control); Echo
 *    + MSR is proven nowhere. Silently opting every prop-carrying shot into an unvalidated stack
 *    is not a default, and identity failures here are invisible in a render that succeeds.
 *
 * So a prop shot now gets a {@link ResolvedClipRouting.suggestedLora} and a reason, and the
 * compilers do not read either. The author adds MSR themselves — per project or per shot — through
 * the LoRA picker that already exists on every Director surface.
 *
 * **Re-enabling this needs evidence, not a one-line change:** render the MSR assay on an Echo dose
 * with MSR attached (frames 0-7 are binary — does the reference read as CONTEXT, or get COPIED into
 * the opening frames?), read the negative control FIRST, and confirm the Echo identity still holds
 * at the same time. Until then, advice only.
 *
 * ## The rules, in order
 *
 * 1. An explicit `clip.model` or `clip.loras` is reported, never overridden — the author asked for
 *    it. A fusing model on a prop shot is surfaced as a warning rather than corrected: Echo's fusion
 *    is sometimes what someone wants, and n=1 is not enough evidence to overrule an author.
 * 2. A project with no memory-regime identity conditioning is left alone: nothing here can help a
 *    shot with no trained response to references at all, and {@link identityAdvisory} says so.
 * 3. Otherwise a PROP in scope SUGGESTS MSR. Nothing is applied.
 *
 * Rule 3 deliberately says nothing about characters and locations: Echo and MSR are measured
 * identical there, so there would be nothing to suggest.
 */
export function resolveClipVariants(
  clips: Pick<DirectorClip, 'id' | 'character_ids' | 'model' | 'loras'>[],
  characters: DirectorCharacter[],
  projectModel: string | undefined,
  projectLoras: LtxRuntimeLora[] = []
): Map<string, ResolvedClipRouting> {
  const out = new Map<string, ResolvedClipRouting>();
  const project = projectModel || 'base';
  const projectIsMemory =
    ltxModelIsIdentityTrained(project, projectLoras) &&
    ltxIdentityRegime(project, projectLoras) === 'memory';
  const msr = shortLoraName(LTX_LORA_MSR_V2);
  for (const clip of clips) {
    const refs = clipCharacters(clip, characters).filter(
      (c) => refExclusionReason(clip, c) === null
    );
    const props = refs.filter((c) => characterRole(c) === 'prop');
    // An authored pin on EITHER axis is the author's, and is reported rather than adjusted. The
    // adapter set matters as much as the model here: a shot that names its own set replaces the
    // project's, so it can drop an inherited MSR without touching `model` at all.
    if (clip.model || clip.loras?.length) {
      const effective = clip.loras?.length ? clip.loras : projectLoras;
      const pinned = clip.model || project;
      const composes = hasLora(effective, LTX_LORA_MSR_V2) || pinned.toLowerCase().includes('msr');
      out.set(clip.id, {
        model: clip.model,
        automatic: false,
        reason:
          props.length && ltxIdentityRegime(pinned, effective) === 'memory' && !composes
            ? `Pinned on this shot, but it carries ${props.length} prop reference` +
              `${props.length === 1 ? '' : 's'} and nothing in scope composes them — ${pinned} ` +
              `fuses a prop into whoever holds it. Add ${msr} to this shot to keep them separate.`
            : `Pinned on this shot.`
      });
      continue;
    }
    if (!projectIsMemory || refs.length === 0) continue;
    if (props.length === 0) continue;
    // Already in scope ⇒ nothing to suggest, the shot already composes its props.
    if (hasLora(projectLoras, LTX_LORA_MSR_V2)) continue;
    out.set(clip.id, {
      // ADVICE ONLY. Not an adapter set, and not a strength — deliberately not something a compiler
      // could pick up and send. See the note above: Echo + MSR has never been rendered.
      suggestedLora: LTX_LORA_MSR_V2,
      automatic: true,
      reason:
        `Carries ${props.length} prop reference${props.length === 1 ? '' : 's'} ` +
        `(${props.map((p) => p.name || 'unnamed').join(', ')}). ${project} fuses a prop into ` +
        `whoever holds it; ${msr} keeps them separate. Add it under Runtime LoRAs — on this shot ` +
        `or the whole project. Not applied automatically: MSR stacked on ${project} has not been ` +
        `eye-tested yet.`
    });
  }
  return out;
}

/**
 * How many times the chain changes DiT variant, in shot order. Each one is a lease change on the
 * worker, so this is the number worth showing before someone renders.
 *
 * Counts MANUAL pins only now — the automatic prop routing switches adapters, not checkpoints, so on
 * a project with no per-shot model pins this is 0 where it used to be one switch per prop run.
 */
export function variantSwitchCount(
  clips: Pick<DirectorClip, 'id' | 'character_ids' | 'model' | 'loras'>[],
  characters: DirectorCharacter[],
  projectModel: string | undefined,
  projectLoras: LtxRuntimeLora[] = []
): number {
  const resolved = resolveClipVariants(clips, characters, projectModel, projectLoras);
  const project = projectModel || 'base';
  let switches = 0;
  let previous = project;
  for (const clip of clips) {
    const model = resolved.get(clip.id)?.model ?? project;
    if (model !== previous) switches += 1;
    previous = model;
  }
  return switches;
}

/**
 * How many times the chain changes ADAPTER SET, in shot order — the cost the prop routing actually
 * incurs. Each change re-folds the new set into the resident weights (~12-30s of GPU work depending
 * on module count); an unchanging set folds once and stays warm for the whole render.
 */
export function loraFoldCount(
  clips: Pick<DirectorClip, 'loras'>[],
  projectLoras: LtxRuntimeLora[] = []
): number {
  const key = (l: LtxRuntimeLora[]) =>
    l
      .map((x) => `${x.path}@${x.multiplier}`)
      .sort()
      .join('|');
  let folds = 0;
  let previous: string | null = null;
  for (const clip of clips) {
    // AUTHORED sets only — the prop routing suggests and does not apply, so it cannot add a fold.
    const set = key(effectiveClipLoras(clip, projectLoras));
    if (set !== previous) folds += 1;
    previous = set;
  }
  // An empty set on every shot folds nothing at all.
  return folds === 1 && previous === '' ? 0 : folds;
}

/**
 * Drop ids of characters that no longer exist, so a deleted cast member can't linger.
 *
 * A shot left with nothing keeps `[]`, and one on the implicit default keeps `undefined`: collapsing
 * an emptied list back to `undefined` would silently turn "none of them" into "all of them", which
 * is the exact inversion the three-state encoding exists to prevent.
 */
export function pruneClipCharacters(clip: DirectorClip, characters: DirectorCharacter[]): void {
  if (clip.character_ids === undefined) return;
  const live = new Set(characters.map((c) => c.id));
  clip.character_ids = clip.character_ids.filter((id) => live.has(id));
}

/**
 * Bring a newly-added character onto every shot, matching the all-on default.
 *
 * Only shots with an EXPLICIT list need touching — a shot still on `undefined` already means "all"
 * and picks the new character up on its own. Shots the user emptied deliberately (`[]`) are the
 * awkward case: they get the new character too, because "off" was a statement about the characters
 * that existed then, and a roster addition defaults on everywhere else.
 */
export function addCharacterToAllClips(clips: DirectorClip[], id: string): void {
  for (const clip of clips) {
    if (clip.character_ids === undefined) continue;
    if (!clip.character_ids.includes(id)) clip.character_ids.push(id);
  }
}

/**
 * The cast as the LLM prompt writers need it: a name to call each character by and the appearance
 * sentence they must reproduce character-for-character.
 *
 * Characters with neither are dropped — there is nothing for the writer to hold — so a project that
 * has not filled any of this in sends nothing and every draft stays byte-identical.
 */
export function castForPrompting(characters: DirectorCharacter[]): LtxCastMember[] {
  return characters
    .map((c) => ({
      name: c.name?.trim() ?? '',
      appearance: c.identity?.trim() ?? '',
      source_id: c.source_id
    }))
    .filter((c) => c.name || c.appearance);
}

/** The cast of ONE shot, for the per-shot draft path. */
export function castForClip(
  clip: Pick<DirectorClip, 'character_ids'>,
  characters: DirectorCharacter[]
): LtxCastMember[] {
  return castForPrompting(clipCharacters(clip, characters));
}

/**
 * A reference's share of the attention sequence — the number that decides whether identity
 * conditioning holds the FACE or captures the FRAMING.
 *
 * Mirrors `ltx_ref_token_share` in koblem-client, and reproduces both GPU-measured points: 39.5%
 * (768×448, 49 frames) CAPTURED the reference's framing; 11.8% (1280×704, 97 frames) was clean. The
 * lever is the ratio, not the resolution — a longer or bigger shot dilutes the same reference.
 *
 * Video tokens are `latentFrames × (w/32) × (h/32)`; a `match_target` reference costs exactly one
 * latent frame's worth, a native-resolution 1536×1024 sheet a fixed 1536.
 */
export function refTokenShare(
  width: number,
  height: number,
  frames: number,
  refs: number,
  regime: LtxIdentityRegime
): number {
  if (refs <= 0) return 0;
  const perFrame = Math.floor(width / 32) * Math.floor(height / 32);
  if (perFrame <= 0) return 0;
  const latentFrames = Math.floor((Math.max(1, frames) - 1) / 8) + 1;
  const perRef = regime === 'memory' ? perFrame : (1536 / 32) * (1024 / 32);
  const refTokens = perRef * refs;
  return refTokens / (refTokens + perFrame * latentFrames);
}

/** Above this share references start competing with the shot; unmeasured ground, so: warn. */
export const REF_SHARE_WARN = 0.2;
/** At/above this share the reference CAPTURES the framing. Measured at 39.5%; 11.8% was clean. */
export const REF_SHARE_CAPTURE = 0.35;

/**
 * The heaviest single shot's reference count — what the token-share advisory has to be computed
 * against now that shots can carry different casts.
 *
 * Not the project-wide union: cost is paid per SHOT (each segment's sequence carries only the
 * references scoped to it), so a project with six characters spread one-per-shot is cheap, and one
 * shot with four is the thing worth warning about.
 */
export function worstClipRefCount(
  clips: Pick<DirectorClip, 'id' | 'character_ids'>[],
  characters: DirectorCharacter[]
): number {
  return clips.reduce((worst, c) => Math.max(worst, clipRefCount(c, characters)), 0);
}

/**
 * The one-line advisory for a project's identity setup, or `null` when there is nothing to say.
 *
 * Three things go wrong silently and none of them fail a render, which is exactly why the editor
 * has to say them: the wrong DiT (references applied, no trained response), a reference that is the
 * wrong KIND for the regime, and too much of the sequence spent on references.
 *
 * `refs` is ONE SHOT's count — pass {@link worstClipRefCount} for a project-level verdict.
 */
export function identityAdvisory(
  model: string | undefined,
  width: number,
  height: number,
  frames: number,
  refs: number,
  loras: LtxRuntimeLora[] = []
): string | null {
  if (refs <= 0) return null;
  if (!ltxModelIsIdentityTrained(model, loras)) {
    return (
      `Nothing in this render conditions on identity — no identity LoRA is attached, so the ` +
      `engine still applies the references but nothing has a trained response to them. Attach ` +
      `${shortLoraName(LTX_LORA_MSR_V2)} (characters, locations AND props as separate entities) ` +
      `or ${shortLoraName(LTX_LORA_FACEID_SHEET)} (4-panel character sheets).`
    );
  }
  const regime = ltxIdentityRegime(model, loras);
  const share = refTokenShare(width, height, frames, refs, regime);
  const pct = Math.round(share * 100);
  const busiest = `The busiest shot's ${refs} reference${refs === 1 ? '' : 's'}`;
  if (share >= REF_SHARE_CAPTURE) {
    return (
      `${busiest} take ${pct}% of its sequence — at this share the reference CAPTURES the framing ` +
      `(measured at 39.5%) and the shot comes back composed like the reference. Use longer shots, ` +
      `a bigger render, or switch characters off on the shots they are not in.`
    );
  }
  if (share >= REF_SHARE_WARN) {
    return (
      `${busiest} take ${pct}% of its sequence — above the measured-clean band (11.8%) and heading ` +
      `toward framing capture. Switching characters off on shots they are not in is the cheap fix.`
    );
  }
  return null;
}

/**
 * What the project's REFERENCE ROLES imply about model selection, or `null` when nothing needs
 * saying. Complements {@link identityAdvisory}, which is about cost and kind rather than routing.
 *
 * Two things this reports, both invisible in a successful render:
 *  - a project whose references include props but whose conditioning cannot compose them;
 *  - how many adapter FOLDS the resolved per-shot plan costs, since each is GPU work at the shot
 *    boundary and grouping the prop shots together removes most of them.
 */
export function roleAdvisory(
  clips: Pick<DirectorClip, 'id' | 'character_ids' | 'model' | 'loras'>[],
  characters: DirectorCharacter[],
  projectModel: string | undefined,
  projectLoras: LtxRuntimeLora[] = []
): string | null {
  const props = characters.filter((c) => characterRole(c) === 'prop');
  if (props.length === 0) return null;
  const project = projectModel || 'base';
  const isProp = props.length === 1 ? ' is a prop' : 's are props';
  const msr = shortLoraName(LTX_LORA_MSR_V2);
  if (!ltxModelIsIdentityTrained(project, projectLoras)) {
    return (
      `${props.length} reference${isProp}, but nothing in this render conditions on identity — ` +
      `no identity LoRA is attached, so nothing holds. Attach ${msr}.`
    );
  }
  if (ltxIdentityRegime(project, projectLoras) !== 'memory') {
    return (
      `${props.length} reference${isProp}, which the Best-Face-ID sheet regime cannot compose — it ` +
      `expects a 4-panel sheet of one person. Attach ${msr} instead of the sheet adapter.`
    );
  }
  const resolved = resolveClipVariants(clips, characters, project, projectLoras);
  const advised = [...resolved.values()].filter((v) => v.automatic).length;
  if (advised === 0) return null;
  const folds = loraFoldCount(clips, projectLoras);
  return (
    `${advised} shot${advised === 1 ? '' : 's'} carr${advised === 1 ? 'ies' : 'y'} a prop, which ` +
    `${project} fuses into whoever holds it — ${msr} keeps them separate. Add it under Runtime ` +
    `LoRAs, on those shots or the whole project; it is NOT applied for you, because MSR stacked on ` +
    `${project} has not been eye-tested yet. ` +
    (folds > 2
      ? `Note the adapter set already changes ${folds} times down this timeline and each change ` +
        `re-folds it into the weights, so grouping shots that share a set is worth doing.`
      : `Adding it to the whole project keeps the timeline on ONE fold; adding it to a few shots ` +
        `costs a re-fold at each boundary.`)
  );
}
