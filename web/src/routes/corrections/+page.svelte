<script lang="ts">
  import { onMount } from 'svelte';
  import { DeleteConfirmModal } from '@koblibs/ui';
  import {
    getCorrections,
    addGenericRule,
    deleteGenericRule,
    addEntityRule,
    deleteEntityRule,
    addDecisionCorrection,
    deleteDecisionCorrection
  } from '$lib/api/client';
  import type { GenericRule, EntityRule, DecisionCorrection } from '$lib/types';

  let genericRules = $state<GenericRule[]>([]);
  let entityRules = $state<EntityRule[]>([]);
  let decisionCorrections = $state<DecisionCorrection[]>([]);
  let loading = $state(true);
  let error = $state('');

  // Generic rule form
  let newGenericScope = $state('global');
  let newGenericRule = $state('');

  // Entity rule form
  let newEntityId = $state('');
  let newEntityType = $state('tv_show');
  let newEntityName = $state('');
  let newEntityRule = $state('');

  // Decision correction form
  let newReleaseName = $state('');
  let newEntityIdForDecision = $state('');
  let newModelDecision = $state('');
  let newCorrectDecision = $state('');
  let newReason = $state('');

  // Delete confirmation
  let deleteTarget = $state<{ type: string; id: string; name: string } | null>(null);
  let deleting = $state(false);

  onMount(loadCorrections);

  async function loadCorrections() {
    loading = true;
    error = '';
    try {
      const data = await getCorrections();
      genericRules = data.generic_rules;
      entityRules = data.entity_rules;
      decisionCorrections = data.decision_corrections;
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to load corrections';
    }
    loading = false;
  }

  async function handleAddGenericRule() {
    if (!newGenericRule.trim()) return;
    try {
      const rule = await addGenericRule({ scope: newGenericScope, rule: newGenericRule });
      genericRules = [...genericRules, rule];
      newGenericRule = '';
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to add rule';
    }
  }

  async function handleAddEntityRule() {
    if (!newEntityId.trim() || !newEntityRule.trim()) return;
    try {
      const rule = await addEntityRule({
        entity_id: newEntityId,
        entity_type: newEntityType,
        entity_name: newEntityName || undefined,
        rule: newEntityRule
      });
      entityRules = [...entityRules, rule];
      newEntityId = '';
      newEntityName = '';
      newEntityRule = '';
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to add rule';
    }
  }

  async function handleAddDecisionCorrection() {
    if (!newReleaseName.trim() || !newCorrectDecision.trim()) return;
    try {
      const correction = await addDecisionCorrection({
        release_name: newReleaseName,
        entity_id: newEntityIdForDecision || undefined,
        model_decision: newModelDecision,
        correct_decision: newCorrectDecision,
        reason: newReason
      });
      decisionCorrections = [...decisionCorrections, correction];
      newReleaseName = '';
      newEntityIdForDecision = '';
      newModelDecision = '';
      newCorrectDecision = '';
      newReason = '';
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to add correction';
    }
  }

  async function handleDeleteConfirm() {
    if (!deleteTarget) return;
    deleting = true;
    try {
      if (deleteTarget.type === 'generic') {
        await deleteGenericRule(deleteTarget.id);
        genericRules = genericRules.filter((r) => r.id !== deleteTarget!.id);
      } else if (deleteTarget.type === 'entity') {
        await deleteEntityRule(deleteTarget.id);
        entityRules = entityRules.filter((r) => r.id !== deleteTarget!.id);
      } else if (deleteTarget.type === 'decision') {
        await deleteDecisionCorrection(deleteTarget.id);
        decisionCorrections = decisionCorrections.filter((c) => c.id !== deleteTarget!.id);
      }
      deleteTarget = null;
    } catch (e) {
      error = e instanceof Error ? e.message : 'Failed to delete';
    }
    deleting = false;
  }
</script>

