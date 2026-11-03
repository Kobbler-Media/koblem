/**
 * Ref2VA **keyframe completion** — an opening or closing image that reaches the model as a
 * `<Picture N>` and four sentences instead of as a wire pin on the other checkpoint.
 *
 * ## 🔴 Why this exists: a frame pin used to cost the whole request
 *
 * Until 2026-08-08 any attached first/last frame forced `fl2va`. That is a different weight set,
 * not a flag, so the generation lost **every omni reference and the project song**, and the packer
 * isolated it on both sides. Measured against the source frame
 * (`~/handoffs/docker/h3-wangp/HANDOFF-koblem-h3-fixes-2026-08-08.md` §3):
 *
 * | path | frame-0 NCC | keeps references + song | cost |
 * |---|---|---|---|
 * | exact pin, `fl2va` wire | **0.968** | no | a 64 s checkpoint flip, isolated on both sides |
 * | prose keyframe, `ref2va` (this file) | 0.776 | **yes** | free |
 * | plain identity reference | 0.262 | yes | free |
 *
 * So the prose keyframe is a real anchor rather than a hint — it is 3× closer to the source frame
 * than simply handing the model the same picture as an identity reference — and it is the right
 * default whenever the project carries a song, because the pin throws the song slice away.
 *
 * ## What the emitter writes, and where the authority is
 *
 * `keyframe completion` is a first-class Ref2VA task type. `GUIDE-ref:134` defines it, and §2.2,
 * §3 and §4.1 give the exact line shapes. Four declarations, one per section:
 *
 * | section | line |
 * |---|---|
 * | `subject_definitions` | `<Picture 1> is the first frame of [Shot 1]: …` |
 * | `summary` | `keyframe completion` joined into the `[…]` bracket |
 * | `retention_analysis` | `<Picture 1> ([Shot 1] first frame): fully_preserved - …` |
 * | `detailed_description` | `[Shot 1] The shot begins from <Picture 1>. …` |
 *
 * ## 🔴 Per-ASSET exclusivity, and why it is not enforced here
 *
 * `reference generation` and `keyframe completion` are mutually exclusive **for the same asset**
 * (`GUIDE-ref:135` — guidance *"without serving as a concrete frame"*). They coexist happily in one
 * summary when different assets play different roles. That rule is enforced structurally in
 * `h3-roles.ts`: a reference has ONE role, `H3_ROLE_TASK_TYPE` maps one role to one task type, so a
 * single asset cannot earn both. `h3UnjustifiedTaskTypes` catches the other half — a bracket
 * claiming a type no asset in scope plays.
 *
 * ## 🔴 Why none of this lives in `composeH3Prompt`
 *
 * The round-trip `compose(parse(compose(x))) === compose(x)` is the gate on the whole lens
 * (`h3-block.ts`). If the composer injected these four declarations from a keyframe list, then
 * parsing its output and recomposing would inject them a SECOND time — the definition line, the
 * retention row and the shot opener would all double, and the round-trip test would be the thing
 * that broke rather than the thing that caught it. So the composer stays a pure emitter of the
 * fields it is handed, and this module has two entry points instead:
 *
 * - {@link h3ApplyKeyframes} folds the declarations into a compose INPUT — used when a generation's
 *   block is seeded, so a fresh keyframe shot opens already declared.
 * - `h3WriteKeyframes` (in `h3-document.ts`, because surgical block writes are that module's job)
 *   writes them into an EXISTING block, section by section through `replaceH3Section`. Both are
 *   idempotent per piece: a declaration that is already there is never written twice.
 *
 * ⚠️ This module deliberately imports nothing from `h3-document.ts`. The dependency runs one way —
 * document → keyframes — so the pair cannot form an import cycle, which on a `const`-arrow export
 * would be a temporal-dead-zone crash at module init rather than a bundler warning.
 */

import type {
  DirectorH3Reference,
  H3RetentionEntry,
  H3SubjectDefinition,
  H3TaskType
} from '#lib/api/client.ts';
import type { H3ParsedBlock } from './h3-block';
import { h3KeyframeImages, h3KeyframeRefId, type PackedGeneration } from './h3-generations';
import { h3UnjustifiedTaskTypes, isFrameRole, roleLabel } from './h3-roles';
import { h3Ordinals, type H3ComposeInput, type H3PromptIssue, type H3ShotBody } from './h3-prompt';

/** The task type a keyframe earns. A literal because it is a fixed English value in the schema. */
export const H3_KEYFRAME_TASK_TYPE: H3TaskType = 'keyframe completion';

