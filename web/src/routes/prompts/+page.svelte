<script lang="ts">
  import { onMount } from 'svelte';
  import type {
    GrammarEntry,
    UpsertGrammarRequest,
    MediaKindKey,
    PromptSlot,
    PromptTemplatesOverview,
    PromptTemplateBody,
    PromptVariableSummary,
    PromptVariableListItem,
    PromptFixtureSummary,
    RenderPreviewResponse,
    ContextSchemaField
  } from '$lib/types';
  import {
    getPromptTemplatesOverview,
    getPromptTemplate,
    upsertPromptTemplate,
    deletePromptTemplate,
    listPromptVariables,
    upsertPromptVariable,
    deletePromptVariable,
    previewPromptRender,
    listPromptFixtures,
    getPromptContextSchema,
    listGrammars,
    upsertGrammar,
    deleteGrammar
  } from '$lib/api/client';
  import JsonBlock from '$lib/components/JsonBlock.svelte';

  type TabKey = 'system' | 'user' | 'variables';

  const TABS: { key: TabKey; label: string; desc: string }[] = [
    {
      key: 'system',
      label: 'System Template',
      desc: "The system message sent to the LLM. Renders once per request and is byte-stable across runs of the same media_kind so llama-server's KV cache can hit."
    },
    {
      key: 'user',
      label: 'User Template',
      desc: 'The user message. Per-request data (CURRENT DATE, WANTED, QUALITY WANT, CANDIDATES) is rendered here from the structured context — see the Available Context panel.'
    },
    {
      key: 'variables',
      label: 'Variables',
      desc: 'Named blobs that templates can reference via {{ vars.<name> }}. Three are seeded (signal_descriptions, watch_patterns, format); add your own as needed.'
    }
  ];

  // ── State ────────────────────────────────────────────────────────────
  let overview: PromptTemplatesOverview | null = $state(null);
  let activeKind: MediaKindKey = $state('default');
  let activeTab: TabKey = $state('system');
  let loading = $state(true);
  let error = $state('');
  let saveMessage = $state('');

  // Per-(kind, slot) loaded body cache and dirty-tracking draft.
  // Keyed by `${kind}:${slot}`.
  let templateRows: Record<string, PromptTemplateBody> = $state({});
  let templateDrafts: Record<string, string> = $state({});

  // Per-kind variable list + per-(kind, name) loaded summaries + drafts.
  let variableList: Record<string, PromptVariableListItem[]> = $state({});
  let variableRows: Record<string, PromptVariableSummary> = $state({});
  let variableDrafts: Record<string, string> = $state({});

  let editingVariableName: string | null = $state(null);
  let newVariableName = $state('');
  let confirmDeleteVariable: string | null = $state(null);
  let confirmResetSlot: PromptSlot | null = $state(null);
  let confirmResetVariable: string | null = $state(null);

  // Context schema panel
  let contextSchema: ContextSchemaField[] = $state([]);

  // Preview state
  let previewOpen = $state(false);
  let fixtures: PromptFixtureSummary[] = $state([]);
  let previewFixture = $state('');
  let previewPasted = $state('');
  let previewMode: 'fixture' | 'pasted' = $state('fixture');
  let preview: RenderPreviewResponse | null = $state(null);
  let previewLoading = $state(false);

  // Grammars (reused unchanged from old page)
  let grammars: GrammarEntry[] = $state([]);
  let grammarsLoading = $state(true);
  let grammarsError = $state('');
  let grammarsMessage = $state('');
  let editingGrammarName: string | null = $state(null);
  let grammarEditorDraft = $state({ name: '', gbnf: '', description: '' });
  let confirmDeleteGrammarName: string | null = $state(null);

  // ── Helpers ──────────────────────────────────────────────────────────
  function tplKey(kind: MediaKindKey, slot: PromptSlot): string {
    return `${kind}:${slot}`;
  }
  function varKey(kind: MediaKindKey, name: string): string {
    return `${kind}:${name}`;
  }

  function flashSaved(msg: string) {
    saveMessage = msg;
    setTimeout(() => (saveMessage = ''), 3000);
  }

  // ── Load ─────────────────────────────────────────────────────────────
  onMount(async () => {
    try {
      const [ov, schema, fx] = await Promise.all([
        getPromptTemplatesOverview(),
        getPromptContextSchema(),
        listPromptFixtures()
      ]);
      overview = ov;
      contextSchema = schema;
      fixtures = fx;
      if (fixtures.length > 0) previewFixture = fixtures[0].id;
      await loadKind(activeKind);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to load prompt overview';
    }
    loading = false;
    await reloadGrammars();
  });

  async function loadKind(kind: MediaKindKey) {
    error = '';
    try {
      // Three parallel HTTP calls = the whole tab. Each variable row already
      // carries its full body + flags in the list response — no per-name
      // fetch loop. Snappy enough that switching kinds feels instant.
      const [sysT, userT, vars] = await Promise.all([
        getPromptTemplate(kind, 'system'),
        getPromptTemplate(kind, 'user'),
        listPromptVariables(kind)
      ]);
      templateRows[tplKey(kind, 'system')] = sysT;
      templateRows[tplKey(kind, 'user')] = userT;
      templateDrafts[tplKey(kind, 'system')] = sysT.body;
      templateDrafts[tplKey(kind, 'user')] = userT.body;
      variableList[kind] = vars;
      for (const v of vars) {
        // Synthesize a PromptVariableSummary from the list row — same shape
        // as the single-variable endpoint, no extra HTTP needed.
        variableRows[varKey(kind, v.name)] = {
          media_kind: kind,
          name: v.name,
          body: v.body,
          is_overridden: v.has_per_kind_override,
          has_db_row: v.has_per_kind_override || v.has_default_override,
          is_seeded: v.is_seeded,
          updated_at: v.updated_at,
          default_body: v.default_body
        };
        variableDrafts[varKey(kind, v.name)] = v.body;
      }
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to load templates';
    }
  }

  async function selectKind(k: MediaKindKey) {
    if (k === activeKind) return;
    activeKind = k;
    confirmResetSlot = null;
    confirmResetVariable = null;
    editingVariableName = null;
    if (!templateRows[tplKey(k, 'system')]) await loadKind(k);
  }

  function isTplDirty(kind: MediaKindKey, slot: PromptSlot): boolean {
    const row = templateRows[tplKey(kind, slot)];
    const draft = templateDrafts[tplKey(kind, slot)];
    return row != null && draft !== row.body;
  }

  function isVarDirty(kind: MediaKindKey, name: string): boolean {
    const row = variableRows[varKey(kind, name)];
    const draft = variableDrafts[varKey(kind, name)];
    return row != null && draft !== row.body;
  }

  async function saveTemplate(kind: MediaKindKey, slot: PromptSlot) {
    const draft = templateDrafts[tplKey(kind, slot)];
    if (draft == null) return;
    error = '';
    try {
      await upsertPromptTemplate(kind, slot, draft);
      const fresh = await getPromptTemplate(kind, slot);
      templateRows[tplKey(kind, slot)] = fresh;
      templateDrafts[tplKey(kind, slot)] = fresh.body;
      flashSaved(`Saved ${slot} template for ${kind}`);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to save';
    }
  }

  async function resetTemplate(kind: MediaKindKey, slot: PromptSlot) {
    error = '';
    try {
      await deletePromptTemplate(kind, slot);
      const fresh = await getPromptTemplate(kind, slot);
      templateRows[tplKey(kind, slot)] = fresh;
      templateDrafts[tplKey(kind, slot)] = fresh.body;
      confirmResetSlot = null;
      flashSaved(`Reset ${slot} template for ${kind}`);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to reset';
    }
  }

  function revertTemplate(kind: MediaKindKey, slot: PromptSlot) {
    const row = templateRows[tplKey(kind, slot)];
    if (row) templateDrafts[tplKey(kind, slot)] = row.body;
  }

  function startEditVariable(name: string) {
    // Everything's already in `variableRows` from `loadKind` — just open
    // the editor.
    editingVariableName = name;
  }

  async function refreshVariablesAfterMutation(name: string) {
    // One round-trip after a save/reset/delete: refresh the whole list
    // (cheap — single SQL query) and re-sync the cached row + draft for
    // the touched variable.
    const list = await listPromptVariables(activeKind);
    variableList[activeKind] = list;
    const fresh = list.find((v) => v.name === name);
    if (fresh) {
      variableRows[varKey(activeKind, name)] = {
        media_kind: activeKind,
        name: fresh.name,
        body: fresh.body,
        is_overridden: fresh.has_per_kind_override,
        has_db_row: fresh.has_per_kind_override || fresh.has_default_override,
        is_seeded: fresh.is_seeded,
        updated_at: fresh.updated_at,
        default_body: fresh.default_body
      };
      variableDrafts[varKey(activeKind, name)] = fresh.body;
    } else {
      delete variableRows[varKey(activeKind, name)];
      delete variableDrafts[varKey(activeKind, name)];
    }
  }

  async function saveVariable(name: string) {
    const body = variableDrafts[varKey(activeKind, name)];
    if (body == null) return;
    error = '';
    try {
      await upsertPromptVariable(activeKind, name, body);
      await refreshVariablesAfterMutation(name);
      flashSaved(`Saved variable ${name} for ${activeKind}`);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to save variable';
    }
  }

  async function resetVariable(name: string) {
    error = '';
    try {
      await deletePromptVariable(activeKind, name);
      await refreshVariablesAfterMutation(name);
      confirmResetVariable = null;
      flashSaved(`Reset variable ${name} for ${activeKind}`);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to reset variable';
    }
  }

  async function deleteCustomVariable(name: string) {
    error = '';
    try {
      // Remove the per-kind row AND the default row to fully drop it from
      // the visible list. Only meaningful for non-seeded variables.
      await deletePromptVariable(activeKind, name);
      if (activeKind !== 'default') {
        await deletePromptVariable('default', name);
      }
      await refreshVariablesAfterMutation(name);
      confirmDeleteVariable = null;
      if (editingVariableName === name) editingVariableName = null;
      flashSaved(`Deleted variable ${name}`);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to delete variable';
    }
  }

  async function createNewVariable() {
    const name = newVariableName.trim();
    if (!name) {
      error = 'Variable name required';
      return;
    }
    if (!/^[A-Za-z0-9_]+$/.test(name)) {
      error = 'Variable name must be ASCII alphanumeric + underscore';
      return;
    }
    error = '';
    try {
      await upsertPromptVariable(activeKind, name, '');
      await refreshVariablesAfterMutation(name);
      newVariableName = '';
      editingVariableName = name;
      flashSaved(`Created variable ${name}`);
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to create variable';
    }
  }

  // ── Preview ──────────────────────────────────────────────────────────
  async function openPreview() {
    error = '';
    saveMessage = '';
    previewOpen = true;
    preview = null;
    await runPreview();
  }

  async function runPreview() {
    previewLoading = true;
    preview = null;
    try {
      // Send the current drafts (so the preview reflects unsaved edits) AND
      // the per-variable drafts. Variables not edited use their resolved body.
      const draftVariables: Record<string, string> = {};
      const list = variableList[activeKind] ?? [];
      for (const v of list) {
        const draft = variableDrafts[varKey(activeKind, v.name)];
        if (draft != null) draftVariables[v.name] = draft;
      }

      let pasted: unknown;
      if (previewMode === 'pasted') {
        try {
          pasted = JSON.parse(previewPasted);
        } catch (e) {
          preview = {
            errors: [
              `pasted JSON could not be parsed: ${e instanceof Error ? e.message : String(e)}`
            ]
          };
          previewLoading = false;
          return;
        }
      }

      preview = await previewPromptRender({
        media_kind: activeKind,
        draft_system: templateDrafts[tplKey(activeKind, 'system')],
        draft_user: templateDrafts[tplKey(activeKind, 'user')],
        draft_variables: draftVariables,
        fixture_id: previewMode === 'fixture' ? previewFixture : undefined,
        fixture_payload: pasted
      });
    } catch (e) {
      preview = { errors: [e instanceof Error ? e.message : 'Preview failed'] };
    }
    previewLoading = false;
  }

  // ── Grammars (reused unchanged) ─────────────────────────────────────
  async function reloadGrammars() {
    grammarsLoading = true;
    grammarsError = '';
    try {
      grammars = await listGrammars();
    } catch (e) {
      grammarsError = e instanceof Error ? e.message : 'Failed to load grammars';
    }
    grammarsLoading = false;
  }
  function startNewGrammar() {
    editingGrammarName = '';
    grammarEditorDraft = { name: '', gbnf: '', description: '' };
    grammarsError = '';
    grammarsMessage = '';
  }
  function startEditGrammar(g: GrammarEntry) {
    editingGrammarName = g.name;
    grammarEditorDraft = { name: g.name, gbnf: g.gbnf, description: g.description ?? '' };
    grammarsError = '';
    grammarsMessage = '';
  }
  function cancelGrammarEdit() {
    editingGrammarName = null;
    grammarEditorDraft = { name: '', gbnf: '', description: '' };
  }
  async function saveGrammar() {
    grammarsError = '';
    const name = grammarEditorDraft.name.trim();
    if (!name) {
      grammarsError = 'Name is required.';
      return;
    }
    if (name === 'off' || name === 'review') {
      grammarsError = 'Name "off" and "review" are reserved.';
      return;
    }
    if (!grammarEditorDraft.gbnf.trim()) {
      grammarsError = 'GBNF body is required.';
      return;
    }
    try {
      const payload: UpsertGrammarRequest = {
        name,
        gbnf: grammarEditorDraft.gbnf,
        description: grammarEditorDraft.description.trim() || undefined
      };
      await upsertGrammar(payload);
      grammarsMessage = `Saved grammar "${name}"`;
      editingGrammarName = null;
      grammarEditorDraft = { name: '', gbnf: '', description: '' };
      await reloadGrammars();
      setTimeout(() => (grammarsMessage = ''), 3000);
    } catch (e) {
      grammarsError = e instanceof Error ? e.message : 'Failed to save grammar';
    }
  }
  async function confirmDeleteGrammar(name: string) {
    grammarsError = '';
    try {
      await deleteGrammar(name);
      grammarsMessage = `Deleted grammar "${name}"`;
      confirmDeleteGrammarName = null;
      if (editingGrammarName === name) cancelGrammarEdit();
      await reloadGrammars();
      setTimeout(() => (grammarsMessage = ''), 3000);
    } catch (e) {
      grammarsError = e instanceof Error ? e.message : 'Failed to delete grammar';
    }
  }

  // ── Derived ──────────────────────────────────────────────────────────
  const activeSysRow = $derived(templateRows[tplKey(activeKind, 'system')]);
  const activeUserRow = $derived(templateRows[tplKey(activeKind, 'user')]);
  const activeVars = $derived(variableList[activeKind] ?? []);
