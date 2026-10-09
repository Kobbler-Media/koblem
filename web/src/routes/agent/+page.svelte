<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { DeleteConfirmModal } from '@koblibs/ui';
  import Composer from '$lib/components/agent/Composer.svelte';
  import Lightbox from '$lib/components/agent/Lightbox.svelte';
  import SessionTabs from '$lib/components/agent/SessionTabs.svelte';
  import Transcript from '$lib/components/agent/Transcript.svelte';
  import type { PendingAttachment } from '$lib/components/agent/attachments';
  import { POLL_MS, outOfRetries, retryDelay } from '$lib/components/agent/poll';
  import {
    ApiError,
    cancelAgentTurn,
    createAgentSession,
    deleteAgentSession,
    getAgentSession,
    listAgentSessions,
    sendAgentMessage,
    type AgentAttachmentUpload,
    type AgentSession,
    type AgentSessionSummary
  } from '$lib/api/client';

  const ACTIVE_KEY = 'koblem-agent-session';

  let sessions = $state<AgentSessionSummary[]>([]);
  let activeId = $state<string | null>(null);
  let session = $state<AgentSession | null>(null);
  let loading = $state(false);
  let creating = $state(false);
  let sending = $state(false);
  let cancelling = $state(false);
  let deleting = $state(false);
  let reconnecting = $state(false);
  // The session the server has accepted a turn for but whose transcript has not
  // come back yet. Without it a failed refresh leaves `running` reading false
  // off the stale transcript, which re-arms Send for a duplicate turn.
  let pendingTurnFor = $state<string | null>(null);
  let error = $state('');
  let deleteTarget = $state<AgentSessionSummary | null>(null);
  let lightboxSrc = $state<string | null>(null);

  let pollTimer: ReturnType<typeof setTimeout> | null = null;
  let loadFails = 0;
  // Bumped on every session switch/reload so a poll or fetch that lands late
  // cannot overwrite the session the user has since moved to.
  let loadSeq = 0;
  let turnWasRunning = false;

  const messages = $derived(session?.messages ?? []);
  const running = $derived(
    (activeId !== null && pendingTurnFor === activeId) ||
      messages.some((m) => m.status === 'running')
  );

  function msg(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }

  // Oldest first, so a tab never moves out from under the pointer when the
  // server touches `updated_at`.
  function sorted(list: AgentSessionSummary[]): AgentSessionSummary[] {
    return [...list].sort((a, b) => a.created_at.localeCompare(b.created_at));
  }

  function persistActive(id: string | null) {
    if (typeof localStorage === 'undefined') return;
    if (id) localStorage.setItem(ACTIVE_KEY, id);
    else localStorage.removeItem(ACTIVE_KEY);
  }

  function stopPoll() {
    if (pollTimer) {
      clearTimeout(pollTimer);
      pollTimer = null;
    }
  }

  async function refreshSessions() {
    try {
      sessions = sorted(await listAgentSessions());
    } catch (e) {
      error = msg(e);
    }
  }

  function apply(s: AgentSession, id: string, seq: number) {
    session = s;
    loadFails = 0;
    reconnecting = false;
    if (pendingTurnFor === id) pendingTurnFor = null;
    if (s.messages.some((m) => m.status === 'running')) {
      turnWasRunning = true;
      stopPoll();
      pollTimer = setTimeout(() => void load(id, seq), POLL_MS);
      return;
    }
    stopPoll();
    // A finished turn is when the server has a real title to show.
    if (turnWasRunning) {
      turnWasRunning = false;
      void refreshSessions();
    }
  }

  function forget(id: string) {
    sessions = sessions.filter((s) => s.id !== id);
    if (activeId === id) {
      activeId = null;
      session = null;
      pendingTurnFor = null;
      persistActive(null);
    }
  }

  // The initial load and every poll go through here, so a transient failure on
  // either arms the same retry chain instead of latching the page.
  async function load(id: string, seq: number) {
    if (seq !== loadSeq) return;
    try {
      const s = await getAgentSession(id);
      if (seq !== loadSeq) return;
      apply(s, id, seq);
    } catch (e) {
      if (seq !== loadSeq) return;
      // A stored id that no longer exists would otherwise wedge the page.
      if (e instanceof ApiError && e.status === 404) {
        stopPoll();
        reconnecting = false;
        error = msg(e);
        forget(id);
        return;
      }
      loadFails += 1;
      if (outOfRetries(loadFails)) {
        stopPoll();
        reconnecting = false;
        pendingTurnFor = null;
        error = `Lost track of this chat (${msg(e)}). Reload to pick it back up.`;
        return;
      }
      reconnecting = true;
      stopPoll();
      pollTimer = setTimeout(() => void load(id, seq), retryDelay(loadFails));
    } finally {
      if (seq === loadSeq) loading = false;
    }
  }

  async function openSession(id: string) {
    stopPoll();
    const seq = ++loadSeq;
    loadFails = 0;
    reconnecting = false;
    loading = true;
    await load(id, seq);
  }

  function adopt(created: { id: string; title: string; created_at: string }): string {
    stopPoll();
    loadSeq += 1;
    sessions = sorted([
      ...sessions,
      {
        id: created.id,
        title: created.title,
        created_at: created.created_at,
        updated_at: created.created_at
      }
    ]);
    activeId = created.id;
    persistActive(created.id);
    session = { id: created.id, title: created.title, messages: [] };
    return created.id;
  }

  async function selectSession(id: string) {
    if (id === activeId) return;
    stopPoll();
    activeId = id;
    session = null;
    error = '';
    persistActive(id);
    await openSession(id);
  }

  async function newSession() {
    creating = true;
    error = '';
    try {
      adopt(await createAgentSession());
    } catch (e) {
      error = msg(e);
    } finally {
      creating = false;
    }
  }

  async function send(content: string, attachments: PendingAttachment[]) {
    error = '';
    sending = true;
    try {
      const id = activeId ?? adopt(await createAgentSession());
      const payload: AgentAttachmentUpload[] = attachments.map((a) => ({
        kind: a.kind,
        data_url: a.dataUrl,
        mime: a.mime
      }));
      await sendAgentMessage(id, content, payload);
      pendingTurnFor = id;
      turnWasRunning = true;
      await openSession(id);
    } catch (e) {
      error = msg(e);
      // A rejected duplicate (409, the server serialises turns per session) and
      // a send that failed on the wire after landing both mean the transcript
      // on screen is behind. Hold the composer shut and go ask; the refresh
      // releases the hold either way.
      if (activeId) {
        pendingTurnFor = activeId;
        void openSession(activeId);
      }
      throw e;
    } finally {
      sending = false;
    }
  }

  async function cancel() {
    const id = activeId;
    if (!id) return;
    cancelling = true;
    try {
      await cancelAgentTurn(id);
      await openSession(id);
    } catch (e) {
      error = msg(e);
    } finally {
      cancelling = false;
    }
  }

  async function confirmDelete() {
    const target = deleteTarget;
    if (!target) return;
    deleting = true;
    try {
      await deleteAgentSession(target.id);
      const remaining = sessions.filter((s) => s.id !== target.id);
      sessions = remaining;
      deleteTarget = null;
      if (pendingTurnFor === target.id) pendingTurnFor = null;
      if (activeId === target.id) {
        stopPoll();
        loadSeq += 1;
        session = null;
        const next = remaining[remaining.length - 1] ?? null;
        activeId = next?.id ?? null;
        persistActive(activeId);
        if (next) await openSession(next.id);
      }
    } catch (e) {
      error = msg(e);
    } finally {
      deleting = false;
    }
  }

  onMount(async () => {
    loading = true;
    try {
      const list = sorted(await listAgentSessions());
      sessions = list;
      const stored = localStorage.getItem(ACTIVE_KEY);
      const pick = list.find((s) => s.id === stored) ?? list[list.length - 1] ?? null;
      if (pick) {
        activeId = pick.id;
        persistActive(pick.id);
        await openSession(pick.id);
      }
    } catch (e) {
      error = msg(e);
    } finally {
      loading = false;
    }
  });

  onDestroy(() => {
    loadSeq += 1;
    stopPoll();
  });