/** One image acting as a concrete frame of one shot. */
export interface H3KeyframeDeclaration {
  /** The synthesised reference's id — see `h3KeyframeRefId`. */
  refId: string;
  /** The ordinal the conditioner will allocate it, brackets included: `<Picture 3>`. */
  label: string;
  /** 1-based `[Shot N]` number inside this generation. */
  shotNumber: number;
  edge: 'first' | 'last';
}

/** `first frame` / `last frame`, as both the guide and the retention note write it. */
const edgeWords = (edge: 'first' | 'last'): string => `${edge} frame`;

/**
 * The `subject_definitions` text for a keyframe, WITHOUT its leading label.
 *
 * The label is stored separately from the text everywhere in the editor and the composer re-joins
 * them (`composeH3Prompt` writes `${label} ${text}`), so writing it in here too would emit
 * `<Picture 3> <Picture 3> is the first frame…`.
 *
 * ⚠️ It ends on a colon, deliberately. `GUIDE-ref:2.2`'s example continues into a description of
 * what the frame shows (*"showing a woman seated beside a café window"*) — and only the author (or
 * the drafting LLM) can see the picture. A trailing colon reads as an invitation to finish the
 * sentence; a full stop reads as a finished one that happens to say nothing.
 */
export const h3KeyframeDefinitionText = (k: H3KeyframeDeclaration): string =>
  `is the ${edgeWords(k.edge)} of [Shot ${k.shotNumber}]:`;

/** The `retention_analysis` row: `<Picture 1> ([Shot 1] first frame): fully_preserved - …`. */
export const h3KeyframeRetention = (k: H3KeyframeDeclaration): H3RetentionEntry => ({
  label: k.label,
  role_note: `[Shot ${k.shotNumber}] ${edgeWords(k.edge)}`,
  // The strongest visible-content marker, and the correct one: the whole point of a keyframe is
  // that the target reproduces that frame. `partially_preserved` here would be asking the model to
  // drift off the anchor we just paid for.
  marker: 'fully_preserved',
  text: `the ${edgeWords(k.edge)} of the target video reproduces ${k.label}.`
});

/** The sentence that opens (or closes) the shot body. `GUIDE-ref` writes it inside the shot. */
export const h3KeyframeOpener = (k: H3KeyframeDeclaration): string =>
  k.edge === 'first' ? `The shot begins from ${k.label}.` : `The shot ends on ${k.label}.`;

/**
 * Every keyframe declaration a packed generation implies, in ordinal order.
 *
 * Reads the ordinals off `gen.references`, which is where `packGenerations` appended the
 * synthesised pictures — so the label here is the one the CONDITIONER will allocate, not one this
 * module counted for itself. Counting separately is how a preview ends up naming `<Picture 2>` for
 * an asset the engine packed as `<Picture 3>`, which renders fine and talks about the wrong image.
 */
export function h3KeyframesFor(gen: PackedGeneration): H3KeyframeDeclaration[] {
  const ordinals = h3Ordinals(gen.references);
  const byId = new Map(ordinals.map((o) => [o.id, o.label]));
  return gen.shots.flatMap((s) =>
    h3KeyframeImages(s.clip).flatMap((img) => {
      const refId = h3KeyframeRefId(s.clip.id, img.edge);
      const label = byId.get(refId);
      // Absent only if the packer and this module disagree about what is in scope, which would be a
      // bug — but inventing an ordinal to paper over it would put a label in the prompt that
      // resolves against nothing, which is worse than emitting nothing.
      return label ? [{ refId, label, shotNumber: s.shotIndex + 1, edge: img.edge }] : [];
    })
  );
}

// ── Which pieces are already in the block ────────────────────────────────────

/**
 * Is this keyframe's definition line present?
 *
 * Matched on the LABEL alone, never on the sentence. The author is expected to finish the
 * definition with what the picture actually shows, and a text match would make their own wording
 * look like an absence and re-add the template underneath it.
 */
export const h3KeyframeHasDefinition = (parsed: H3ParsedBlock, k: H3KeyframeDeclaration): boolean =>
  parsed.subjectDefinitions.some((d) => d.label === k.label);
/** Is its retention row present? */
export const h3KeyframeHasRetention = (parsed: H3ParsedBlock, k: H3KeyframeDeclaration): boolean =>
  parsed.retentionAnalysis.some((e) => e.label === k.label);
/** Does the shot body it anchors already name it? */
export const h3KeyframeHasOpener = (parsed: H3ParsedBlock, k: H3KeyframeDeclaration): boolean =>
  (parsed.shots[k.shotNumber - 1]?.body ?? '').includes(k.label);

/**
 * Keyframes the block does not yet fully declare — the list the editor offers to write.
 *
 * "Fully" is all four pieces. A block with the definition but no retention row is still under-
 * declared: the guide's own worked example carries both, and the retention marker is what tells the
 * model how hard to hold the frame. Partial is exactly the state a hand-edit leaves behind, so it
 * has to count as undeclared or the offer disappears the moment the author writes half of it.
 */