<div class="page-container">
  <div class="section-header">
    <h2>Corrections</h2>
  </div>

  {#if error}
    <div class="error-banner">{error}</div>
  {/if}

  {#if loading}
    <div class="loading-card">Loading corrections...</div>
  {:else}
    <!-- Generic Rules -->
    <section class="correction-section">
      <h3>Generic Rules</h3>
      <p class="section-desc">Rules that apply broadly across all reviews.</p>

      {#if genericRules.length > 0}
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Scope</th>
                <th>Rule</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {#each genericRules as rule (rule.id)}
                <tr>
                  <td><span class="status-chip">{rule.scope}</span></td>
                  <td class="rule-cell">{rule.rule}</td>
                  <td>
                    <button
                      class="icon-btn small"
                      type="button"
                      title="Delete"
                      onclick={() =>
                        (deleteTarget = {
                          type: 'generic',
                          id: rule.id,
                          name: rule.rule.slice(0, 40)
                        })}
                    >
                      <span class="material-icons-outlined">delete</span>
                    </button>
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {:else}
        <div class="empty-state">
          <p>No generic rules yet.</p>
        </div>
      {/if}

      <div class="add-form">
        <div class="form-row">
          <label class="form-group" style="min-width: 120px;">
            <span class="form-label">Scope</span>
            <select class="form-select" bind:value={newGenericScope}>
              <option value="global">Global</option>
              <option value="quality">Quality</option>
              <option value="naming">Naming</option>
              <option value="release_group">Release Group</option>
            </select>
          </label>
          <label class="form-group" style="flex: 1;">
            <span class="form-label">Rule</span>
            <textarea
              class="form-textarea"
              bind:value={newGenericRule}
              rows="2"
              placeholder="Describe the rule..."></textarea>
          </label>
        </div>
        <div class="form-actions">
          <button
            class="btn-primary"
            type="button"
            onclick={handleAddGenericRule}
            disabled={!newGenericRule.trim()}>Add Rule</button
          >
        </div>
      </div>
    </section>

    <!-- Entity Rules -->
    <section class="correction-section">
      <h3>Entity Rules</h3>
      <p class="section-desc">Rules specific to a particular show, movie, or entity.</p>

      {#if entityRules.length > 0}
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Entity</th>
                <th>Type</th>
                <th>Rule</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {#each entityRules as rule (rule.id)}
                <tr>
                  <td>{rule.entity_name || rule.entity_id}</td>
                  <td><span class="status-chip">{rule.entity_type}</span></td>
                  <td class="rule-cell">{rule.rule}</td>
                  <td>
                    <button
                      class="icon-btn small"
                      type="button"
                      title="Delete"
                      onclick={() =>
                        (deleteTarget = {
                          type: 'entity',
                          id: rule.id,
                          name: rule.entity_name || rule.entity_id
                        })}
                    >
                      <span class="material-icons-outlined">delete</span>
                    </button>
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {:else}
        <div class="empty-state">
          <p>No entity rules yet.</p>
        </div>
      {/if}

      <div class="add-form">
        <div class="form-grid">
          <label>
            Entity ID
            <input type="text" bind:value={newEntityId} placeholder="e.g. tmdb:12345" />
          </label>
          <label>
            Entity Type
            <select bind:value={newEntityType}>
              <option value="tv_show">TV Show</option>
              <option value="movie">Movie</option>
              <option value="anime">Anime</option>
            </select>
          </label>
          <label>
            Entity Name (optional)
            <input type="text" bind:value={newEntityName} placeholder="e.g. Breaking Bad" />
          </label>
        </div>
        <label class="form-group" style="margin-top: 12px;">
          <span class="form-label">Rule</span>
          <textarea
            class="form-textarea"
            bind:value={newEntityRule}
            rows="2"
            placeholder="Describe the rule for this entity..."></textarea>
        </label>
        <div class="form-actions">
          <button
            class="btn-primary"
            type="button"
            onclick={handleAddEntityRule}
            disabled={!newEntityId.trim() || !newEntityRule.trim()}>Add Rule</button
          >
        </div>
      </div>
    </section>

    <!-- Decision Corrections -->
    <section class="correction-section">
      <h3>Decision Corrections</h3>
      <p class="section-desc">
        Override specific model decisions to teach it the correct behavior.
      </p>

      {#if decisionCorrections.length > 0}
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Release Name</th>
                <th>Model Said</th>
                <th>Correct</th>
                <th>Reason</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {#each decisionCorrections as correction (correction.id)}
                <tr>
                  <td class="release-cell">{correction.release_name}</td>
                  <td><span class="status-chip">{correction.model_decision}</span></td>
                  <td><span class="status-chip correct">{correction.correct_decision}</span></td>
                  <td class="rule-cell">{correction.reason}</td>
                  <td>
                    <button
                      class="icon-btn small"
                      type="button"
                      title="Delete"
                      onclick={() =>
                        (deleteTarget = {
                          type: 'decision',
                          id: correction.id,
                          name: correction.release_name.slice(0, 40)
                        })}
                    >
                      <span class="material-icons-outlined">delete</span>
                    </button>
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {:else}
        <div class="empty-state">
          <p>No decision corrections yet.</p>
        </div>
      {/if}

      <div class="add-form">
        <div class="form-grid">
          <label>
            Release Name
            <input
              type="text"
              bind:value={newReleaseName}
              placeholder="e.g. Show.S01E01.720p.WEB..."
            />
          </label>
          <label>
            Entity ID (optional)
            <input type="text" bind:value={newEntityIdForDecision} placeholder="e.g. tmdb:12345" />
          </label>
          <label>
            Model Decision
            <select bind:value={newModelDecision}>
              <option value="">Select...</option>
              <option value="accept">Accept</option>
              <option value="reject">Reject</option>
              <option value="veto">Veto</option>
              <option value="boost">Boost</option>
            </select>
          </label>
          <label>
            Correct Decision
            <select bind:value={newCorrectDecision}>
              <option value="">Select...</option>
              <option value="accept">Accept</option>
              <option value="reject">Reject</option>
              <option value="veto">Veto</option>
              <option value="boost">Boost</option>
            </select>
          </label>
        </div>
        <label class="form-group" style="margin-top: 12px;">
          <span class="form-label">Reason</span>
          <textarea
            class="form-textarea"
            bind:value={newReason}
            rows="2"
            placeholder="Why is the model wrong?"></textarea>
        </label>
        <div class="form-actions">
          <button
            class="btn-primary"
            type="button"
            onclick={handleAddDecisionCorrection}
            disabled={!newReleaseName.trim() || !newCorrectDecision}>Add Correction</button
          >
        </div>
      </div>
    </section>
  {/if}
</div>

{#if deleteTarget}
  <DeleteConfirmModal
    title="Delete {deleteTarget.type === 'generic'
      ? 'Rule'
      : deleteTarget.type === 'entity'
        ? 'Entity Rule'
        : 'Correction'}"
    message="Delete '{deleteTarget.name}'? This cannot be undone."
    loading={deleting}
    onConfirm={handleDeleteConfirm}
    onCancel={() => (deleteTarget = null)}
  />
{/if}

<style>
  .correction-section {
    background: var(--card);
    border: 1px solid var(--ring);
    border-radius: 16px;
    padding: 24px;
    margin-bottom: 20px;
  }

  .correction-section h3 {
    margin: 0 0 4px;
    font-size: 16px;
  }

  .section-desc {
    margin: 0 0 16px;
    font-size: 13px;
    color: var(--muted);
  }

  .table-wrap {
    overflow-x: auto;
    margin-bottom: 16px;
  }

  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }

  th {
    text-align: left;
    padding: 8px 12px;
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    color: var(--muted);
    border-bottom: 1px solid var(--ring);
    font-weight: 600;
  }

  td {
    padding: 10px 12px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.04);
    vertical-align: top;
  }

  :global([data-theme='light']) td {
    border-bottom: 1px solid rgba(0, 0, 0, 0.04);
  }

  .rule-cell {
    max-width: 400px;
    word-break: break-word;
  }

  .release-cell {
    max-width: 300px;
    word-break: break-all;
    font-family: monospace;
    font-size: 12px;
  }

  .status-chip.correct {
    background: rgba(34, 197, 94, 0.15);
    border-color: rgba(34, 197, 94, 0.3);
    color: var(--success);
  }

  .add-form {
    padding-top: 16px;
    border-top: 1px solid var(--ring);
  }

  .error-banner {
    padding: 12px 16px;
    background: rgba(239, 68, 68, 0.1);
    border: 1px solid rgba(239, 68, 68, 0.3);
    border-radius: 12px;
    color: var(--danger);
    font-size: 13px;
    margin-bottom: 16px;
  }
</style>