</script>

<div class="page-container">
  <div class="section-header">
    <h2>Review Prompt</h2>
    <p class="section-sub">
      Prompts are templated per <code>media_kind</code>. Pick a kind below; a kind with no row falls
      back to <strong>Default</strong>; if the default itself has no row the engine uses a baked-in
      Rust constant. Variables (signal_descriptions, watch_patterns, format) layer the same way.
    </p>
  </div>

  {#if error}
    <div class="error-banner">{error}</div>
  {/if}
  {#if saveMessage}
    <div class="save-banner">{saveMessage}</div>
  {/if}

  {#if loading}
    <div class="loading-card">Loading…</div>
  {:else if overview}
    <div class="kind-strip" role="tablist">
      {#each overview.media_kinds as k (k.media_kind)}
        <button
          type="button"
          class="kind-btn"
          class:active={activeKind === k.media_kind}
          role="tab"
          aria-selected={activeKind === k.media_kind}
          onclick={() => selectKind(k.media_kind)}
        >
          <span class="kind-label">{k.label}</span>
          <span
            class="kind-badge"
            class:has-overrides={k.template_overrides + k.variable_overrides > 0}
          >
            {k.template_overrides + k.variable_overrides > 0
              ? `${k.template_overrides + k.variable_overrides} custom`
              : 'inherits'}
          </span>
        </button>
      {/each}
    </div>

    <div class="layout">
      <!-- ── Editor column ──────────────────────────────────────── -->
      <div class="editor-column">
        <div class="prompt-card">
          <div class="tab-strip" role="tablist">
            {#each TABS as tab (tab.key)}
              <button
                type="button"
                class="tab-btn"
                class:active={activeTab === tab.key}
                role="tab"
                aria-selected={activeTab === tab.key}
                onclick={() => (activeTab = tab.key)}
              >
                {tab.label}
                {#if tab.key === 'system' && activeSysRow && isTplDirty(activeKind, 'system')}
                  <span class="dirty-dot" title="unsaved"></span>
                {/if}
                {#if tab.key === 'user' && activeUserRow && isTplDirty(activeKind, 'user')}
                  <span class="dirty-dot" title="unsaved"></span>
                {/if}
              </button>
            {/each}
          </div>

          {#each TABS as tab (tab.key)}
            {#if activeTab === tab.key}
              <p class="section-desc">{tab.desc}</p>
            {/if}
          {/each}

          {#if activeTab === 'system' && activeSysRow}
            {@const dirty = isTplDirty(activeKind, 'system')}
            {@const overridden = activeSysRow.is_overridden}
            <div class="origin-row">
              <span class="origin-badge" class:overridden class:inherited={!overridden}>
                {overridden
                  ? `OVERRIDING for ${activeKind}`
                  : activeSysRow.has_db_row
                    ? 'INHERITING DB DEFAULT'
                    : 'INHERITING BAKED-IN DEFAULT'}
              </span>
            </div>
            <textarea
              class="prompt-editor"
              bind:value={templateDrafts[tplKey(activeKind, 'system')]}
              rows="22"
              spellcheck="false"></textarea>
            <div class="tab-actions">
              {#if dirty}
                <button
                  class="btn-ghost"
                  type="button"
                  onclick={() => revertTemplate(activeKind, 'system')}
                >
                  Revert
                </button>
              {/if}
              {#if confirmResetSlot === 'system'}
                <span class="confirm-text">
                  Drop {activeKind}'s system override?
                </span>
                <button
                  class="btn-danger"
                  type="button"
                  onclick={() => resetTemplate(activeKind, 'system')}
                >
                  Confirm
                </button>
                <button class="btn-ghost" type="button" onclick={() => (confirmResetSlot = null)}>
                  Cancel
                </button>
              {:else}
                <button
                  class="btn-ghost"
                  type="button"
                  onclick={() => (confirmResetSlot = 'system')}
                  disabled={!overridden}
                >
                  Reset to fallback
                </button>
              {/if}
              <button
                class="btn-primary"
                type="button"
                onclick={() => saveTemplate(activeKind, 'system')}
                disabled={!dirty}
              >
                Save
              </button>
            </div>
          {/if}

          {#if activeTab === 'user' && activeUserRow}
            {@const dirty = isTplDirty(activeKind, 'user')}
            {@const overridden = activeUserRow.is_overridden}
            <div class="origin-row">
              <span class="origin-badge" class:overridden class:inherited={!overridden}>
                {overridden
                  ? `OVERRIDING for ${activeKind}`
                  : activeUserRow.has_db_row
                    ? 'INHERITING DB DEFAULT'
                    : 'INHERITING BAKED-IN DEFAULT'}
              </span>
            </div>
            <textarea
              class="prompt-editor"
              bind:value={templateDrafts[tplKey(activeKind, 'user')]}
              rows="22"
              spellcheck="false"></textarea>
            <div class="tab-actions">
              {#if dirty}
                <button
                  class="btn-ghost"
                  type="button"
                  onclick={() => revertTemplate(activeKind, 'user')}
                >
                  Revert
                </button>
              {/if}
              {#if confirmResetSlot === 'user'}
                <span class="confirm-text">
                  Drop {activeKind}'s user override?
                </span>
                <button
                  class="btn-danger"
                  type="button"
                  onclick={() => resetTemplate(activeKind, 'user')}
                >
                  Confirm
                </button>
                <button class="btn-ghost" type="button" onclick={() => (confirmResetSlot = null)}>
                  Cancel
                </button>
              {:else}
                <button
                  class="btn-ghost"
                  type="button"
                  onclick={() => (confirmResetSlot = 'user')}
                  disabled={!overridden}
                >
                  Reset to fallback
                </button>
              {/if}
              <button
                class="btn-primary"
                type="button"
                onclick={() => saveTemplate(activeKind, 'user')}
                disabled={!dirty}
              >
                Save
              </button>
            </div>
          {/if}

          {#if activeTab === 'variables'}
            <div class="var-list">
              {#each activeVars as v (v.name)}
                {@const row = variableRows[varKey(activeKind, v.name)]}
                {@const isEditing = editingVariableName === v.name}
                <div class="var-row">
                  <div class="var-row-head">
                    <code class="var-name">{v.name}</code>
                    {#if v.is_seeded}
                      <span class="tab-badge default">Seeded</span>
                    {:else}
                      <span class="tab-badge custom">Custom</span>
                    {/if}
                    {#if v.has_per_kind_override && activeKind !== 'default'}
                      <span class="origin-badge overridden">
                        OVERRIDING for {activeKind}
                      </span>
                    {:else if v.has_default_override}
                      <span class="origin-badge inherited">DB default</span>
                    {:else}
                      <span class="origin-badge inherited">Baked-in default</span>
                    {/if}
                    {#if isVarDirty(activeKind, v.name)}
                      <span class="dirty-dot" title="unsaved"></span>
                    {/if}
                    <div class="row-actions">
                      {#if isEditing}
                        <button
                          class="btn-ghost"
                          type="button"
                          onclick={() => (editingVariableName = null)}
                        >
                          Close
                        </button>
                      {:else}
                        <button
                          class="btn-ghost"
                          type="button"
                          onclick={() => startEditVariable(v.name)}
                        >
                          Edit
                        </button>
                      {/if}
                      {#if !v.is_seeded}
                        {#if confirmDeleteVariable === v.name}
                          <button
                            class="btn-danger"
                            type="button"
                            onclick={() => deleteCustomVariable(v.name)}
                          >
                            Confirm delete
                          </button>
                          <button
                            class="btn-ghost"
                            type="button"
                            onclick={() => (confirmDeleteVariable = null)}
                          >
                            Cancel
                          </button>
                        {:else}
                          <button
                            class="btn-ghost"
                            type="button"
                            onclick={() => (confirmDeleteVariable = v.name)}
                          >
                            Delete
                          </button>
                        {/if}
                      {/if}
                    </div>
                  </div>
                  {#if isEditing && row}
                    <textarea
                      class="prompt-editor"
                      bind:value={variableDrafts[varKey(activeKind, v.name)]}
                      rows="10"
                      spellcheck="false"></textarea>
                    <div class="tab-actions">
                      {#if isVarDirty(activeKind, v.name)}
                        <button
                          class="btn-ghost"
                          type="button"
                          onclick={() => {
                            variableDrafts[varKey(activeKind, v.name)] = row.body;
                          }}
                        >
                          Revert
                        </button>
                      {/if}
                      {#if confirmResetVariable === v.name}
                        <span class="confirm-text">
                          Reset {v.name} for {activeKind} to fallback?
                        </span>
                        <button
                          class="btn-danger"
                          type="button"
                          onclick={() => resetVariable(v.name)}
                        >
                          Confirm
                        </button>
                        <button
                          class="btn-ghost"
                          type="button"
                          onclick={() => (confirmResetVariable = null)}
                        >
                          Cancel
                        </button>
                      {:else}
                        <button
                          class="btn-ghost"
                          type="button"
                          onclick={() => (confirmResetVariable = v.name)}
                          disabled={!row.is_overridden}
                        >
                          Reset to fallback
                        </button>
                      {/if}
                      <button
                        class="btn-primary"
                        type="button"
                        onclick={() => saveVariable(v.name)}
                        disabled={!isVarDirty(activeKind, v.name)}
                      >
                        Save
                      </button>
                    </div>
                  {/if}
                </div>
              {/each}
            </div>
            <div class="var-create">
              <input
                type="text"
                class="text-input"
                placeholder="new_variable_name"
                bind:value={newVariableName}
              />
              <button class="btn-primary" type="button" onclick={createNewVariable}>
                Add variable
              </button>
            </div>
          {/if}

          <div class="prompt-actions">
            <button class="btn-primary" type="button" onclick={openPreview}>
              Preview rendered output
            </button>
          </div>
        </div>
      </div>

      <!-- ── Sidebar: available context ────────────────────────── -->
      <aside class="context-card">
        <h3>Available context</h3>
        <p class="section-desc">
          Every template + variable receives the same JSON object. Reference fields with <code
            >{`{{ path }}`}</code
          >; iterate arrays with
          <code>{`{% for x in arr %}`}</code>; pipe through filters like
          <code>render_candidate_list</code>
          or
          <code>render_siblings</code>.
        </p>
        <table class="context-table">
          <tbody>
            {#each contextSchema as f (f.path)}
              <tr>
                <td><code>{f.path}</code></td>
                <td><span class="schema-kind">{f.kind}</span></td>
                <td class="schema-note">{f.note}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </aside>
    </div>
  {/if}

  <!-- ── Grammars ────────────────────────────────────────────── -->
  <div class="section-header">
    <h2>Grammars</h2>
  </div>
  <div class="prompt-card">
    <p class="section-desc">
      llama.cpp GBNF grammars constraining the model's output shape. Pick one from the Grammar
      selector in any sampling block. Two built-ins (<code>off</code> and <code>review</code>) are
      read-only.
    </p>
    {#if grammarsError}
      <div class="error-banner inline">{grammarsError}</div>
    {/if}
    {#if grammarsMessage}
      <span class="save-msg block">{grammarsMessage}</span>
    {/if}
    {#if grammarsLoading}
      <div class="loading-card">Loading grammars…</div>
    {:else}
      <div class="grammar-list">
        {#each grammars as g (g.name)}
          <div class="grammar-row">
            <div class="grammar-row-head">
              <code class="grammar-name">{g.name}</code>
              {#if g.is_builtin}
                <span class="tab-badge default">Built-in</span>
              {:else}
                <span class="tab-badge custom">Custom</span>
              {/if}
              {#if g.description}
                <span class="grammar-desc">{g.description}</span>
              {/if}
              <div class="grammar-row-actions">
                {#if g.is_builtin}
                  <button class="btn-ghost" type="button" onclick={() => startEditGrammar(g)}>
                    View
                  </button>
                {:else}
                  <button class="btn-ghost" type="button" onclick={() => startEditGrammar(g)}>
                    Edit
                  </button>
                  {#if confirmDeleteGrammarName === g.name}
                    <button
                      class="btn-danger"
                      type="button"
                      onclick={() => confirmDeleteGrammar(g.name)}
                    >
                      Confirm delete
                    </button>
                    <button
                      class="btn-ghost"
                      type="button"
                      onclick={() => (confirmDeleteGrammarName = null)}
                    >
                      Cancel
                    </button>
                  {:else}
                    <button
                      class="btn-ghost"
                      type="button"
                      onclick={() => (confirmDeleteGrammarName = g.name)}
                    >
                      Delete
                    </button>
                  {/if}
                {/if}
              </div>
            </div>
          </div>
        {/each}
      </div>
      <div class="grammar-actions">
        {#if editingGrammarName === null}
          <button class="btn-primary" type="button" onclick={startNewGrammar}> Add grammar </button>
        {/if}
      </div>
      {#if editingGrammarName !== null}
        {@const editingExisting = grammars.find((g) => g.name === editingGrammarName)}
        {@const readOnly = editingExisting?.is_builtin === true}
        <div class="grammar-editor">
          <h3>
            {editingGrammarName === ''
              ? 'New grammar'
              : readOnly
                ? `Built-in: ${editingGrammarName}`
                : `Edit: ${editingGrammarName}`}
          </h3>
          <label class="editor-field">
            <span class="knob-label">Name</span>
            <input
              type="text"
              class="text-input"
              bind:value={grammarEditorDraft.name}
              disabled={editingGrammarName !== '' || readOnly}
              placeholder="my_grammar"
            />
          </label>
          <label class="editor-field">
            <span class="knob-label">Description (optional)</span>
            <input
              type="text"
              class="text-input"
              bind:value={grammarEditorDraft.description}
              disabled={readOnly}
              placeholder="What this grammar constrains"
            />
          </label>
          <label class="editor-field">
            <span class="knob-label">GBNF</span>
            <textarea
              class="prompt-editor"
              bind:value={grammarEditorDraft.gbnf}
              rows="14"
              spellcheck="false"
              readonly={readOnly}></textarea>
          </label>
          <div class="grammar-editor-actions">
            <button class="btn-ghost" type="button" onclick={cancelGrammarEdit}>
              {readOnly ? 'Close' : 'Cancel'}
            </button>
            {#if !readOnly}
              <button class="btn-primary" type="button" onclick={saveGrammar}>
                Save grammar
              </button>
            {/if}
          </div>
        </div>
      {/if}
    {/if}
  </div>

  <!-- ── Preview modal ───────────────────────────────────────── -->
  {#if previewOpen}
    <div
      class="modal-backdrop"
      onclick={() => (previewOpen = false)}
      onkeydown={(e) => e.key === 'Escape' && (previewOpen = false)}
      role="presentation"
    >
      <div
        class="modal"
        role="dialog"
        aria-modal="true"
        aria-label="Preview"
        tabindex="-1"
        onclick={(e) => e.stopPropagation()}
        onkeydown={(e) => e.stopPropagation()}
      >
        <div class="modal-header">
          <h3>Preview — {activeKind}</h3>
          <button class="modal-close" type="button" onclick={() => (previewOpen = false)}>
            ×
          </button>
        </div>
        <p class="section-desc">
          Renders your draft templates + variables against a sample ReviewRequest. Using current
          unsaved drafts.
        </p>

        <div class="preview-controls">
          <label class="editor-field">
            <span class="knob-label">Sample input</span>
            <div class="radio-row">
              <label>
                <input type="radio" name="previewMode" value="fixture" bind:group={previewMode} />
                Shipped fixture
              </label>
              <label>
                <input type="radio" name="previewMode" value="pasted" bind:group={previewMode} />
                Paste JSON
              </label>
            </div>
          </label>
          {#if previewMode === 'fixture'}
            <label class="editor-field">
              <span class="knob-label">Fixture</span>
              <select class="text-input" bind:value={previewFixture}>
                {#each fixtures as f (f.id)}
                  <option value={f.id}>{f.label}</option>
                {/each}
              </select>
            </label>
          {:else}
            <label class="editor-field">
              <span class="knob-label">ReviewRequest JSON</span>
              <textarea
                class="prompt-editor"
                rows="8"
                spellcheck="false"
                bind:value={previewPasted}
                placeholder={'{\n  "wanted_title": "...",\n  "want": { "kind": "tv_episode", ... },\n  "candidates": [ ... ]\n}'}
              ></textarea>
            </label>
          {/if}
          <div class="modal-actions">
            <button class="btn-primary" type="button" onclick={runPreview}>
              {previewLoading ? 'Rendering…' : 'Render preview'}
            </button>
          </div>
        </div>

        {#if preview}
          {#if preview.errors.length > 0}
            <div class="error-banner inline">
              {#each preview.errors as err, i (i)}
                <div>{err}</div>
              {/each}
            </div>
          {/if}
          {#if preview.rendered_system}
            <div class="preview-section">
              <div class="preview-label">SYSTEM MESSAGE</div>
              <JsonBlock content={preview.rendered_system} label="System" maxHeight="400px" />
            </div>
          {/if}
          {#if preview.rendered_user}
            <div class="preview-section">
              <div class="preview-label">USER MESSAGE</div>
              <JsonBlock content={preview.rendered_user} label="User" maxHeight="400px" />
            </div>
          {/if}
        {/if}
      </div>
    </div>
  {/if}
</div>

<style>
  .prompt-card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 24px;
    margin-bottom: 16px;
  }
  .prompt-card h3 {
    margin: 0 0 4px;
    font-size: 16px;
  }
  .section-sub {
    margin: 4px 0 12px;
    color: var(--muted);
    font-size: 13px;
  }
  .section-desc {
    margin: 0 0 16px;
    font-size: 13px;
    color: var(--muted);
  }
  .save-banner {
    padding: 8px 12px;
    background: rgba(34, 197, 94, 0.1);
    border: 1px solid rgba(34, 197, 94, 0.3);
    border-radius: 10px;
    color: var(--success);
    font-size: 13px;
    margin-bottom: 16px;
    max-width: 1100px;
  }

  .kind-strip {
    display: flex;
    gap: 8px;
    margin-bottom: 16px;
    flex-wrap: wrap;
  }
  .kind-btn {
    padding: 10px 14px;
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 12px;
    color: var(--muted);
    cursor: pointer;
    font-size: 13px;
    display: inline-flex;
    align-items: center;
    gap: 8px;
    transition: all 0.15s;
  }
  .kind-btn:hover {
    color: var(--ink);
  }
  .kind-btn.active {
    color: var(--ink);
    border-color: var(--accent);
    background: var(--accent-dim);
  }
  .kind-label {
    font-weight: 600;
  }
  .kind-badge {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 2px 6px;
    border-radius: 6px;
    background: rgba(255, 255, 255, 0.06);
    color: var(--muted);
  }
  .kind-badge.has-overrides {
    background: var(--accent-dim);
    color: var(--accent);
  }

  .layout {
    display: grid;
    grid-template-columns: minmax(0, 2.4fr) minmax(0, 1fr);
    gap: 16px;
    align-items: start;
  }
  @media (max-width: 1080px) {
    .layout {
      grid-template-columns: 1fr;
    }
  }

  .tab-strip {
    display: flex;
    gap: 4px;
    margin-bottom: 16px;
    border-bottom: 1px solid var(--ring);
    flex-wrap: wrap;
  }
  .tab-btn {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 10px 14px;
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--muted);
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }
  .tab-btn:hover {
    color: var(--ink);
  }
  .tab-btn.active {
    color: var(--ink);
    border-bottom-color: var(--accent);
  }
  .tab-badge {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 2px 8px;
    border-radius: 6px;
  }
  .tab-badge.default {
    background: rgba(34, 197, 94, 0.15);
    color: var(--success);
  }
  .tab-badge.custom {
    background: var(--accent-dim);
    color: var(--accent);
  }
  .dirty-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--warning);
    display: inline-block;
  }

  .origin-row {
    margin-bottom: 8px;
  }
  .origin-badge {
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    padding: 3px 8px;
    border-radius: 6px;
  }
  .origin-badge.overridden {
    background: var(--accent-dim);
    color: var(--accent);
  }
  .origin-badge.inherited {
    background: rgba(255, 255, 255, 0.06);
    color: var(--muted);
  }

  .prompt-editor {
    width: 100%;
    padding: 16px;
    border: 1px solid var(--ring);
    border-radius: 12px;
    background: rgba(0, 0, 0, 0.2);
    color: var(--ink);
    font-family: 'JetBrains Mono', 'Fira Code', monospace;
    font-size: 12px;
    line-height: 1.6;
    resize: vertical;
    tab-size: 2;
  }
  :global([data-theme='light']) .prompt-editor {
    background: rgba(0, 0, 0, 0.04);
  }

  .tab-actions {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: 6px;
    margin-top: 8px;
    min-height: 24px;
    flex-wrap: wrap;
  }
  .btn-ghost {
    background: transparent;
    border: none;
    color: var(--muted);
    font-size: 12px;
    cursor: pointer;
    padding: 4px 8px;
  }
  .btn-ghost:hover {
    color: var(--ink);
  }
  .btn-primary {
    padding: 8px 16px;
    border: 1px solid var(--accent);
    border-radius: 10px;
    background: var(--accent-dim);
    color: var(--accent);
    cursor: pointer;
    font-size: 13px;
    font-weight: 600;
  }
  .btn-primary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .prompt-actions {
    display: flex;
    gap: 8px;
    align-items: center;
    justify-content: flex-end;
    margin-top: 16px;
  }

  .var-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 16px;
  }
  .var-row {
    padding: 12px 14px;
    background: rgba(255, 255, 255, 0.03);
    border: 1px solid var(--ring);
    border-radius: 10px;
  }
  :global([data-theme='light']) .var-row {
    background: rgba(0, 0, 0, 0.02);
  }
  .var-row-head {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .var-name {
    font-size: 13px;
    font-weight: 600;
    color: var(--accent);
  }
  .row-actions {
    display: flex;
    gap: 4px;
    margin-left: auto;
  }
  .var-row .prompt-editor {
    margin-top: 8px;
  }

  .var-create {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-top: 8px;
  }
  .var-create .text-input {
    flex: 1;
  }

  .text-input {
    padding: 8px 10px;
    border: 1px solid var(--ring);
    border-radius: 8px;
    background: transparent;
    color: var(--ink);
    font-size: 13px;
  }

  .context-card {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 16px 20px;
    position: sticky;
    top: 16px;
  }
  .context-card h3 {
    margin: 0 0 4px;
    font-size: 14px;
  }
  .context-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }
  .context-table td {
    padding: 4px 6px;
    vertical-align: top;
    border-top: 1px solid rgba(255, 255, 255, 0.04);
  }
  :global([data-theme='light']) .context-table td {
    border-top: 1px solid rgba(0, 0, 0, 0.04);
  }
  .context-table code {
    font-size: 11px;
    color: var(--accent);
  }
  .schema-kind {
    font-size: 11px;
    color: var(--muted);
    font-style: italic;
  }
  .schema-note {
    color: var(--muted);
  }

  .confirm-text {
    font-size: 13px;
    color: var(--warning);
  }
  .btn-danger {
    padding: 8px 16px;
    border: 1px solid var(--danger);
    border-radius: 10px;
    background: rgba(239, 68, 68, 0.15);
    color: var(--danger);
    cursor: pointer;
    font-size: 13px;
    font-weight: 600;
  }

  .error-banner {
    padding: 12px 16px;
    background: rgba(239, 68, 68, 0.1);
    border: 1px solid rgba(239, 68, 68, 0.3);
    border-radius: 12px;
    color: var(--danger);
    font-size: 13px;
    margin-bottom: 16px;
    max-width: 1100px;
  }
  .error-banner.inline {
    margin-top: 8px;
    margin-bottom: 12px;
  }
  .save-msg {
    font-size: 13px;
    color: var(--success);
    margin-right: auto;
  }
  .save-msg.block {
    display: block;
    margin: 0 0 12px;
  }

  .grammar-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 16px;
  }
  .grammar-row {
    padding: 12px 14px;
    background: rgba(255, 255, 255, 0.03);
    border: 1px solid var(--ring);
    border-radius: 10px;
  }
  :global([data-theme='light']) .grammar-row {
    background: rgba(0, 0, 0, 0.02);
  }
  .grammar-row-head {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .grammar-name {
    font-size: 13px;
    font-weight: 600;
    color: var(--accent);
  }
  .grammar-desc {
    font-size: 12px;
    color: var(--muted);
    flex: 1;
    min-width: 0;
  }
  .grammar-row-actions {
    display: flex;
    gap: 4px;
    margin-left: auto;
  }
  .grammar-actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    margin-bottom: 16px;
  }
  .grammar-editor {
    border: 1px solid var(--ring);
    border-radius: 12px;
    padding: 16px;
    background: rgba(0, 0, 0, 0.04);
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  :global([data-theme='light']) .grammar-editor {
    background: rgba(0, 0, 0, 0.02);
  }
  .grammar-editor h3 {
    margin: 0;
    font-size: 14px;
  }
  .editor-field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .knob-label {
    font-size: 11px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.3px;
    font-weight: 600;
  }
  .grammar-editor-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
    padding: 20px;
  }
  .modal {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 24px;
    max-width: 1000px;
    width: 100%;
    max-height: 90vh;
    overflow-y: auto;
  }
  .modal-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 8px;
  }
  .modal-header h3 {
    margin: 0;
    font-size: 16px;
  }
  .modal-close {
    background: transparent;
    border: none;
    color: var(--muted);
    font-size: 24px;
    line-height: 1;
    cursor: pointer;
    padding: 0 8px;
  }

  .preview-controls {
    display: flex;
    flex-direction: column;
    gap: 12px;
    margin-bottom: 16px;
  }
  .radio-row {
    display: flex;
    gap: 16px;
    align-items: center;
    font-size: 13px;
  }
  .preview-section {
    margin-top: 16px;
  }
  .preview-label {
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--accent);
    margin-bottom: 6px;
  }
  .modal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 8px;
  }
  .loading-card {
    padding: 16px;
    color: var(--muted);
    font-size: 13px;
  }
</style>