export function h3UndeclaredKeyframes(
  parsed: H3ParsedBlock,
  keyframes: readonly H3KeyframeDeclaration[]
): H3KeyframeDeclaration[] {
  const typed = parsed.taskTypes.includes(H3_KEYFRAME_TASK_TYPE);
  return keyframes.filter(
    (k) =>
      !typed ||
      !h3KeyframeHasDefinition(parsed, k) ||
      !h3KeyframeHasRetention(parsed, k) ||
      !h3KeyframeHasOpener(parsed, k)
  );
}

// ── Seeding: fold the declarations into a compose input ──────────────────────

/**
 * Fold keyframe declarations into a compose input, for the block SEED only.
 *
 * 🔴 Not called by `composeH3Prompt` and never on a block that already exists — see this module's
 * header for why that would break the round-trip guarantee. Its one caller is `h3SeedInput`, so a
 * generation whose shot opens on an image is born declared instead of being born wrong and needing
 * a repair button.
 *
 * Idempotent against the input it is given: a definition, retention row or opener that is already
 * in the input is left exactly as the author wrote it rather than replaced with the template.
 */
export function h3ApplyKeyframes(
  input: H3ComposeInput,
  keyframes: readonly H3KeyframeDeclaration[]
): H3ComposeInput {
  if (!keyframes.length) return input;

  const defs: H3SubjectDefinition[] = [...(input.subjectDefinitions ?? [])];
  const ret: H3RetentionEntry[] = [...(input.retentionAnalysis ?? [])];
  const shots: H3ShotBody[] = input.shots.map((s) => ({ ...s }));

  for (const k of keyframes) {
    if (!defs.some((d) => d.label === k.label)) {
      defs.push({ label: k.label, text: h3KeyframeDefinitionText(k) });
    }
    if (!ret.some((e) => e.label === k.label)) ret.push(h3KeyframeRetention(k));
    const shot = shots[k.shotNumber - 1];
    // 🔴 The opener goes at the FRONT of the body, after any cut verb the composer will prefix.
    // `shotText` writes `[Shot N] At MM:SS.mmm, <verb> <body>`, so an opener appended at the end
    // would tell the model the shot BEGINS from a picture in the sentence after it has described
    // the whole shot. Position is the instruction here.
    if (shot && !shot.body.includes(k.label)) {
      shot.body = `${h3KeyframeOpener(k)} ${shot.body}`.trim();
    }
  }

  const taskTypes = [...(input.taskTypes ?? [])];
  if (!taskTypes.includes(H3_KEYFRAME_TASK_TYPE)) taskTypes.push(H3_KEYFRAME_TASK_TYPE);

  return {
    ...input,
    subjectDefinitions: defs,
    retentionAnalysis: ret,
    taskTypes,
    shots
  };
}

// ── Validation: what the block claims vs what the request contains ───────────

/**
 * Issues about references and keyframes that {@link validateH3Prompt} cannot see.
 *
 * Kept out of `h3-prompt.ts` for one structural reason: that module is imported BY this one (for
 * `h3Ordinals` and the compose types), so a check that needs a reference's ROLE or a keyframe
 * declaration cannot live there without an import cycle. The composer concatenates the two lists,
 * and both tiers mean exactly what `H3IssueLevel` says they mean.
 *
 * Three checks. The first IS a refusal and the other two are not:
 *
 * 0. 🔴 **Over an engine reference cap**, counting the keyframe pictures the packer synthesised.
 *    `error` — see {@link capIssues}. Skipped entirely when `caps` is absent.
 * 1. **An undeclared keyframe.** `unwritten` — the image IS attached and IS sent as a `<Picture N>`,
 *    so the render succeeds; what is missing is the four sentences that turn it from a picture the
 *    model has seen into the frame the shot opens on. That difference measured 0.262 → 0.776
 *    frame-0 NCC, which is the whole reason the path exists.
 * 2. 🔴 **A task type nothing in scope justifies.** `warn`. This is the other half of the guide's
 *    per-ASSET exclusivity rule (`GUIDE-ref:135`): one asset has one role and therefore one task
 *    type, which `H3_ROLE_TASK_TYPE` makes structural — but the bracket is free text in the block,
 *    so a hand-edit or an LLM can still claim `[reference generation + keyframe completion]` over a
 *    single first-frame picture. That is not refused; the model simply reads a relationship the
 *    request does not contain.
 */
/**
 * What the engine will accept on ONE request. From `H3Props`; `null` when props were never fetched.
 *
 * ⚠️ Per REQUEST, which on this engine is per generation — not per project. That distinction is the
 * entire point of {@link h3ReferenceIssues}' cap check: the References panel counts a roster the
 * author assembled, and the engine counts the list one call carries, and a keyframe picture is in
 * the second and not in the first.
 */