</script>

<div class="page-container agent-page">
  <div class="section-header">
    <h2>Agent</h2>
    <p class="muted">
      Describe what you want; the model picks the engine, runs it, and reports back here.
    </p>
  </div>

  <SessionTabs
    {sessions}
    {activeId}
    {creating}
    onSelect={selectSession}
    onCreate={newSession}
    onDelete={(s) => (deleteTarget = s)}
  />

  {#if error}
    <div class="error-banner">
      <span>{error}</span>
      <button class="dismiss" type="button" aria-label="Dismiss error" onclick={() => (error = '')}
        >×</button
      >
    </div>
  {/if}

  {#if reconnecting}
    <div class="reconnect-banner">
      <span class="spinner"></span>
      <span>Lost the connection — retrying{running ? '; your turn is still running' : ''}…</span>
    </div>
  {/if}

  <Transcript
    {messages}
    loading={(loading || reconnecting) && messages.length === 0}
    onOpenImage={(src) => (lightboxSrc = src)}
  />

  <div class="composer-area">
    <Composer {running} {sending} {cancelling} onSend={send} onCancel={cancel} />
  </div>
</div>

{#if lightboxSrc}
  <Lightbox src={lightboxSrc} alt="Full size" onClose={() => (lightboxSrc = null)} />
{/if}

{#if deleteTarget}
  <DeleteConfirmModal
    title="Delete chat"
    message={`Delete “${deleteTarget.title?.trim() || 'Untitled'}” and everything in it?`}
    loading={deleting}
    onConfirm={confirmDelete}
    onCancel={() => (deleteTarget = null)}
  />
{/if}

<style>
  .agent-page {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    padding-bottom: 20px;
  }

  .section-header {
    align-items: baseline;
  }

  .section-header .muted {
    margin: 0;
    font-size: 13px;
  }

  .composer-area {
    flex-shrink: 0;
    padding-top: 12px;
    max-width: 1100px;
    width: 100%;
  }

  .error-banner {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    padding: 12px 16px;
    background: rgba(239, 68, 68, 0.1);
    border: 1px solid rgba(239, 68, 68, 0.3);
    border-radius: 12px;
    color: var(--danger);
    font-size: 13px;
    margin-bottom: 12px;
    max-width: 1100px;
  }

  .error-banner span {
    flex: 1;
  }

  .reconnect-banner {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 16px;
    background: rgba(234, 179, 8, 0.1);
    border: 1px solid rgba(234, 179, 8, 0.3);
    border-radius: 12px;
    color: var(--warning);
    font-size: 13px;
    margin-bottom: 12px;
    max-width: 1100px;
  }

  .spinner {
    display: inline-block;
    width: 13px;
    height: 13px;
    border: 2px solid currentColor;
    border-right-color: transparent;
    border-radius: 50%;
    animation: spin 0.75s linear infinite;
    flex-shrink: 0;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .dismiss {
    background: transparent;
    border: none;
    color: inherit;
    font-size: 18px;
    line-height: 1;
    cursor: pointer;
    padding: 0;
  }
</style>
