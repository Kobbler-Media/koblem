<script lang="ts">
  /*
   * The options for one joint, drawn at the joint.
   *
   * 🔴 THE NAMING FLOW IS INVERTED FROM THE ONE THIS REPLACES, and that inversion is the whole
   * point. The old panel asked you to pick a missing core slot and then choose from a dropdown of
   * sanitized bone names — `mixamorigLeftArm`, `joint_31`, `Bone_007` — which say nothing about
   * where the joint IS. It was unusable by construction. Here you click the joint you can see and
   * the missing slots it could plausibly fill come to you, ranked by `coreCandidates` (skeleton
   * topology, not string similarity).
   *
   * ⚠️ Naming a joint does not check that it is anatomically where the name claims. A wrong
   * assignment retargets CLEANLY and animates WRONGLY, and nothing downstream will ever notice.
   * "Show what it drives" is the check: it paints the vertices this joint actually moves, so
   * claiming a joint is the left hand and watching the right foot light up is a caught mistake
   * rather than a shipped one.
   */
  import type { JointState } from './overlays';

  let {
    name,
    sanitized,
    // 🔴 Aliased, and it has to stay aliased. A local binding called `state` makes every `$state(…)`
    // in this file parse as a STORE SUBSCRIPTION on it (`$`-prefix = store access), so `showAll`
    // silently stopped being reactive and the type-check failed on `state` having no `.subscribe`.
    // The prop keeps its public name; only the binding moves.
    state: jointState,
    suggestions,
    driving = false,
    moved = false,
    canEdit = true,
    reason = '',
    keepChildren = $bindable(true),
    dissolveRefusal = '',
    dissolvedAlready = false,
    onAssign,
    onNudge,
    onToggleDrives,
    onReset,
    onDissolve,
    onClose
  }: {
    /** The CANONICAL, colon-bearing name — `bone.userData.name ?? bone.name`. This is what the
     *  engine's retarget matches on; `sanitized` is what three.js calls the same bone. */
    name: string;
    /** The three-side name. Shown only when it differs, so the two-names rule is visible rather
     *  than folklore. */
    sanitized: string;
    state: JointState;
    /** Missing core slots this joint could fill, best first. `rank` is its position in
     *  `coreCandidates` for that slot: 0 means "hangs directly off the right parent". */
    suggestions: { core: string; rank: number }[];
    /** Is the what-it-drives paint currently on for this joint? */
    driving?: boolean;
    /** Is this joint off its delivered position? */
    moved?: boolean;
    /** False on a take: every edit here re-binds the skeleton, which is rest-pose only. */
    canEdit?: boolean;
    /** Why editing is refused, shown in place of the controls. */
    reason?: string;
    keepChildren?: boolean;
    /** Why this joint cannot be dissolved, or empty if it can. A root joint has no parent to take
     *  its weights; one already staged has nothing left to do. */
    dissolveRefusal?: string;
    /** Already staged for deletion by an earlier press. */
    dissolvedAlready?: boolean;
    onAssign: (core: string) => void;
    onNudge: (axis: 0 | 1 | 2, sign: 1 | -1) => void;
    onToggleDrives: () => void;
    onReset: () => void;
    onDissolve: () => void;
    onClose: () => void;
  } = $props();

  const AXES = [
    { axis: 0 as const, label: 'X' },
    { axis: 1 as const, label: 'Y' },
    { axis: 2 as const, label: 'Z' }
  ];

  const STATE_WORD: Record<JointState, string> = {
    core: 'fills a core slot',
    named: 'named, not a core slot',
    unnamed: 'anonymous — no clip can drive it',
    dissolved: 'dissolved — the save will delete it'
  };

  /** `mixamorig:LeftForeArm` → `LeftForeArm`. The prefix is on every one of the 22 and is noise in
   *  a list of them; it still goes into the file in full. */
  function shortCore(core: string): string {
    const colon = core.indexOf(':');
    return colon < 0 ? core : core.slice(colon + 1);
  }

  let showAll = $state(false);
  const TOP = 5;
  const shown = $derived(showAll ? suggestions : suggestions.slice(0, TOP));
</script>

