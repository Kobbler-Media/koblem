<script lang="ts">
  import type { DirectorProjectSummary } from '$lib/api/client';

  // Switch, rename, branch or delete the open project. The name field is here as well as in the app
  // bar because this sheet is what a narrow screen gets instead of one.
  let {
    projectName = $bindable(),
    projectId,
    projects,
    saveStatus,
    saving,
    stale,
    titling,
    duplicating,
    onclose,
    onopen,
    onnew,
    oninput,
    oncommitname,
    onautotitle,
    onsave,
    onduplicate,
    onremove
  }: {
    projectName: string;
    projectId: string | null;
    projects: DirectorProjectSummary[];
    saveStatus: string;
    saving: boolean;
    /** The document changed elsewhere, so every write is paused until that is resolved. */
    stale: boolean;
    titling: boolean;
    duplicating: boolean;
    onclose: () => void;
    onopen: (id: string) => void;
    onnew: () => void;
    oninput: () => void;
    oncommitname: () => void;
    onautotitle: () => void;
    onsave: () => void;
    onduplicate: () => void;
    onremove: () => void;
  } = $props();
</script>

<div class="overlay" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="modal" style="height:auto;max-width:460px">
    <div class="chat-h">
      <div class="chat-badge">🎬</div>
      <div style="flex:1">
        <b>{projectName}</b><small>{saveStatus}</small>
      </div>
      <button class="btn icon-only ghost" onclick={onclose}>✕</button>
    </div>
    <div class="sheet-body">
      {#if projects.length}
        <label class="field"
          ><span>Switch project</span>
          <select
            class="inp"
            onchange={(e) => {
              const v = e.currentTarget.value;
              if (v) onopen(v);
              e.currentTarget.value = '';
            }}
          >
            <option value="">— pick a project —</option>
            {#each projects as p (p.id)}<option value={p.id}
                >{p.name}{p.id === projectId ? ' (current)' : ''}</option
              >{/each}
          </select>
        </label>
      {/if}
      <label class="field" style="margin-top:14px"
        ><span>Project name</span>
        <div class="inline-row">
          <input class="inp" bind:value={projectName} {oninput} onblur={oncommitname} /><button
            class="btn sm"
            onclick={onautotitle}
            disabled={titling}>{titling ? '…' : '✨ Auto-title'}</button
          >
        </div>
      </label>
      <div class="inline-row" style="flex-wrap:wrap;gap:7px;margin-top:6px">
        <button class="btn sm" onclick={onnew}>New project</button>
        <button
          class="btn sm primary"
          onclick={onsave}
          disabled={saving || stale}
          title={stale ? 'Changed elsewhere — reload or overwrite first' : undefined}
          >{saving ? 'Saving…' : stale ? 'Changed elsewhere' : 'Save now'}</button
        >
        {#if projectId}<button class="btn sm" onclick={onduplicate} disabled={duplicating}
            >{duplicating ? 'Copying…' : 'Duplicate'}</button
          >{/if}
        {#if projectId}<button class="btn sm danger" onclick={onremove}>Delete</button>{/if}
      </div>
    </div>
  </div>
</div>