export interface H3ReferenceCaps {
  maxImages: number;
  maxVideos: number;
  maxAudios: number;
  maxTotal: number;
}

/**
 * 🔴 **A keyframe picture occupies an engine reference slot that no roster count sees.**
 *
 * `h3KeyframeReferences` appends a synthesised `<Picture N>` to the generation's reference list for
 * every `keyframe`-mode first/last frame. It is a real asset on the wire — that is the whole point,
 * it is what makes the 0.776 frame-0 match possible — so it is counted by
 * `validate_generative_settings` exactly like any other picture. But it is not in `h3References`,
 * so `ReferencesPanel`'s "9 pictures max" arithmetic has never known about it.
 *
 * Consequence, before this check existed: a project sitting on the picture cap plus one cut that
 * opens on an image is a **400 at submit** — after the queue wait, naming a limit the editor had
 * just told the author they were inside. Which is why this is `error` rather than `warn`: it is not
 * "outside the guide's envelope, renders anyway", it is a refusal.
 */
function capIssues(
  refs: readonly DirectorH3Reference[],
  keyframes: readonly H3KeyframeDeclaration[],
  caps: H3ReferenceCaps
): H3PromptIssue[] {
  const kf = new Set(keyframes.map((k) => k.refId));
  const count = (kind: DirectorH3Reference['kind']) => refs.filter((r) => r.kind === kind).length;
  // Counted by kind exactly as `ReferencesPanel` counts the roster, so the two numbers are
  // comparable rather than two different arithmetics that happen to be near each other.
  const rows: { n: number; max: number; what: string }[] = [
    { n: count('image'), max: caps.maxImages, what: 'pictures' },
    { n: count('video'), max: caps.maxVideos, what: 'clips' },
    { n: count('audio'), max: caps.maxAudios, what: 'audio references' },
    { n: refs.length, max: caps.maxTotal, what: 'references in total' }
  ];
  return rows
    .filter((r) => r.n > r.max)
    .map((r) => {
      // Named only where it is the reason, and named by COUNT rather than by ordinal: which
      // specific picture tips it over is arbitrary, but "N of these are keyframes you may not have
      // counted" is the fact that makes the number make sense.
      const fromKeyframes = refs.filter((x) => kf.has(x.id) && x.kind === 'image').length;
      const because =
        fromKeyframes && (r.what === 'pictures' || r.what === 'references in total')
          ? ` ${fromKeyframes} of them ${fromKeyframes === 1 ? 'is a frame' : 'are frames'} a cut opens or closes on — those are real <Picture N> assets on the wire, appended to this request, and they are not in the References list.`
          : '';
      return {
        level: 'error' as const,
        field: 'references',
        message: `This render sends ${r.n} ${r.what}; the engine takes ${r.max}. It is refused at submit, not trimmed.${because}`
      };
    });
}

export function h3ReferenceIssues(
  parsed: H3ParsedBlock,
  refs: readonly DirectorH3Reference[],
  keyframes: readonly H3KeyframeDeclaration[],
  caps?: H3ReferenceCaps | null
): H3PromptIssue[] {
  const out: H3PromptIssue[] = caps ? capIssues(refs, keyframes, caps) : [];

  for (const k of h3UndeclaredKeyframes(parsed, keyframes)) {
    out.push({
      level: 'unwritten',
      field: 'keyframes',
      message: `${k.label} is attached as the ${edgeWords(k.edge)} of [Shot ${k.shotNumber}] but the prompt does not say so. It is sent either way — as a picture the model has merely seen, which measured 0.262 frame-0 similarity against 0.776 once the four keyframe-completion declarations are written.`
    });
  }

  for (const t of h3UnjustifiedTaskTypes(refs, parsed.taskTypes)) {
    const frames = refs.filter((r) => isFrameRole(r.role)).length;
    out.push({
      level: 'warn',
      field: 'summary',
      message:
        `The summary claims "${t}", which no reference in scope plays the role for` +
        (t === 'reference generation' && frames
          ? ` — every picture here is a concrete frame anchor (${refs
              .filter((r) => isFrameRole(r.role))
              .map((r) => roleLabel(r.role).toLowerCase())
              .join(
                ', '
              )}), and the guide defines reference generation as guidance WITHOUT serving as a concrete frame. The two are mutually exclusive for the same asset.`
          : t === H3_KEYFRAME_TASK_TYPE
            ? ' — no picture here is set to First frame, Last frame or Frame anchor. Set the role on the reference, or drop the claim.'
            : '. Set the role that matches on the reference itself, or drop the claim — the bracket is what tells the model what relationship to look for.')
    });
  }

  return out;
}
