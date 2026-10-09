<script lang="ts">
  // Everything that is not a shot, a prompt or a render — as a list of rows, each opening its own
  // full-screen sheet with a real back button. The desktop's rail is four folds and one modal whose
  // contents you switch with tabs; neither shape survives a 390px column.
  let {
    storySummary,
    refsTitle,
    refsSummary,
    songSummary,
    settingsSummary,
    reviseSummary,
    autoSummary,
    promptsSummary,
    projectSummary,
    duplicating,
    canDuplicate,
    onopen,
    onchat,
    onprojects,
    onduplicate
  }: {
    storySummary: string;
    /** The cast panel and the reference panel are different mechanisms, so the row is named by the
     * engine's own — see the `castBody` branch in DirectorWorkspace. */
    refsTitle: string;
    refsSummary: string;
    songSummary: string;
    settingsSummary: string;
    reviseSummary: string;
    autoSummary: string;
    /**
     * The Prompts row's second line, or `null` to leave the row out — the engine has no structured
     * prompt, or there is nothing on the timeline to have one yet.
     */
    promptsSummary: string | null;
    projectSummary: string;
    duplicating: boolean;
    canDuplicate: boolean;
    onopen: (sheet: string) => void;
    onchat: () => void;
    onprojects: () => void;
    onduplicate: () => void;
  } = $props();
</script>

<p class="eye">This project</p>
<div class="mlist">
  <button class="mrow" type="button" onclick={() => onopen('story')}>
    <span class="gl" aria-hidden="true">◈</span>
    <span class="tx"><b>Story context</b><span>{storySummary}</span></span>
    <span class="ch" aria-hidden="true">›</span>
  </button>
  <button class="mrow" type="button" onclick={() => onopen('refs')}>
    <span class="gl" aria-hidden="true">◧</span>
    <span class="tx"><b>{refsTitle}</b><span>{refsSummary}</span></span>
    <span class="ch" aria-hidden="true">›</span>
  </button>
  <button class="mrow" type="button" onclick={() => onopen('audio')}>
    <span class="gl" aria-hidden="true">♫</span>
    <span class="tx"><b>Song &amp; lyrics</b><span>{songSummary}</span></span>
    <span class="ch" aria-hidden="true">›</span>
  </button>
  <button class="mrow" type="button" onclick={() => onopen('settings')}>
    <span class="gl" aria-hidden="true">⚙</span>
    <span class="tx"><b>Render settings</b><span>{settingsSummary}</span></span>
    <span class="ch" aria-hidden="true">›</span>
  </button>
</div>

<p class="eye" style="margin-top:14px">Write with the director</p>
<div class="mlist">
  <button class="mrow" type="button" onclick={onchat}>
    <span class="gl" aria-hidden="true">✦</span>
    <span class="tx"><b>Chat</b><span>Ask for edits across the whole cut</span></span>
    <span class="ch" aria-hidden="true">›</span>
  </button>
  <button class="mrow" type="button" onclick={() => onopen('auto')}>
    <span class="gl" aria-hidden="true">⌁</span>
    <span class="tx"><b>Auto-direct</b><span>{autoSummary}</span></span>
    <span class="ch" aria-hidden="true">›</span>
  </button>
  {#if promptsSummary}
    <button class="mrow" type="button" onclick={() => onopen('prompts')}>
      <span class="gl" aria-hidden="true">⌸</span>
      <span class="tx"><b>Prompts</b><span>{promptsSummary}</span></span>
      <span class="ch" aria-hidden="true">›</span>
    </button>
  {/if}
  <button class="mrow" type="button" onclick={() => onopen('revise')}>
    <span class="gl" aria-hidden="true">✎</span>
    <span class="tx"><b>Revise &amp; review</b><span>{reviseSummary}</span></span>
    <span class="ch" aria-hidden="true">›</span>
  </button>
</div>

<p class="eye" style="margin-top:14px">Projects</p>
<div class="mlist">
  <button class="mrow" type="button" onclick={onprojects}>
    <span class="gl" aria-hidden="true">▤</span>
    <span class="tx"><b>Switch project</b><span>{projectSummary}</span></span>
    <span class="ch" aria-hidden="true">›</span>
  </button>
  <button class="mrow" type="button" onclick={onduplicate} disabled={!canDuplicate || duplicating}>
    <span class="gl" aria-hidden="true">⧉</span>
    <span class="tx"
      ><b>Duplicate</b><span>{duplicating ? 'Copying…' : 'Copy this cut and its takes'}</span></span
    >
    <span class="ch" aria-hidden="true">›</span>
  </button>
</div>