<div class="pop">
  <div class="head">
    <div class="title">
      <code class="name">{name}</code>
      <span class="badge {jointState}">{STATE_WORD[jointState]}</span>
    </div>
    <button class="x" onclick={onClose} aria-label="Close this joint's options">×</button>
  </div>
  {#if sanitized !== name}
    <p class="hint">
      three.js calls it <code>{sanitized}</code> — it deletes <code>:</code> from every node name. The
      colon-bearing form is what reaches the file.
    </p>
  {/if}

  <div class="row">
    <button
      class="btn-sec small"
      class:on={driving}
      onclick={onToggleDrives}
      aria-pressed={driving}
    >
      {driving ? 'Hide what it drives' : 'Show what it drives'}
    </button>
  </div>
  <p class="hint">
    Paints the vertices this joint actually moves. This is the only check there is on a name being
    right — a joint claiming to be the left hand should light up the left hand.
  </p>

  {#if !canEdit}
    <p class="warn">{reason}</p>
  {:else}
    {#if suggestions.length}
      <h4>Name this joint</h4>
      <p class="hint">
        ⚠️ Unvalidated. A wrong assignment retargets cleanly and animates wrongly. A take is 0.22 s
        — bake one and look.
      </p>
      <ul class="slots">
        {#each shown as slot (slot.core)}
          <li>
            <button class="slot" onclick={() => onAssign(slot.core)}>
              <span class="core">{shortCore(slot.core)}</span>
              {#if slot.rank === 0}
                <span class="fit">best fit</span>
              {/if}
            </button>
          </li>
        {/each}
      </ul>
      {#if suggestions.length > TOP}
        <button class="link" onclick={() => (showAll = !showAll)}>
          {showAll ? 'Show fewer' : `${suggestions.length - TOP} more slots`}
        </button>
      {/if}
    {:else if jointState === 'core'}
      <p class="hint">This joint already fills a core slot, so it is not offered for another.</p>
    {:else}
      <p class="hint">Every core slot is filled. Nothing to name here.</p>
    {/if}

    <h4>Move it</h4>
    <div class="nudge">
      {#each AXES as ax (ax.axis)}
        <span class="axis">{ax.label}</span>
        <button
          class="btn-sec small"
          onclick={() => onNudge(ax.axis, -1)}
          aria-label={`nudge ${ax.label} negative`}>−</button
        >
        <button
          class="btn-sec small"
          onclick={() => onNudge(ax.axis, 1)}
          aria-label={`nudge ${ax.label} positive`}>+</button
        >
      {/each}
    </div>
    <label class="toggle">
      <input type="checkbox" bind:checked={keepChildren} /> Move this joint only
    </label>
    <p class="hint">
      {#if keepChildren}
        Children are held where they are, so dragging an elbow leaves the wrist alone.
      {:else}
        Every joint below follows, which re-proportions the limb.
      {/if}
      One step is 0.5% of the model's size. The mesh does not move — the joint moves
      <em>inside</em> it, and its rotation pivot moves with it.
    </p>
    {#if moved}
      <div class="row">
        <button class="btn-sec small" onclick={onReset}>Put this joint back</button>
      </div>
    {/if}

    <h4>Delete it</h4>
    {#if dissolvedAlready}
      <p class="hint">
        Staged for deletion. Its weights are on its parent and its children hang off the parent now;
        the joint itself goes when you save. Ctrl+Z puts all three back.
      </p>
    {:else if dissolveRefusal}
      <p class="hint">{dissolveRefusal}</p>
    {:else}
      <div class="row">
        <button class="btn-sec small danger" onclick={onDissolve}>Dissolve into its parent</button>
      </div>
      <p class="hint">
        For the joint that should not be there — the one sticking out of the middle of the body. Its
        weights move onto its parent and any joints below it re-hang there, both without moving the
        mesh; the joint is cut out of the file when you save. One undo step.
      </p>
    {/if}
  {/if}
</div>

<style>
  .pop {
    width: 264px;
    max-height: 60vh;
    overflow-y: auto;
    box-sizing: border-box;
    padding: 10px 12px 12px;
    border: 1px solid var(--ring);
    border-radius: 10px;
    background: var(--card);
    box-shadow: 0 10px 30px rgb(0 0 0 / 45%);
    pointer-events: auto;
  }
  .danger {
    border-color: var(--danger, #ef4444);
    color: var(--danger, #ef4444);
  }
  .head {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    justify-content: space-between;
  }
  .title {
    display: flex;
    flex-direction: column;
    gap: 3px;
    min-width: 0;
  }
  .name {
    font-size: 12px;
    color: var(--ink);
    overflow-wrap: anywhere;
  }
  .badge {
    font-size: 10px;
    letter-spacing: 0.02em;
    color: var(--muted);
  }
  .badge.core {
    color: var(--ok, #4ade80);
  }
  .badge.named {
    color: var(--accent);
  }
  .badge.unnamed {
    color: var(--warning, #d9952d);
  }
  .x {
    flex: none;
    background: none;
    border: none;
    color: var(--muted);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    padding: 0 2px;
  }
  .x:hover {
    color: var(--ink);
  }
  h4 {
    margin: 12px 0 0;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted);
  }
  .hint {
    color: var(--muted);
    font-size: 10.5px;
    line-height: 1.4;
    margin: 6px 0 0;
  }
  .warn {
    color: var(--warning, #d9952d);
    font-size: 10.5px;
    line-height: 1.4;
    margin: 8px 0 0;
  }
  .row {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    margin-top: 8px;
  }
  .btn-sec.small {
    padding: 3px 9px;
    font-size: 11px;
  }
  .btn-sec.small.on {
    border-color: var(--accent);
    color: var(--accent);
  }
  .slots {
    list-style: none;
    margin: 6px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .slot {
    width: 100%;
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
    padding: 5px 8px;
    background: var(--bg-2);
    border: 1px solid var(--ring);
    border-radius: 6px;
    color: var(--ink);
    font-family: inherit;
    font-size: 11.5px;
    text-align: left;
    cursor: pointer;
  }
  .slot:hover {
    border-color: var(--accent);
  }
  .fit {
    font-size: 10px;
    color: var(--ok, #4ade80);
  }
  .link {
    margin-top: 6px;
    background: none;
    border: none;
    padding: 0;
    color: var(--accent);
    font-family: inherit;
    font-size: 10.5px;
    cursor: pointer;
  }
  .nudge {
    display: flex;
    align-items: center;
    gap: 4px;
    margin-top: 8px;
  }
  .nudge .axis {
    color: var(--muted);
    font-size: 10.5px;
    margin-left: 6px;
  }
  .nudge .axis:first-child {
    margin-left: 0;
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 10px;
    font-size: 11px;
    color: var(--ink);
  }
</style>
